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
use tokio::time::timeout;

use super::{RuntimeCache, lock_path, user_id, wire};
use crate::gitstatus;

const IDLE_TIMEOUT: Duration = Duration::from_hours(1);
const GITSTATUS_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_EXCLUSIVE: i32 = 2;
const LOCK_NONBLOCKING: i32 = 4;

unsafe extern "C" {
    fn flock(file_descriptor: i32, operation: i32) -> i32;
}

struct Shared {
    cache: Arc<RuntimeCache>,
    shutdown: Notify,
    gitstatus: Mutex<gitstatus::Client>,
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

    let cache = Arc::new(RuntimeCache::new());
    let shared = Arc::new(Shared {
        cache: Arc::clone(&cache),
        shutdown: Notify::new(),
        gitstatus: Mutex::new(gitstatus::Client::start()?),
    });

    let load_task = tokio::spawn(Arc::clone(&cache).load());
    let flush_task = tokio::spawn(Arc::clone(&cache).flush_loop());
    let mut clients = JoinSet::new();

    loop {
        while clients.try_join_next().is_some() {}

        let accepted = tokio::select! {
            () = shared.shutdown.notified() => break,
            accepted = timeout(IDLE_TIMEOUT, listener.accept()) => accepted,
        };
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
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    shared.cache.flush_latest().await.map(|_| ())
}

async fn handle_client(mut stream: UnixStream, shared: Arc<Shared>) -> io::Result<()> {
    match wire::read_header(&mut stream).await? {
        wire::RequestHeader::DaemonOutdated => {
            wire::write_daemon_outdated(&mut stream).await?;
            shared.shutdown.notify_one();
            Ok(())
        }
        wire::RequestHeader::ClientOutdated => wire::write_client_outdated(&mut stream).await,
        wire::RequestHeader::Operation(wire::GET) => {
            let key = wire::read_key(&mut stream).await?;
            match shared.cache.get(key).await {
                Some(value) => wire::write_hit(&mut stream, &value).await,
                None => wire::write_miss(&mut stream).await,
            }
        }
        wire::RequestHeader::Operation(wire::PUT) => {
            let key = wire::read_key(&mut stream).await?;
            let value = wire::read_value(&mut stream).await?;
            shared.cache.put(key, value).await?;
            wire::write_ok(&mut stream).await
        }
        wire::RequestHeader::Operation(wire::CLEAR) => {
            shared.cache.clear().await?;
            shared.gitstatus.lock().await.restart()?;
            wire::write_ok(&mut stream).await
        }
        wire::RequestHeader::Operation(wire::GIT) => {
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
        wire::RequestHeader::Operation(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown daemon operation",
        )),
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
    if !metadata.file_type().is_dir() || metadata.uid() != user_id() || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon directory ownership or permissions are unsafe",
        ));
    }
    Ok(())
}

fn acquire_lock(socket: &Path) -> io::Result<Option<LockGuard>> {
    let path = lock_path(socket);
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::acquire_lock;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ztheme-daemon-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn daemon_lock_has_a_single_owner_and_is_reusable() {
        let directory = TestDirectory::new();
        let socket = directory.path().join("daemon.sock");
        let first = acquire_lock(&socket).unwrap().unwrap();
        assert!(acquire_lock(&socket).unwrap().is_none());
        drop(first);
        assert!(acquire_lock(&socket).unwrap().is_some());
    }
}
