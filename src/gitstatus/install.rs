use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const VERSION_SERIES: &str = "v1.5";
const ARTIFACT_VERSION: &str = "v1.5.4";
const MAX_ARCHIVE_BYTES: u64 = 16 * 1024 * 1024;
const LOCK_WAIT: Duration = Duration::from_mins(1);
const LOCK_POLL: Duration = Duration::from_millis(100);
const STALE_LOCK_AGE: Duration = Duration::from_mins(2);

struct Artifact {
    os: &'static str,
    arch: &'static str,
    upstream_os: &'static str,
    upstream_arch: &'static str,
    sha256: &'static str,
}

const ARTIFACTS: &[Artifact] = &[
    Artifact {
        os: "macos",
        arch: "aarch64",
        upstream_os: "darwin",
        upstream_arch: "arm64",
        sha256: "eae979e990ca37c56ee39fadd0c3f392cbbd0c6bdfb9a603010be60d9e48910a",
    },
    Artifact {
        os: "macos",
        arch: "x86_64",
        upstream_os: "darwin",
        upstream_arch: "x86_64",
        sha256: "9fd3913ec1b6b856ab6e08a99a2343f0e8e809eb6b62ca4b0963163656c668e6",
    },
    Artifact {
        os: "linux",
        arch: "aarch64",
        upstream_os: "linux",
        upstream_arch: "aarch64",
        sha256: "32b57eb28bf6d80b280e4020a0045184f8ca897b20b570c12948aa6838673225",
    },
    Artifact {
        os: "linux",
        arch: "x86_64",
        upstream_os: "linux",
        upstream_arch: "x86_64",
        sha256: "9633816e7832109e530c9e2532b11a1edae08136d63aa7e40246c0339b7db304",
    },
];

pub fn managed_binary() -> PathBuf {
    data_root().join("ztheme/gitstatus/v1.5/gitstatusd")
}

pub fn ensure_installed(assume_yes: bool) -> io::Result<bool> {
    let target = managed_binary();
    if is_executable(&target) {
        return Ok(true);
    }

    install_missing(&target, assume_yes)
}

fn install_missing(target: &Path, assume_yes: bool) -> io::Result<bool> {
    let artifact = artifact()?;
    if let Some(existing) = find_existing(artifact)
        && compatible(&existing)
    {
        install_copy(&existing, target)?;
        return Ok(true);
    }

    if !assume_yes && !confirm_install()? {
        return Ok(false);
    }

    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("gitstatusd destination has no parent"))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let lock = InstallLock::acquire(&parent.join(".install.lock"), target)?;
    if is_executable(target) {
        return Ok(true);
    }
    download_artifact(artifact, target)?;
    drop(lock);
    Ok(true)
}

fn artifact() -> io::Result<&'static Artifact> {
    ARTIFACTS
        .iter()
        .find(|artifact| artifact.os == env::consts::OS && artifact.arch == env::consts::ARCH)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "no pinned gitstatusd artifact for {}-{}",
                    env::consts::OS,
                    env::consts::ARCH
                ),
            )
        })
}

fn find_existing(artifact: &Artifact) -> Option<PathBuf> {
    let filename = artifact.filename();
    find_in_path("gitstatusd").or_else(|| {
        let mut prefixes = Vec::with_capacity(3);
        if let Some(prefix) = env::var_os("HOMEBREW_PREFIX") {
            prefixes.push(PathBuf::from(prefix));
        }
        prefixes.push(PathBuf::from("/opt/homebrew"));
        prefixes.push(PathBuf::from("/usr/local"));
        prefixes
            .into_iter()
            .map(|prefix| prefix.join("opt/gitstatus/usrbin").join(&filename))
            .find(|path| is_executable(path))
    })
}

fn find_in_path(command: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(command))
        .find(|path| is_executable(path))
}

fn compatible(binary: &Path) -> bool {
    Command::new(binary)
        .args(["-G", "v1.5.*", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn confirm_install() -> io::Result<bool> {
    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return Ok(false);
    };
    write!(
        tty,
        "ztheme requires gitstatusd {VERSION_SERIES}.\n\
         Install the pinned gitstatusd binary now? [y/N] "
    )?;
    tty.flush()?;
    let mut answer = String::new();
    BufReader::new(tty).read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn install_copy(source: &Path, target: &Path) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("gitstatusd destination has no parent"))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = parent.join(format!(".gitstatusd-copy-{}", std::process::id()));
    let result = (|| {
        fs::copy(source, &temporary)?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))?;
        if !compatible(&temporary) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "installed gitstatusd is incompatible",
            ));
        }
        fs::rename(&temporary, target)
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn download_artifact(artifact: &Artifact, target: &Path) -> io::Result<()> {
    require_command("curl")?;
    require_command("tar")?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("gitstatusd destination has no parent"))?;
    let temporary = create_temporary_directory(parent)?;
    let filename = artifact.filename();
    let archive = temporary.join("gitstatusd.tar.gz");
    let url = format!(
        "https://github.com/romkatv/gitstatus/releases/download/{ARTIFACT_VERSION}/{filename}.tar.gz"
    );

    let result = (|| {
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
                .arg(&archive)
                .arg(&url),
            "download gitstatusd",
        )?;
        let size = archive.metadata()?.len();
        if size == 0 || size > MAX_ARCHIVE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gitstatusd archive has an invalid size",
            ));
        }
        let actual = sha256(&archive)?;
        if actual != artifact.sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "gitstatusd checksum mismatch: expected {}, got {actual}",
                    artifact.sha256
                ),
            ));
        }
        run(
            Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&temporary)
                .arg(&filename),
            "extract gitstatusd",
        )?;
        let extracted = temporary.join(filename);
        fs::set_permissions(&extracted, fs::Permissions::from_mode(0o700))?;
        if !compatible(&extracted) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "downloaded gitstatusd is incompatible",
            ));
        }
        fs::rename(extracted, target)
    })();
    let _ = fs::remove_dir_all(&temporary);
    result
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

fn require_command(command: &str) -> io::Result<()> {
    find_in_path(command).map_or_else(
        || {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{command} is required to install gitstatusd"),
            ))
        },
        |_| Ok(()),
    )
}

fn run(command: &mut Command, description: &'static str) -> io::Result<()> {
    let status = command.status()?;
    if status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "failed to {description}: process exited with {status}"
    )))
}

fn create_temporary_directory(parent: &Path) -> io::Result<PathBuf> {
    for sequence in 0..16 {
        let path = parent.join(format!(".install-{}-{sequence}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot create a unique gitstatusd installation directory",
    ))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn data_root() -> PathBuf {
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

fn user_id() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid has no preconditions and returns the real user ID.
    unsafe { getuid() }
}

impl Artifact {
    fn filename(&self) -> String {
        format!("gitstatusd-{}-{}", self.upstream_os, self.upstream_arch)
    }
}

struct InstallLock {
    path: PathBuf,
}

impl InstallLock {
    fn acquire(path: &Path, target: &Path) -> io::Result<Self> {
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
                    if is_executable(target) {
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
                            "timed out waiting for another gitstatusd installation",
                        ));
                    }
                    thread::sleep(LOCK_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn is_stale(path: &Path) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age >= STALE_LOCK_AGE)
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}
