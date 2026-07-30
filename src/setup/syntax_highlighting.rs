use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::install::{
    InstallLock, TemporaryDirectory, data_root, download, require_command, run, verify_sha256,
};

const COMPONENT: &str = "zsh-syntax-highlighting";
const VERSION: &str = "0.8.0";
const REVISION: &str = "db085e4661f6aafd24e5acb5b2e17e4dd5dddf3e";
const SHA256: &str = "874999413d147b7ad6776077e1b756d31d68284f9ef2ef240b33a4c842f0e1d8";
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;

pub fn managed_script() -> PathBuf {
    managed_root().join("zsh-syntax-highlighting.zsh")
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
        .ok_or_else(|| io::Error::other("syntax highlighter destination has no parent"))?;
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
    data_root()
        .join("ztheme/zsh-syntax-highlighting")
        .join(VERSION)
}

fn confirm_install() -> io::Result<bool> {
    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return Ok(false);
    };
    write!(
        tty,
        "ztheme can provide themed command-line syntax highlighting with \
         zsh-syntax-highlighting {VERSION}.\n\
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
        .ok_or_else(|| io::Error::other("syntax highlighter destination has no parent"))?;
    let temporary = TemporaryDirectory::create(parent, COMPONENT)?;
    let archive = temporary.path().join("source.tar.gz");
    let url =
        format!("https://github.com/zsh-users/zsh-syntax-highlighting/archive/{REVISION}.tar.gz");

    download(&url, &archive, MAX_ARCHIVE_BYTES, COMPONENT)?;
    verify_sha256(&archive, SHA256, COMPONENT)?;
    run(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(temporary.path()),
        "extract zsh-syntax-highlighting",
    )?;

    let extracted = temporary
        .path()
        .join(format!("zsh-syntax-highlighting-{REVISION}"));
    if !installed(&extracted) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded zsh-syntax-highlighting archive is incomplete",
        ));
    }
    remove_existing(target)?;
    fs::rename(extracted, target)
}

fn installed(root: &Path) -> bool {
    is_directory(root)
        && is_file(&root.join("zsh-syntax-highlighting.zsh"))
        && is_file(&root.join("highlighters/main/main-highlighter.zsh"))
        && is_file(&root.join("COPYING.md"))
        && has_contents(&root.join(".version"), VERSION)
        && has_contents(&root.join(".revision-hash"), REVISION)
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
