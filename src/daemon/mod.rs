use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinSet;
use tokio::time::timeout;

mod protocol;

use crate::cache::{CacheKey, RuntimeCache};
use crate::gitstatus;
use crate::utils::HashBuilder;

const IDLE_TIMEOUT: Duration = Duration::from_hours(1);
const GITSTATUS_TIMEOUT: Duration = Duration::from_secs(30);
const START_ATTEMPTS: usize = 10;
const START_DELAY: Duration = Duration::from_millis(20);
const REPLACEMENT_ATTEMPTS: usize = 20;
const REPLACEMENT_DELAY: Duration = Duration::from_millis(10);
const LOCK_EXCLUSIVE: i32 = 2;
const LOCK_NONBLOCKING: i32 = 4;

unsafe extern "C" {
    fn flock(file_descriptor: i32, operation: i32) -> i32;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Instance {
    Production,
    Development(String),
}

impl Instance {
    pub(crate) fn development(name: String) -> Result<Self, &'static str> {
        if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
            return Err("development instance name must be 1-64 printable characters");
        }
        Ok(Self::Development(name))
    }

    pub(crate) fn development_name(&self) -> Option<&str> {
        match self {
            Self::Production => None,
            Self::Development(name) => Some(name),
        }
    }

    fn socket_path(&self) -> PathBuf {
        let directory = runtime_directory();
        match self {
            Self::Production => directory.join("daemon.sock"),
            Self::Development(name) => {
                let mut hash = HashBuilder::new(b"ztheme-development-instance-v1");
                hash.add_bytes(b"name", name.as_bytes());
                directory.join(format!("dev-{:016x}.sock", hash.finish()))
            }
        }
    }

    fn add_command_arguments(&self, command: &mut Command) {
        if let Self::Development(name) = self {
            command.arg("--dev").arg(name);
        }
    }
}

pub(crate) async fn runtime_cache_get(
    instance: &Instance,
    key: CacheKey,
) -> io::Result<Option<Vec<u8>>> {
    let Response::RuntimeCache(value) = request(instance, Operation::RuntimeCacheGet(key)).await?
    else {
        unreachable!("runtime cache get returned a different response")
    };
    Ok(value)
}

pub(crate) async fn runtime_cache_put(
    instance: &Instance,
    key: CacheKey,
    value: &[u8],
) -> io::Result<()> {
    let Response::Complete = request(instance, Operation::RuntimeCachePut(key, value)).await?
    else {
        unreachable!("runtime cache put returned a different response")
    };
    Ok(())
}

pub(crate) async fn git_status(
    instance: &Instance,
    query: &gitstatus::Query,
) -> io::Result<Option<gitstatus::Snapshot>> {
    let Response::GitStatus(value) = request(instance, Operation::GitStatus(query)).await? else {
        unreachable!("Git status returned a different response")
    };
    Ok(value)
}

pub(crate) async fn reset(instance: &Instance) -> io::Result<()> {
    let socket = instance.socket_path();
    match protocol::reset(&socket).await {
        Ok(()) => return Ok(()),
        Err(protocol::Error::ClientOutdated) => return Err(client_outdated()),
        Err(protocol::Error::Io(error)) if !daemon_unavailable(&error) => return Err(error),
        Err(protocol::Error::Io(_) | protocol::Error::DaemonOutdated) => {}
    }

    for _ in 0..REPLACEMENT_ATTEMPTS {
        tokio::time::sleep(REPLACEMENT_DELAY).await;
        match protocol::reset(&socket).await {
            Ok(()) => return Ok(()),
            Err(protocol::Error::ClientOutdated) => return Err(client_outdated()),
            Err(protocol::Error::Io(error)) if replacement_transition(&error) => {
                if !socket.try_exists()? {
                    break;
                }
            }
            Err(protocol::Error::DaemonOutdated) => {}
            Err(protocol::Error::Io(error)) => return Err(error),
        }
    }

    RuntimeCache::new().clear().await
}

pub(crate) async fn serve(instance: &Instance) -> io::Result<()> {
    serve_socket(instance.socket_path()).await
}

