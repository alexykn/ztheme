use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
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
    if !url.starts_with("https://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{component} download URL is not HTTPS"),
        ));
    }
    // The agent enforces HTTPS on every request, including redirect hops, so
    // an HTTPS source cannot be downgraded to a plaintext transfer.
    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_mins(1)))
        .build()
        .new_agent();
    download_inner(&agent, url, destination, maximum_bytes, component)
}

/// Performs the bounded transfer. The caller owns the agent so the HTTPS-only
/// policy cannot be bypassed while the transfer logic stays testable against a
/// local plain-HTTP server.
fn download_inner(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    maximum_bytes: u64,
    component: &str,
) -> io::Result<()> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| io::Error::other(format!("failed to download {component}: {error}")))?;

    // Stream at most maximum_bytes + 1 bytes: the extra byte is the signal
    // that the archive exceeds the cap, so the destination never grows more
    // than one byte beyond the limit during the transfer.
    let copied = {
        let mut destination_file = fs::File::create(destination)?;
        let mut body = response
            .body_mut()
            .as_reader()
            .take(maximum_bytes.saturating_add(1));
        io::copy(&mut body, &mut destination_file)
            .map_err(|error| io::Error::other(format!("failed to download {component}: {error}")))?
    };
    // Defense in depth: re-validate the written file after the transfer.
    let size = destination.metadata()?.len();
    if copied > maximum_bytes || size == 0 || size > maximum_bytes {
        let _ = fs::remove_file(destination);
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
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(fs::read(path)?);
    let mut output = String::with_capacity(64);
    for byte in &digest {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(output)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Read as _, Write as _};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{TemporaryDirectory, download, download_inner, verify_sha256};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ztheme-install-test-{}-{sequence}",
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

    /// Serves one HTTP response over a local listener and returns its URL.
    fn serve_once(payload: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 4096];
            let mut consumed = 0;
            while consumed < request.len()
                && !request[..consumed]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
            {
                let Ok(read) = stream.read(&mut request[consumed..]) else {
                    return;
                };
                if read == 0 {
                    return;
                }
                consumed += read;
            }
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(&payload);
        });
        format!("http://{address}/archive")
    }

    /// An agent without the HTTPS-only policy, for exercising the transfer
    /// logic against the local plain-HTTP test server.
    fn plain_agent() -> ureq::Agent {
        ureq::Agent::config_builder().build().new_agent()
    }

    #[test]
    fn download_is_bounded_and_leaves_no_partial_destination() {
        let directory = TestDirectory::new();
        let agent = plain_agent();
        let maximum = 64 * 1024;

        let valid: Vec<u8> = (0..256).map(|index| u8::try_from(index).unwrap()).collect();
        let destination = directory.path().join("valid.bin");
        download_inner(
            &agent,
            &serve_once(valid.clone()),
            &destination,
            maximum,
            "fixture",
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), valid);

        let destination = directory.path().join("oversized.bin");
        let error = download_inner(
            &agent,
            &serve_once(vec![b'x'; usize::try_from(maximum + 100).unwrap()]),
            &destination,
            maximum,
            "fixture",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!destination.exists());

        let destination = directory.path().join("empty.bin");
        let error = download_inner(
            &agent,
            &serve_once(Vec::new()),
            &destination,
            maximum,
            "fixture",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!destination.exists());
    }

    #[test]
    fn download_rejects_non_https_urls() {
        let directory = TestDirectory::new();
        let destination = directory.path().join("http.bin");

        let error =
            download("http://127.0.0.1:1/archive", &destination, 1024, "fixture").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!destination.exists());
    }

    #[test]
    fn checksum_verification_accepts_only_the_expected_digest() {
        let directory = TestDirectory::new();
        let path = directory.path().join("archive");
        fs::write(&path, b"hello\n").unwrap();

        verify_sha256(
            &path,
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03",
            "fixture",
        )
        .unwrap();
        assert!(verify_sha256(&path, &"0".repeat(64), "fixture").is_err());
    }

    #[test]
    fn temporary_install_directories_are_private_and_cleaned_up() {
        let directory = TestDirectory::new();
        let temporary_path;
        {
            let temporary = TemporaryDirectory::create(directory.path(), "fixture").unwrap();
            temporary_path = temporary.path().to_path_buf();
            assert_eq!(
                fs::metadata(&temporary_path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(!temporary_path.exists());
    }
}
