mod daemon;
mod disk;
mod wire;

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::gitstatus::{Query, Snapshot};
use crate::utils::HashBuilder;

const CACHE_FILE_PREFIX: &str = "runtime-v1-";
const CACHE_FILE_SUFFIX: &str = ".bin";
const CACHE_FORMAT_VERSION: u16 = 1;
const MAX_ENTRIES: usize = 500;
const MAX_VALUE_BYTES: usize = 16 * 1024;
const SAFETY_EXPIRY: Duration = Duration::from_hours(24);
const REPLACEMENT_ATTEMPTS: usize = 20;
const REPLACEMENT_DELAY: Duration = Duration::from_millis(10);

macro_rules! request_with_upgrade {
    ($instance:expr, |$socket:ident| $operation:expr) => {{
        let instance = $instance;
        let $socket = instance.socket_path();
        match $operation.await {
            Ok(value) => Ok(value),
            Err(wire::Error::Io(error)) => Err(error),
            Err(wire::Error::ClientOutdated) => Err(client_outdated()),
            Err(wire::Error::DaemonOutdated) => {
                let mut spawned = false;
                let mut result = None;
                for _ in 0..REPLACEMENT_ATTEMPTS {
                    tokio::time::sleep(REPLACEMENT_DELAY).await;
                    match $operation.await {
                        Ok(value) => {
                            result = Some(Ok(value));
                            break;
                        }
                        Err(wire::Error::DaemonOutdated) => {}
                        Err(wire::Error::ClientOutdated) => {
                            result = Some(Err(client_outdated()));
                            break;
                        }
                        Err(wire::Error::Io(error)) if replacement_transition(&error) => {
                            if !spawned && !$socket.try_exists()? {
                                spawn_daemon(instance)?;
                                spawned = true;
                            }
                        }
                        Err(wire::Error::Io(error)) => {
                            result = Some(Err(error));
                            break;
                        }
                    }
                }
                result.unwrap_or_else(|| {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "outdated ztheme daemon did not restart",
                    ))
                })
            }
        }
    }};
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Instance {
    Production,
    Development(String),
}

impl Instance {
    pub fn development(name: String) -> Result<Self, &'static str> {
        if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
            return Err("development instance name must be 1-64 printable characters");
        }
        Ok(Self::Development(name))
    }

    pub fn development_name(&self) -> Option<&str> {
        match self {
            Self::Production => None,
            Self::Development(name) => Some(name),
        }
    }

    fn socket_path(&self) -> PathBuf {
        let user = user_id();
        let directory = Path::new("/tmp").join(format!("ztheme-{user}"));
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CacheKey(u64);

impl CacheKey {
    pub const fn from_value(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug)]
struct Entry {
    value: Arc<[u8]>,
    refreshed_at: u64,
    last_used_at: u64,
    persisted_last_used_at: u64,
    lru_order: u64,
}

impl Entry {
    fn new(value: Vec<u8>, now: u64, lru_order: u64) -> Self {
        Self {
            value: Arc::from(value),
            refreshed_at: now,
            last_used_at: now,
            persisted_last_used_at: 0,
            lru_order,
        }
    }

    fn is_fresh(&self, now: u64) -> bool {
        now.checked_sub(self.refreshed_at)
            .is_some_and(|age| age <= SAFETY_EXPIRY.as_secs())
    }
}

pub async fn get(instance: &Instance, key: CacheKey) -> io::Result<Option<Vec<u8>>> {
    request_with_upgrade!(instance, |socket| wire::get(&socket, key))
}

pub async fn put(instance: &Instance, key: CacheKey, value: &[u8]) -> io::Result<()> {
    validate_value(value)?;
    request_with_upgrade!(instance, |socket| wire::put(&socket, key, value))
}

pub async fn git(instance: &Instance, query: &Query) -> io::Result<Option<Snapshot>> {
    match request_with_upgrade!(instance, |socket| wire::git(&socket, query)) {
        Ok(snapshot) => Ok(snapshot),
        Err(first_error) if daemon_unavailable(&first_error) => {
            spawn_daemon(instance)?;
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                match request_with_upgrade!(instance, |socket| wire::git(&socket, query)) {
                    Ok(snapshot) => return Ok(snapshot),
                    Err(error) if daemon_unavailable(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(first_error)
        }
        Err(error) => Err(error),
    }
}

pub async fn clear(instance: &Instance) -> io::Result<()> {
    match request_with_upgrade!(instance, |socket| wire::clear(&socket)) {
        Ok(()) => return Ok(()),
        Err(error) if daemon_unavailable(&error) => {}
        Err(error) => return Err(error),
    }

    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        match request_with_upgrade!(instance, |socket| wire::clear(&socket)) {
            Ok(()) => return Ok(()),
            Err(error) if daemon_unavailable(&error) => {}
            Err(error) => return Err(error),
        }
    }

    disk::clear_all()
}

pub fn spawn_daemon(instance: &Instance) -> io::Result<()> {
    let executable = env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    instance.add_command_arguments(&mut command);
    command.spawn()?;
    Ok(())
}

pub async fn serve(instance: &Instance) -> io::Result<()> {
    daemon::serve(instance.socket_path()).await
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

fn cache_path() -> Option<PathBuf> {
    cache_root().map(|root| {
        root.join("ztheme").join(format!(
            "{CACHE_FILE_PREFIX}{}{CACHE_FILE_SUFFIX}",
            cache_identity()
        ))
    })
}

fn cache_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_CACHE_HOME") {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            return Some(root);
        }
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".cache"))
}

fn cache_identity() -> String {
    let mut hash = HashBuilder::new(b"ztheme-cache-identity-v1");
    hash.add_bytes(b"package-version", env!("CARGO_PKG_VERSION").as_bytes());
    hash.add_u64(b"cache-format-version", u64::from(CACHE_FORMAT_VERSION));
    hash.add_u64(b"wire-version", u64::from(wire::VERSION));
    if let Ok(executable) = env::current_exe() {
        hash.add_path(b"executable", &executable);
    }
    format!("{:016x}", hash.finish())
}

fn lock_path(socket: &Path) -> PathBuf {
    socket.with_extension("lock")
}

fn validate_value(value: &[u8]) -> io::Result<()> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache value exceeds size limit",
        ));
    }
    Ok(())
}

fn now_epoch_seconds() -> u64 {
    epoch_duration(SystemTime::now()).map_or(0, |duration| duration.as_secs())
}

fn epoch_duration(time: SystemTime) -> Option<Duration> {
    time.duration_since(UNIX_EPOCH).ok()
}

fn user_id() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }

    // SAFETY: getuid takes no arguments and has no failure mode.
    unsafe { getuid() }
}

#[cfg(test)]
mod tests {
    use super::{Entry, Instance, SAFETY_EXPIRY};

    #[test]
    fn development_instance_names_are_validated() {
        assert!(Instance::development(String::new()).is_err());
        assert!(Instance::development("x".repeat(65)).is_err());
        assert!(Instance::development("bad\nname".to_owned()).is_err());
        assert!(Instance::development("feature".to_owned()).is_ok());
    }

    #[test]
    fn instance_socket_paths_are_isolated() {
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
    fn entry_freshness_has_an_exact_safety_boundary() {
        let entry = Entry::new(Vec::new(), 100, 1);
        assert!(entry.is_fresh(100 + SAFETY_EXPIRY.as_secs()));
        assert!(!entry.is_fresh(101 + SAFETY_EXPIRY.as_secs()));
        assert!(!entry.is_fresh(99));
    }
}
