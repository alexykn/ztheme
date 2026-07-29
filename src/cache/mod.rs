mod daemon;
mod disk;
mod wire;

use std::env;
use std::ffi::OsStr;
use std::fs::Metadata;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::gitstatus::{Query, Snapshot};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const CACHE_FILE_PREFIX: &str = "runtime-v1-";
const CACHE_FILE_SUFFIX: &str = ".bin";
const CACHE_FORMAT_VERSION: u16 = 1;
const MAX_ENTRIES: usize = 500;
const MAX_VALUE_BYTES: usize = 16 * 1024;
const SAFETY_EXPIRY: Duration = Duration::from_hours(24);

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
                let mut fingerprint = Fingerprint::new(b"ztheme-development-instance-v1");
                fingerprint.add_bytes(b"name", name.as_bytes());
                directory.join(format!("dev-{:016x}.sock", fingerprint.finish().value()))
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
pub struct Fingerprint {
    state: u64,
}

impl Fingerprint {
    pub fn new(domain: &[u8]) -> Self {
        let mut fingerprint = Self { state: FNV_OFFSET };
        fingerprint.add_bytes(b"domain", domain);
        fingerprint
    }

    pub fn add_bytes(&mut self, label: &[u8], value: &[u8]) {
        self.add_raw(&u64_len(label).to_be_bytes());
        self.add_raw(label);
        self.add_raw(&u64_len(value).to_be_bytes());
        self.add_raw(value);
    }

    pub fn add_os(&mut self, label: &[u8], value: &OsStr) {
        self.add_bytes(label, value.as_bytes());
    }

    pub fn add_optional_os(&mut self, label: &[u8], value: Option<&OsStr>) {
        match value {
            Some(value) => {
                self.add_bytes(b"present", b"1");
                self.add_os(label, value);
            }
            None => self.add_bytes(b"present", b"0"),
        }
    }

    pub fn add_path(&mut self, label: &[u8], value: &Path) {
        self.add_os(label, value.as_os_str());
    }

    pub fn add_u64(&mut self, label: &[u8], value: u64) {
        self.add_bytes(label, &value.to_be_bytes());
    }

    pub fn add_metadata(&mut self, label: &[u8], metadata: Option<&Metadata>) {
        let Some(metadata) = metadata else {
            self.add_bytes(label, b"missing");
            return;
        };

        self.add_bytes(label, b"present");
        self.add_u64(b"metadata-length", metadata.len());
        self.add_u64(b"metadata-device", metadata.dev());
        self.add_u64(b"metadata-inode", metadata.ino());
        self.add_u64(b"metadata-mode", u64::from(metadata.mode()));
        self.add_u64(
            b"metadata-change-seconds",
            u64::try_from(metadata.ctime()).unwrap_or(0),
        );
        self.add_u64(
            b"metadata-change-nanos",
            u64::try_from(metadata.ctime_nsec()).unwrap_or(0),
        );
        match metadata.modified().ok().and_then(epoch_duration) {
            Some(modified) => {
                self.add_u64(b"metadata-modified-seconds", modified.as_secs());
                self.add_u64(
                    b"metadata-modified-nanos",
                    u64::from(modified.subsec_nanos()),
                );
            }
            None => self.add_bytes(b"metadata-modified", b"unknown"),
        }
    }

    pub fn finish(self) -> CacheKey {
        CacheKey(self.state)
    }

    fn add_raw(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(FNV_PRIME);
        }
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
    wire::get(&instance.socket_path(), key).await
}

pub async fn put(instance: &Instance, key: CacheKey, value: &[u8]) -> io::Result<()> {
    validate_value(value)?;
    wire::put(&instance.socket_path(), key, value).await
}

pub async fn git(instance: &Instance, query: &Query) -> io::Result<Option<Snapshot>> {
    let socket = instance.socket_path();
    match wire::git(&socket, query).await {
        Ok(snapshot) => Ok(snapshot),
        Err(first_error)
            if matches!(
                first_error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            spawn_daemon(instance)?;
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                match wire::git(&socket, query).await {
                    Ok(snapshot) => return Ok(snapshot),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(first_error)
        }
        Err(error) => Err(error),
    }
}

pub async fn clear(instance: &Instance) -> io::Result<()> {
    let socket = instance.socket_path();
    if wire::clear(&socket).await.is_ok() {
        return Ok(());
    }

    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if wire::clear(&socket).await.is_ok() {
            return Ok(());
        }
    }

    disk::clear_all()
}

pub async fn ensure_daemon(instance: &Instance) -> io::Result<()> {
    if wire::ping(&instance.socket_path()).await.is_ok() {
        return Ok(());
    }
    spawn_daemon(instance)
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
    let mut fingerprint = Fingerprint::new(b"ztheme-cache-identity-v1");
    fingerprint.add_bytes(b"package-version", env!("CARGO_PKG_VERSION").as_bytes());
    fingerprint.add_u64(b"cache-format-version", u64::from(CACHE_FORMAT_VERSION));
    fingerprint.add_u64(b"wire-version", u64::from(wire::VERSION));
    if let Ok(executable) = env::current_exe() {
        fingerprint.add_path(b"executable", &executable);
    }
    format!("{:016x}", fingerprint.finish().value())
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

fn u64_len(value: &[u8]) -> u64 {
    u64::try_from(value.len()).unwrap_or(u64::MAX)
}

fn user_id() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }

    // SAFETY: getuid takes no arguments and has no failure mode.
    unsafe { getuid() }
}
