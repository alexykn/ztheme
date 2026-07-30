use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::setup::install::{
    InstallLock, TemporaryDirectory, data_root, download, require_command, run, verify_sha256,
};

const VERSION_SERIES: &str = "v1.5";
const ARTIFACT_VERSION: &str = "v1.5.4";
const MAX_ARCHIVE_BYTES: u64 = 16 * 1024 * 1024;

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
    let lock = InstallLock::acquire(
        &parent.join(".install.lock"),
        || is_executable(target),
        "gitstatusd",
    )?;
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
    require_command("tar", "gitstatusd")?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("gitstatusd destination has no parent"))?;
    let temporary = TemporaryDirectory::create(parent, "gitstatusd")?;
    let filename = artifact.filename();
    let archive = temporary.path().join("gitstatusd.tar.gz");
    let url = format!(
        "https://github.com/romkatv/gitstatus/releases/download/{ARTIFACT_VERSION}/{filename}.tar.gz"
    );

    download(&url, &archive, MAX_ARCHIVE_BYTES, "gitstatusd")?;
    verify_sha256(&archive, artifact.sha256, "gitstatusd")?;
    run(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(temporary.path())
            .arg(&filename),
        "extract gitstatusd",
    )?;
    let extracted = temporary.path().join(filename);
    fs::set_permissions(&extracted, fs::Permissions::from_mode(0o700))?;
    if !compatible(&extracted) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded gitstatusd is incompatible",
        ));
    }
    fs::rename(extracted, target)
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

impl Artifact {
    fn filename(&self) -> String {
        format!("gitstatusd-{}-{}", self.upstream_os, self.upstream_arch)
    }
}
