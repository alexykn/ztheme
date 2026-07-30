use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

const LOCK_WAIT: Duration = Duration::from_mins(1);
const LOCK_POLL: Duration = Duration::from_millis(100);
const STALE_LOCK_AGE: Duration = Duration::from_mins(2);

pub struct InstallLock {
    path: PathBuf,
}

pub struct TemporaryDirectory {
    path: PathBuf,
}

impl InstallLock {
    pub fn acquire(path: &Path, ready: impl Fn() -> bool, component: &str) -> io::Result<Self> {
        let started = std::time::Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(mut lock) => {
                    writeln!(lock, "{}", std::process::id())?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if ready() {
                        return Ok(Self {
                            path: PathBuf::new(),
                        });
                    }
                    if is_stale(path) {
                        match fs::remove_file(path) {
                            Ok(()) => continue,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error),
                        }
                    }
                    if started.elapsed() >= LOCK_WAIT {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("timed out waiting for another {component} installation"),
                        ));
                    }
                    thread::sleep(LOCK_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "ztheme: cannot remove installation lock {}: {error}",
                self.path.display()
            );
        }
    }
}

impl TemporaryDirectory {
    pub fn create(parent: &Path, component: &str) -> io::Result<Self> {
        for sequence in 0..16 {
            let path = parent.join(format!(
                ".{component}-install-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("cannot create a unique {component} installation directory"),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "ztheme: cannot remove temporary directory {}: {error}",
                self.path.display()
            );
        }
    }
}

pub fn data_root() -> PathBuf {
    if let Some(root) = env::var_os("XDG_DATA_HOME") {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            return root;
        }
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map_or_else(
            || env::temp_dir().join(format!("ztheme-{}", user_id())),
            |home| home.join(".local/share"),
        )
}

pub fn download(
    url: &str,
    destination: &Path,
    maximum_bytes: u64,
    component: &str,
) -> io::Result<()> {
    require_command("curl", component)?;
    run(
        Command::new("curl")
            .args([
                "--proto",
                "=https",
                "--tlsv1.2",
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--connect-timeout",
                "5",
                "--max-time",
                "60",
                "--output",
            ])
            .arg(destination)
            .arg(url),
        &format!("download {component}"),
    )?;
    let size = destination.metadata()?.len();
    if size == 0 || size > maximum_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{component} archive has an invalid size"),
        ));
    }
    Ok(())
}

pub fn verify_sha256(path: &Path, expected: &str, component: &str) -> io::Result<()> {
    let actual = sha256(path)?;
    if actual == expected {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{component} checksum mismatch: expected {expected}, got {actual}"),
    ))
}

pub fn require_command(command: &str, component: &str) -> io::Result<()> {
    if find_in_path(command).is_some() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{command} is required to install {component}"),
    ))
}

pub fn run(command: &mut Command, description: &str) -> io::Result<()> {
    let status = command.status()?;
    if status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "failed to {description}: process exited with {status}"
    )))
}

fn sha256(path: &Path) -> io::Result<String> {
    for (program, arguments) in [("shasum", &["-a", "256"][..]), ("sha256sum", &[][..])] {
        let Ok(output) = Command::new(program).args(arguments).arg(path).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let output = String::from_utf8(output.stdout)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid SHA-256 output"))?;
        if let Some(hash) = output.split_whitespace().next()
            && hash.len() == 64
            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "neither shasum nor sha256sum is available",
    ))
}

fn find_in_path(command: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(command))
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn is_stale(path: &Path) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age >= STALE_LOCK_AGE)
}

fn user_id() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid has no preconditions and returns the real user ID.
    unsafe { getuid() }
}
