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
use tokio::sync::{Mutex, MutexGuard, Notify, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;

mod protocol;

use crate::cache::{Acquire, CacheKey, RuntimeCache};
use crate::gitstatus;
use crate::utils::HashBuilder;

const IDLE_TIMEOUT: Duration = Duration::from_hours(1);
const GITSTATUS_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTION_LIMIT: usize = 64;
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);
const HALF_CLOSED_POLL: Duration = Duration::from_millis(10);
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

    fn development_hash(name: &str) -> u64 {
        let mut hash = HashBuilder::new(b"ztheme-development-instance-v1");
        hash.add_bytes(b"name", name.as_bytes());
        hash.finish()
    }

    fn socket_path(&self) -> PathBuf {
        let directory = runtime_directory();
        match self {
            Self::Production => directory.join("daemon.sock"),
            Self::Development(name) => {
                directory.join(format!("dev-{:016x}.sock", Self::development_hash(name)))
            }
        }
    }

    fn runtime_cache_path(&self) -> Option<PathBuf> {
        crate::cache::path_for_file_name(&self.runtime_cache_file_name())
    }

    fn runtime_cache_file_name(&self) -> String {
        match self {
            Self::Production => "runtime-v2.bin".to_owned(),
            Self::Development(name) => {
                format!("runtime-v2-dev-{:016x}.bin", Self::development_hash(name))
            }
        }
    }

    fn runtime_cache(&self) -> RuntimeCache {
        match self {
            Self::Production => RuntimeCache::new(),
            Self::Development(_) => RuntimeCache::new_with_path(self.runtime_cache_path()),
        }
    }

    fn add_command_arguments(&self, command: &mut Command) {
        if let Self::Development(name) = self {
            command.arg("--dev").arg(name);
        }
    }
}

pub(crate) async fn runtime_cache_acquire(
    instance: &Instance,
    key: CacheKey,
) -> io::Result<Acquire> {
    let Response::CacheAcquire(value) =
        request(instance, Operation::RuntimeCacheAcquire(key)).await?
    else {
        unreachable!("runtime cache acquire returned a different response")
    };
    Ok(value)
}

pub(crate) async fn runtime_cache_put_owned(
    instance: &Instance,
    key: CacheKey,
    token: u64,
    value: &[u8],
) -> io::Result<bool> {
    let Response::CacheMutation(value) =
        request(instance, Operation::RuntimeCachePutOwned(key, token, value)).await?
    else {
        unreachable!("runtime cache put returned a different response")
    };
    Ok(value)
}

pub(crate) async fn runtime_cache_release(
    instance: &Instance,
    key: CacheKey,
    token: u64,
) -> io::Result<bool> {
    let Response::CacheMutation(value) =
        request(instance, Operation::RuntimeCacheRelease(key, token)).await?
    else {
        unreachable!("runtime cache release returned a different response")
    };
    Ok(value)
}