#[derive(Clone, Copy)]
enum Operation<'a> {
    RuntimeCacheGet(CacheKey),
    RuntimeCachePut(CacheKey, &'a [u8]),
    GitStatus(&'a gitstatus::Query),
}

enum Response {
    RuntimeCache(Option<Vec<u8>>),
    GitStatus(Option<gitstatus::Snapshot>),
    Complete,
}

async fn request(instance: &Instance, operation: Operation<'_>) -> io::Result<Response> {
    let socket = instance.socket_path();
    match perform(&socket, operation).await {
        Ok(value) => return Ok(value),
        Err(protocol::Error::ClientOutdated) => return Err(client_outdated()),
        Err(protocol::Error::DaemonOutdated) => {
            return replace_daemon(instance, &socket, operation).await;
        }
        Err(protocol::Error::Io(error)) if daemon_unavailable(&error) => {}
        Err(protocol::Error::Io(error)) => return Err(error),
    }

    spawn_daemon(instance)?;
    let mut last_error = None;
    for _ in 0..START_ATTEMPTS {
        tokio::time::sleep(START_DELAY).await;
        match perform(&socket, operation).await {
            Ok(value) => return Ok(value),
            Err(protocol::Error::ClientOutdated) => return Err(client_outdated()),
            Err(protocol::Error::DaemonOutdated) => {
                return replace_daemon(instance, &socket, operation).await;
            }
            Err(protocol::Error::Io(error)) if daemon_unavailable(&error) => {
                last_error = Some(error);
            }
            Err(protocol::Error::Io(error)) => return Err(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "ztheme daemon did not start")))
}

async fn replace_daemon(
    instance: &Instance,
    socket: &Path,
    operation: Operation<'_>,
) -> io::Result<Response> {
    let mut spawned = false;
    for _ in 0..REPLACEMENT_ATTEMPTS {
        tokio::time::sleep(REPLACEMENT_DELAY).await;
        match perform(socket, operation).await {
            Ok(value) => return Ok(value),
            Err(protocol::Error::ClientOutdated) => return Err(client_outdated()),
            Err(protocol::Error::DaemonOutdated) => {}
            Err(protocol::Error::Io(error)) if replacement_transition(&error) => {
                if !spawned && !socket.try_exists()? {
                    spawn_daemon(instance)?;
                    spawned = true;
                }
            }
            Err(protocol::Error::Io(error)) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "outdated ztheme daemon did not restart",
    ))
}

async fn perform(socket: &Path, operation: Operation<'_>) -> protocol::Result<Response> {
    match operation {
        Operation::RuntimeCacheGet(key) => protocol::runtime_cache_get(socket, key)
            .await
            .map(Response::RuntimeCache),
        Operation::RuntimeCachePut(key, value) => protocol::runtime_cache_put(socket, key, value)
            .await
            .map(|()| Response::Complete),
        Operation::GitStatus(query) => protocol::git_status(socket, query)
            .await
            .map(Response::GitStatus),
    }
}

fn spawn_daemon(instance: &Instance) -> io::Result<()> {
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    instance.add_command_arguments(&mut command);
    command.spawn()?;
    Ok(())
}

fn client_outdated() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "ztheme client is older than the running daemon",
    )
}

fn daemon_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

fn replacement_transition(error: &io::Error) -> bool {
    daemon_unavailable(error)
        || matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::UnexpectedEof
        )
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

async fn serve_socket(socket: PathBuf) -> io::Result<()> {
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
    match protocol::read_header(&mut stream).await? {
        protocol::RequestHeader::DaemonOutdated => {
            protocol::write_daemon_outdated(&mut stream).await?;
            shared.shutdown.notify_one();
            Ok(())
        }
        protocol::RequestHeader::ClientOutdated => {
            protocol::write_client_outdated(&mut stream).await
        }
        protocol::RequestHeader::Operation(protocol::RUNTIME_CACHE_GET) => {
            let key = protocol::read_key(&mut stream).await?;
            match shared.cache.get(key).await {
                Some(value) => protocol::write_hit(&mut stream, &value).await,
                None => protocol::write_miss(&mut stream).await,
            }
        }
        protocol::RequestHeader::Operation(protocol::RUNTIME_CACHE_PUT) => {
            let key = protocol::read_key(&mut stream).await?;
            let value = protocol::read_value(&mut stream).await?;
            shared.cache.put(key, value).await?;
            protocol::write_ok(&mut stream).await
        }
        protocol::RequestHeader::Operation(protocol::RESET) => {
            shared.cache.clear().await?;
            shared.gitstatus.lock().await.restart()?;
            protocol::write_ok(&mut stream).await
        }
        protocol::RequestHeader::Operation(protocol::GIT_STATUS) => {
            let query = protocol::read_query(&mut stream).await?;
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
            protocol::write_git_result(&mut stream, result).await
        }
        protocol::RequestHeader::Operation(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown daemon operation",
        )),
    }
}

fn lock_path(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

/// The runtime directory for sockets and lock files. Production uses the
/// per-user /tmp directory; tests override it with `ZTHEME_RUNTIME_DIR` so
/// development instances never pollute the shared directory. The override
/// inherits to every spawned process (shell, client, server).
fn runtime_directory() -> PathBuf {
    std::env::var_os("ZTHEME_RUNTIME_DIR").map_or_else(
        || Path::new("/tmp").join(format!("ztheme-{}", user_id())),
        PathBuf::from,
    )
}

fn user_id() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }

    // SAFETY: getuid takes no arguments and has no failure mode.
    unsafe { getuid() }
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

    use super::{Instance, acquire_lock, daemon_unavailable, lock_path, replacement_transition};

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

    #[test]
    fn repeated_lock_generations_reuse_one_lock_file() {
        let directory = TestDirectory::new();
        let socket = directory.path().join("daemon.sock");
        let lock_path = lock_path(&socket);

        for _ in 0..20 {
            // A transient WouldBlock can follow the previous owner's drop
            // under load; retry within a bounded window, as the daemon's
            // startup loop does. The bound keeps a real descriptor leak from
            // hanging the suite instead of failing it.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let guard = loop {
                if let Some(guard) = acquire_lock(&socket).unwrap() {
                    break guard;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "lock was not released after dropping its previous owner"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            assert!(lock_path.exists());
            drop(guard);
        }

        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn development_instances_are_validated_and_isolated() {
        assert!(Instance::development(String::new()).is_err());
        assert!(Instance::development("x".repeat(65)).is_err());
        assert!(Instance::development("bad\nname".to_owned()).is_err());

        let production = Instance::Production.socket_path();
        let first = Instance::development("one".to_owned())
            .unwrap()
            .socket_path();
        let second = Instance::development("two".to_owned())
            .unwrap()
            .socket_path();
        assert_ne!(production, first);
        assert_ne!(first, second);
        assert_eq!(
            first,
            Instance::development("one".to_owned())
                .unwrap()
                .socket_path()
        );
    }

    #[test]
    fn daemon_transition_errors_are_classified_explicitly() {
        assert!(daemon_unavailable(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(replacement_transition(&std::io::Error::from(
            std::io::ErrorKind::ConnectionReset
        )));
        assert!(!replacement_transition(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }
}
