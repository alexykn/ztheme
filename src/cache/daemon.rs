use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};

use super::{CacheKey, Entry, MAX_ENTRIES, disk, now_epoch_seconds, wire};
use crate::gitstatus;

const IDLE_TIMEOUT: Duration = Duration::from_hours(1);
const GITSTATUS_TIMEOUT: Duration = Duration::from_secs(30);
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
const SAVE_RETRY: Duration = Duration::from_secs(30);
const LAST_USED_SAVE_INTERVAL: u64 = 5 * 60;
const LOCK_EXCLUSIVE: i32 = 2;
const LOCK_NONBLOCKING: i32 = 4;

unsafe extern "C" {
    fn flock(file_descriptor: i32, operation: i32) -> i32;
}

struct Shared {
    state: Mutex<State>,
    disk_io: Mutex<()>,
    changed: Notify,
    cache_path: Option<PathBuf>,
    gitstatus: Mutex<gitstatus::Client>,
}

#[derive(Default)]
struct State {
    entries: HashMap<CacheKey, Entry>,
    revision: u64,
    saved_revision: u64,
    load_epoch: u64,
    lru_order: u64,
}

struct LockGuard {
    _file: File,
}

struct SocketGuard {
    path: PathBuf,
}

pub async fn serve(socket: PathBuf) -> io::Result<()> {
    prepare_directory(&socket)?;
    let Some(_lock) = acquire_lock(&socket)? else {
        return Ok(());
    };
    prepare_socket(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let _socket = SocketGuard {
        path: socket.clone(),
    };

    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        disk_io: Mutex::new(()),
        changed: Notify::new(),
        cache_path: super::cache_path(),
        gitstatus: Mutex::new(gitstatus::Client::start()?),
    });

    let load_task = tokio::spawn(load_cache(Arc::clone(&shared)));
    let flush_task = tokio::spawn(flush_loop(Arc::clone(&shared)));
    let mut clients = JoinSet::new();

    loop {
        while clients.try_join_next().is_some() {}

        let accepted = timeout(IDLE_TIMEOUT, listener.accept()).await;
        let Ok(accepted) = accepted else {
            break;
        };
        let (stream, _) = accepted?;
        let state = Arc::clone(&shared);
        clients.spawn(async move {
            if let Err(error) = handle_client(stream, state).await {
                eprintln!("ztheme: daemon client failed: {error}");
            }
        });
    }

    load_task.abort();
    flush_task.abort();
    while clients.join_next().await.is_some() {}
    flush_latest(&shared).await.map(|_| ())
}

async fn handle_client(mut stream: UnixStream, shared: Arc<Shared>) -> io::Result<()> {
    match wire::read_header(&mut stream).await? {
        wire::GET => {
            let key = wire::read_key(&mut stream).await?;
            match cache_get(&shared, key).await {
                Some(value) => wire::write_hit(&mut stream, &value).await,
                None => wire::write_miss(&mut stream).await,
            }
        }
        wire::PUT => {
            let key = wire::read_key(&mut stream).await?;
            let value = wire::read_value(&mut stream).await?;
            cache_put(&shared, key, value).await;
            wire::write_ok(&mut stream).await
        }
        wire::PING => wire::write_ok(&mut stream).await,
        wire::CLEAR => {
            clear_cache(&shared).await?;
            shared.gitstatus.lock().await.restart()?;
            wire::write_ok(&mut stream).await
        }
        wire::GIT => {
            let query = wire::read_query(&mut stream).await?;
            let mut client = shared.gitstatus.lock().await;
            let result = if let Ok(result) = timeout(GITSTATUS_TIMEOUT, client.query(&query)).await
            {
                result
            } else {
                let restart = client.restart();
                match restart {
                    Ok(()) => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "gitstatusd query exceeded 30 seconds",
                    )),
                    Err(error) => Err(io::Error::other(format!(
                        "gitstatusd query timed out and restart failed: {error}"
                    ))),
                }
            };
            wire::write_git_result(&mut stream, result).await
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown daemon operation",
        )),
    }
}

async fn cache_get(shared: &Shared, key: CacheKey) -> Option<Arc<[u8]>> {
    let now = now_epoch_seconds();
    let mut state = shared.state.lock().await;
    state.lru_order = state.lru_order.saturating_add(1);
    let lru_order = state.lru_order;
    let entry = state.entries.get_mut(&key)?;

    if !entry.is_fresh(now) {
        state.entries.remove(&key);
        state.revision = state.revision.wrapping_add(1);
        shared.changed.notify_one();
        return None;
    }

    entry.last_used_at = now;
    entry.lru_order = lru_order;
    let should_persist_use =
        now.saturating_sub(entry.persisted_last_used_at) >= LAST_USED_SAVE_INTERVAL;
    let value = entry.value.clone();
    if should_persist_use {
        state.revision = state.revision.wrapping_add(1);
        shared.changed.notify_one();
    }
    Some(value)
}