pub(crate) async fn runtime_cache_remove(instance: &Instance, key: CacheKey) -> io::Result<()> {
    let Response::Complete = request(instance, Operation::RuntimeCacheRemove(key)).await? else {
        unreachable!("runtime cache remove returned a different response")
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

    instance.runtime_cache().clear().await
}

pub(crate) async fn serve(instance: &Instance) -> io::Result<()> {
    serve_socket(instance.socket_path(), instance.runtime_cache()).await
}

#[derive(Clone, Copy)]
enum Operation<'a> {
    RuntimeCacheAcquire(CacheKey),
    RuntimeCachePutOwned(CacheKey, u64, &'a [u8]),
    RuntimeCacheRelease(CacheKey, u64),
    RuntimeCacheRemove(CacheKey),
    GitStatus(&'a gitstatus::Query),
}

enum Response {
    CacheAcquire(Acquire),
    CacheMutation(bool),
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
        Operation::RuntimeCacheAcquire(key) => protocol::runtime_cache_acquire(socket, key)
            .await
            .map(|response| match response {
                protocol::CacheAcquire::Hit(value) => {
                    Response::CacheAcquire(Acquire::Hit(Arc::from(value)))
                }
                protocol::CacheAcquire::Owner(token) => {
                    Response::CacheAcquire(Acquire::Owner(token))
                }
            }),
        Operation::RuntimeCachePutOwned(key, token, value) => {
            protocol::runtime_cache_put_owned(socket, key, token, value)
                .await
                .map(Response::CacheMutation)
        }
        Operation::RuntimeCacheRelease(key, token) => {
            protocol::runtime_cache_release(socket, key, token)
                .await
                .map(Response::CacheMutation)
        }
        Operation::RuntimeCacheRemove(key) => protocol::runtime_cache_remove(socket, key)
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
    gitstatus: Mutex<Option<gitstatus::Client>>,
}

struct LockGuard {
    _file: File,
}

struct SocketGuard {
    path: PathBuf,
}

async fn serve_socket(socket: PathBuf, cache: RuntimeCache) -> io::Result<()> {
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

    let cache = Arc::new(cache);
    let shared = Arc::new(Shared {
        cache: Arc::clone(&cache),
        shutdown: Notify::new(),
        gitstatus: Mutex::new(None),
    });

    // Load before accepting requests so a client cannot observe a cold cache
    // while persisted entries are still being read.
    cache.clone().load().await;
    let flush_task = tokio::spawn(Arc::clone(&cache).flush_loop());
    let mut clients = JoinSet::new();
    let admission = Arc::new(Semaphore::new(CONNECTION_LIMIT));

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
        admit_client(stream, Arc::clone(&shared), &admission, &mut clients);
    }

    flush_task.abort();
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    shared.cache.flush_latest().await.map(|_| ())
}

fn admit_client(
    stream: UnixStream,
    shared: Arc<Shared>,
    admission: &Arc<Semaphore>,
    clients: &mut JoinSet<()>,
) {
    // Admission is synchronous: excess streams are dropped without a
    // queued permit waiter or another retained task.
    let Ok(permit) = Arc::clone(admission).try_acquire_owned() else {
        return;
    };
    clients.spawn(async move {
        let _permit = permit;
        if let Err(error) = handle_client(stream, shared).await {
            eprintln!("ztheme: daemon client failed: {error}");
        }
    });
}

async fn handle_client(mut stream: UnixStream, shared: Arc<Shared>) -> io::Result<()> {
    let request = timeout(FRAME_TIMEOUT, protocol::read_request(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "daemon request frame timed out"))??;
    // Cache acquire keeps its 400 ms waiter budget; Git owns its separate
    // 30 second protocol deadline. Neither is part of the framing deadline.
    let response = dispatch(request, &shared, &stream).await?;
    let shutdown = matches!(response, protocol::Response::DaemonOutdated);
    timeout(
        RESPONSE_TIMEOUT,
        protocol::write_response(&mut stream, response),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "daemon response timed out"))??;
    if shutdown {
        shared.shutdown.notify_one();
    }
    Ok(())
}

async fn dispatch(
    request: protocol::Request,
    shared: &Shared,
    stream: &UnixStream,
) -> io::Result<protocol::Response> {
    use protocol::{Request, Response};

    let response = match request {
        Request::DaemonOutdated => Response::DaemonOutdated,
        Request::ClientOutdated => Response::ClientOutdated,
        Request::CacheAcquire(key) => Response::CacheAcquire(shared.cache.acquire(key).await?),
        Request::CachePutOwned(key, token, value) => {
            Response::Mutation(shared.cache.put_owned(key, token, value).await?)
        }
        Request::CacheRelease(key, token) => {
            Response::Mutation(shared.cache.release_owned(key, token).await)
        }
        Request::CacheRemove(key) => {
            shared.cache.remove(key).await?;
            Response::Complete
        }
        Request::Reset => {
            reset_state(shared).await?;
            Response::Complete
        }
        Request::GitStatus(query) => Response::GitStatus(git_query(shared, &query, stream).await),
    };
    Ok(response)
}

async fn reset_state(shared: &Shared) -> io::Result<()> {
    shared.cache.clear().await?;
    // Reset an already started capability, but never start an unused one.
    let mut client = shared.gitstatus.lock().await;
    if let Some(client) = client.as_mut() {
        client.restart()?;
    }
    Ok(())
}

/// Runs one Git query against the lazily started `gitstatusd` client. The
/// client is created on the first real Git request, so a daemon that only
/// serves runtime-cache operations never requires the managed binary. Startup
/// and restart failures surface as explicit Git errors without taking down
/// the daemon.
async fn git_query(
    shared: &Shared,
    query: &gitstatus::Query,
    stream: &UnixStream,
) -> io::Result<Option<gitstatus::Snapshot>> {
    let mut client = acquire_git_owner(shared, stream).await?;
    let client = match *client {
        Some(ref mut client) => client,
        None => client.insert(gitstatus::Client::start()?),
    };
    // After taking ownership, finish/drain the query even if the peer leaves.
    // Cancelling here would abandon a reply on the shared gitstatusd pipe.
    run_git_query(client, query).await
}

async fn acquire_git_owner<'a>(
    shared: &'a Shared,
    stream: &UnixStream,
) -> io::Result<MutexGuard<'a, Option<gitstatus::Client>>> {
    let client = tokio::select! {
        biased;
        error = disconnected(stream) => return Err(error),
        client = shared.gitstatus.lock() => client,
    };
    // Close the race between the disconnect watcher and an available mutex.
    // No gitstatusd request has started, so releasing this guard is safe.
    probe_response_peer(stream).await?;
    Ok(client)
}

