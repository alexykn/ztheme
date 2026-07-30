use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::{
    InstallLock, TemporaryDirectory, data_root, download, require_command, run, verify_sha256,
};

const COMPONENT: &str = "zsh-autosuggestions";
const VERSION: &str = "0.7.1";
const UPSTREAM_VERSION: &str = "v0.7.1";
const REVISION: &str = "e52ee8ca55bcc56a17c828767a3f98f22a68d4eb";
const SHA256: &str = "166fad9326c99904fd3b16b5d770efb4f5df39a674599e72bba8fc41636ab065";
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;
const REVISION_FILE: &str = ".ztheme-revision";

pub fn managed_script() -> PathBuf {
    managed_root().join("zsh-autosuggestions.zsh")
}

pub fn ensure_installed(assume_yes: bool) -> io::Result<bool> {
    let target = managed_root();
    if installed(&target) {
        return Ok(true);
    }
    if !assume_yes && !confirm_install()? {
        return Ok(false);
    }

    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("autosuggestions destination has no parent"))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

    let lock = InstallLock::acquire(
        &parent.join(".install.lock"),
        || installed(&target),
        COMPONENT,
    )?;
    if installed(&target) {
        return Ok(true);
    }

    install(&target)?;
    drop(lock);
    Ok(true)
}

fn managed_root() -> PathBuf {
    data_root().join("ztheme/zsh-autosuggestions").join(VERSION)
}

fn confirm_install() -> io::Result<bool> {
    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return Ok(false);
    };
    write!(
        tty,
        "ztheme can provide asynchronous command suggestions with \
         zsh-autosuggestions {VERSION}.\n\
         Install the pinned managed copy now? [y/N] "
    )?;
    tty.flush()?;
    let mut answer = String::new();
    BufReader::new(tty).read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn install(target: &Path) -> io::Result<()> {
    require_command("tar", COMPONENT)?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("autosuggestions destination has no parent"))?;
    let temporary = TemporaryDirectory::create(parent, COMPONENT)?;
    let archive = temporary.path().join("source.tar.gz");
    let url = format!("https://github.com/zsh-users/zsh-autosuggestions/archive/{REVISION}.tar.gz");

    download(&url, &archive, MAX_ARCHIVE_BYTES, COMPONENT)?;
    verify_sha256(&archive, SHA256, COMPONENT)?;
    run(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(temporary.path()),
        "extract zsh-autosuggestions",
    )?;

    let extracted = temporary
        .path()
        .join(format!("zsh-autosuggestions-{REVISION}"));
    if !archive_complete(&extracted) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded zsh-autosuggestions archive is incomplete",
        ));
    }
    fs::write(extracted.join(REVISION_FILE), format!("{REVISION}\n"))?;
    remove_existing(target)?;
    fs::rename(extracted, target)
}

fn installed(root: &Path) -> bool {
    archive_complete(root) && has_contents(&root.join(REVISION_FILE), REVISION)
}

fn archive_complete(root: &Path) -> bool {
    is_directory(root)
        && is_file(&root.join("zsh-autosuggestions.zsh"))
        && is_file(&root.join("src/start.zsh"))
        && is_file(&root.join("LICENSE"))
        && has_contents(&root.join("VERSION"), UPSTREAM_VERSION)
}

fn is_directory(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn is_file(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
}

fn has_contents(path: &Path, expected: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| contents.trim() == expected)
}

fn remove_existing(path: &Path) -> io::Result<()> {
    let Ok(metadata) = path.symlink_metadata() else {
        return Ok(());
    };
    if metadata.file_type().is_dir() {
        return fs::remove_dir_all(path);
    }
    fs::remove_file(path)
}