async fn cache_put(shared: &Shared, key: CacheKey, value: Vec<u8>) {
    let now = now_epoch_seconds();
    let mut state = shared.state.lock().await;
    state.lru_order = state.lru_order.saturating_add(1);
    let lru_order = state.lru_order;
    state.entries.insert(key, Entry::new(value, now, lru_order));
    trim_lru(&mut state.entries);
    state.revision = state.revision.wrapping_add(1);
    shared.changed.notify_one();
}

async fn clear_cache(shared: &Shared) -> io::Result<()> {
    {
        let mut state = shared.state.lock().await;
        state.entries.clear();
        state.load_epoch = state.load_epoch.wrapping_add(1);
        state.revision = state.revision.wrapping_add(1);
    }

    let _disk = shared.disk_io.lock().await;
    let result = tokio::task::spawn_blocking(disk::clear_all)
        .await
        .map_err(io::Error::other)?;
    if result.is_ok() {
        let mut state = shared.state.lock().await;
        state.saved_revision = state.revision;
    } else {
        shared.changed.notify_one();
    }
    result
}

async fn load_cache(shared: Arc<Shared>) {
    let Some(path) = shared.cache_path.clone() else {
        return;
    };
    let load_epoch = shared.state.lock().await.load_epoch;
    let _disk = shared.disk_io.lock().await;
    let loaded = tokio::task::spawn_blocking(move || disk::load(&path)).await;
    let loaded = match loaded {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => return,
        Ok(Err(error)) => {
            eprintln!("ztheme: persistent cache load failed: {error}");
            return;
        }
        Err(error) => {
            eprintln!("ztheme: persistent cache task failed: {error}");
            return;
        }
    };

    let mut state = shared.state.lock().await;
    if state.load_epoch != load_epoch {
        return;
    }
    let mut entries: Vec<_> = loaded.entries.into_iter().collect();
    entries.sort_unstable_by_key(|(_, entry)| (entry.lru_order, entry.last_used_at));
    let shift = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    for entry in state.entries.values_mut() {
        entry.lru_order = entry.lru_order.saturating_add(shift);
    }
    state.lru_order = state.lru_order.saturating_add(shift);
    for (index, (key, mut entry)) in entries.into_iter().enumerate() {
        entry.lru_order = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        state.entries.entry(key).or_insert(entry);
    }
    trim_lru(&mut state.entries);
    if loaded.needs_rewrite {
        state.revision = state.revision.wrapping_add(1);
        shared.changed.notify_one();
    }
}

async fn flush_loop(shared: Arc<Shared>) {
    loop {
        shared.changed.notified().await;
        sleep(SAVE_DEBOUNCE).await;

        loop {
            match flush_latest(&shared).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    eprintln!("ztheme: persistent cache save failed: {error}");
                    sleep(SAVE_RETRY).await;
                }
            }
        }
    }
}

async fn flush_latest(shared: &Shared) -> io::Result<bool> {
    let Some(path) = shared.cache_path.clone() else {
        return Ok(false);
    };
    let (revision, entries) = {
        let state = shared.state.lock().await;
        if state.revision == state.saved_revision {
            return Ok(false);
        }
        (state.revision, state.entries.clone())
    };

    let _disk = shared.disk_io.lock().await;
    if shared.state.lock().await.revision != revision {
        return Ok(true);
    }
    tokio::task::spawn_blocking(move || disk::save(&path, &entries))
        .await
        .map_err(io::Error::other)??;

    let mut state = shared.state.lock().await;
    state.saved_revision = revision;
    for entry in state.entries.values_mut() {
        entry.persisted_last_used_at = entry.last_used_at;
    }
    Ok(state.revision != revision)
}

fn trim_lru(entries: &mut HashMap<CacheKey, Entry>) {
    while entries.len() > MAX_ENTRIES {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.lru_order)
            .map(|(key, _)| *key)
        else {
            return;
        };
        entries.remove(&oldest);
    }
}

fn prepare_directory(socket: &Path) -> io::Result<()> {
    let directory = socket
        .parent()
        .ok_or_else(|| io::Error::other("daemon socket has no parent directory"))?;
    match fs::create_dir(directory) {
        Ok(()) => fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != super::user_id()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon directory ownership or permissions are unsafe",
        ));
    }
    Ok(())
}

fn acquire_lock(socket: &Path) -> io::Result<Option<LockGuard>> {
    let path = super::lock_path(socket);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

    // SAFETY: file is open for this process and flock does not retain the pointer.
    if unsafe { flock(file.as_raw_fd(), LOCK_EXCLUSIVE | LOCK_NONBLOCKING) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error);
    }

    file.set_len(0)?;
    writeln!(file, "{}", std::process::id()).map(|()| Some(LockGuard { _file: file }))
}

fn prepare_socket(socket: &Path) -> io::Result<()> {
    match fs::remove_file(socket) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("ztheme: cache socket cleanup failed: {error}");
        }
    }
}