/// A connection has exactly one request. Extra input is invalid. Read EOF
/// alone may be a write-half-close by a client still awaiting its response.
async fn disconnected(stream: &UnixStream) -> io::Error {
    loop {
        if let Err(error) = stream.readable().await {
            return error;
        }
        match stream.try_read(&mut [0]) {
            Ok(0) => {
                if let Err(error) = probe_response_peer(stream).await {
                    return error;
                }
                // EOF stays readable forever. Poll half-closed peers without
                // spinning, so a later full close can still cancel the queue.
                tokio::time::sleep(HALF_CLOSED_POLL).await;
            }
            Ok(_) => {
                return io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected data after daemon request",
                );
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return error,
        }
    }
}

/// A zero-length Unix socket write detects a closed response peer without
/// sending protocol bytes, including after its request side was half-closed.
async fn probe_response_peer(stream: &UnixStream) -> io::Result<()> {
    loop {
        stream.writable().await?;
        match stream.try_write(&[]) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
}

async fn run_git_query(
    client: &mut gitstatus::Client,
    query: &gitstatus::Query,
) -> io::Result<Option<gitstatus::Snapshot>> {
    if let Ok(result) = timeout(GITSTATUS_TIMEOUT, client.query(query)).await {
        return result;
    }
    match client.restart() {
        Ok(()) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "gitstatusd query exceeded 30 seconds",
        )),
        Err(error) => Err(io::Error::other(format!(
            "gitstatusd query timed out and restart failed: {error}"
        ))),
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

    fn shared() -> std::sync::Arc<super::Shared> {
        std::sync::Arc::new(super::Shared {
            cache: std::sync::Arc::new(crate::cache::RuntimeCache::new_with_path(None)),
            shutdown: tokio::sync::Notify::new(),
            gitstatus: tokio::sync::Mutex::new(None),
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_and_partial_frames_expire_without_state_operations() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixStream;
        use tokio::task::JoinSet;

        let mut cases = JoinSet::new();
        let state = shared();
        let key = crate::cache::CacheKey::from_value(42);
        let mut put = b"ZT\x00\x02\x02".to_vec();
        put.extend_from_slice(&key.bytes());
        put.extend_from_slice(&0_u64.to_be_bytes());
        put.extend_from_slice(&10_u32.to_be_bytes());
        put.push(b'x');
        for frame in [
            Vec::new(),
            b"Z".to_vec(),
            b"ZT\x00\x02\x01x".to_vec(),
            put,
            b"ZT\x00\x02\x05\x00\x00\x00\x00\x04x".to_vec(),
        ] {
            let state = std::sync::Arc::clone(&state);
            cases.spawn(async move {
                let (mut client, server) = UnixStream::pair().unwrap();
                client.write_all(&frame).await.unwrap();
                let error = super::handle_client(server, state).await.unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
                assert_eq!(error.to_string(), "daemon request frame timed out");
                assert_eq!(
                    client.read_u8().await.unwrap_err().kind(),
                    std::io::ErrorKind::UnexpectedEof
                );
            });
        }
        while let Some(result) = cases.join_next().await {
            result.unwrap();
        }
        assert!(matches!(
            state.cache.acquire(key).await.unwrap(),
            crate::cache::Acquire::Owner(_)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admission_rejects_excess_tasks_and_recovers_after_frame_cleanup() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixStream;
        use tokio::sync::Semaphore;
        use tokio::task::JoinSet;

        let admission = Arc::new(Semaphore::new(super::CONNECTION_LIMIT));
        let state = shared();
        let mut clients = JoinSet::new();
        let mut peers = Vec::new();
        for _ in 0..super::CONNECTION_LIMIT {
            let (client, server) = UnixStream::pair().unwrap();
            super::admit_client(server, Arc::clone(&state), &admission, &mut clients);
            peers.push(client);
        }
        assert_eq!(clients.len(), super::CONNECTION_LIMIT);
        assert_eq!(admission.available_permits(), 0);
        let (mut excess, server) = UnixStream::pair().unwrap();
        super::admit_client(server, Arc::clone(&state), &admission, &mut clients);
        assert_eq!(clients.len(), super::CONNECTION_LIMIT);
        assert_eq!(
            excess.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(result) = clients.join_next().await {
                result.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(admission.available_permits(), super::CONNECTION_LIMIT);
        for mut peer in peers {
            assert_eq!(
                peer.read_u8().await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        }

        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"ZT\x00\x02\x06").await.unwrap();
        client
            .write_all(&crate::cache::CacheKey::from_value(42).bytes())
            .await
            .unwrap();
        super::admit_client(server, state, &admission, &mut clients);
        assert_eq!(client.read_u8().await.unwrap(), 3);
        clients.join_next().await.unwrap().unwrap();
        assert_eq!(admission.available_permits(), super::CONNECTION_LIMIT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_response_reader_is_bounded_separately_from_framing() {
        use std::os::fd::AsRawFd as _;
        use tokio::io::AsyncWriteExt as _;
        use tokio::net::UnixStream;

        let state = shared();
        let key = crate::cache::CacheKey::from_value(42);
        state
            .cache
            .put(key, vec![b'x'; crate::cache::MAX_VALUE_BYTES])
            .await
            .unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let size: libc::c_int = 1024;
        // SAFETY: the socket is live, and size is a correctly sized integer.
        let result = unsafe {
            libc::setsockopt(
                server.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::from_ref(&size).cast(),
                libc::socklen_t::try_from(std::mem::size_of_val(&size)).unwrap(),
            )
        };
        assert_eq!(result, 0);
        client.write_all(b"ZT\x00\x02\x01").await.unwrap();
        client.write_all(&key.bytes()).await.unwrap();
        let error = super::handle_client(server, state).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "daemon response timed out");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disconnected_queued_git_never_acquires_or_starts_the_process() {
        use std::sync::Arc;
        use tokio::net::UnixStream;

        for half_close_first in [false, true] {
            let state = shared();
            let owner = state.gitstatus.lock().await;
            let (client, server) = UnixStream::pair().unwrap();
            let queued_state = Arc::clone(&state);
            let queued = tokio::spawn(async move {
                super::git_query(
                    &queued_state,
                    &crate::gitstatus::Query::Directory("/repo".into()),
                    &server,
                )
                .await
            });
            if half_close_first {
                use tokio::io::AsyncWriteExt as _;
                let mut client = client;
                client.shutdown().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                assert!(
                    !queued.is_finished(),
                    "write-half-close still awaits a response"
                );
                drop(client);
            } else {
                drop(client);
            }
            let result = tokio::time::timeout(std::time::Duration::from_millis(200), queued)
                .await
                .unwrap()
                .unwrap();
            assert!(result.is_err());
            assert!(owner.is_none(), "obsolete Git work started gitstatusd");
            drop(owner);
            assert!(state.gitstatus.lock().await.is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_half_closed_git_client_can_acquire_the_owner() {
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt as _;
        use tokio::net::UnixStream;

        let state = shared();
        let owner = state.gitstatus.lock().await;
        let (mut client, server) = UnixStream::pair().unwrap();
        client.shutdown().await.unwrap();
        let queued_state = Arc::clone(&state);
        let queued = tokio::spawn(async move {
            super::acquire_git_owner(&queued_state, &server)
                .await
                .is_ok()
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!queued.is_finished());
        drop(owner);
        assert!(queued.await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lease_capacity_failure_propagates_and_closes_the_transport() {
        use std::io;
        use std::sync::Arc;

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixStream;
        use tokio::sync::{Mutex, Notify};

        use super::{Shared, handle_client};
        use crate::cache::{Acquire, CacheKey, RuntimeCache};

        let cache = Arc::new(RuntimeCache::new_with_path(None));
        for value in 0..500 {
            assert!(matches!(
                cache.acquire(CacheKey::from_value(value)).await.unwrap(),
                Acquire::Owner(_)
            ));
        }
        let shared = Arc::new(Shared {
            cache,
            shutdown: Notify::new(),
            gitstatus: Mutex::new(None),
        });
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"ZT\x00\x02\x01").await.unwrap();
        client
            .write_all(&CacheKey::from_value(500).bytes())
            .await
            .unwrap();
        let error = handle_client(server, shared).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("lease capacity exhausted"));
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(
            response.is_empty(),
            "capacity exhaustion must not grant ownership"
        );
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

        let production_cache = Instance::Production.runtime_cache_path().unwrap();
        let first_cache = Instance::development("one".to_owned())
            .unwrap()
            .runtime_cache_path()
            .unwrap();
        let second_cache = Instance::development("two".to_owned())
            .unwrap()
            .runtime_cache_path()
            .unwrap();
        assert_eq!(production_cache.file_name().unwrap(), "runtime-v2.bin");
        assert_eq!(
            Instance::Production.runtime_cache_file_name(),
            "runtime-v2.bin"
        );
        assert_ne!(production_cache, first_cache);
        assert_ne!(first_cache, second_cache);
        assert_eq!(
            first_cache,
            Instance::development("one".to_owned())
                .unwrap()
                .runtime_cache_path()
                .unwrap()
        );
        assert!(
            first_cache
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("runtime-v2-dev-")
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
