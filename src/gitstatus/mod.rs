mod install;
mod process;

use std::env;
use std::io;
use std::path::{Path, PathBuf};

pub use install::{ensure_installed, managed_binary};
pub use process::Client;

#[derive(Clone, Debug)]
pub enum Query {
    Directory(PathBuf),
    GitDir(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub worktree: PathBuf,
    pub oid: String,
    pub branch: String,
    pub action: String,
    pub ahead: u64,
    pub behind: u64,
    pub stashes: u64,
    pub changes: u8,
}

pub const CONFLICTED: u8 = 1 << 0;
pub const DELETED: u8 = 1 << 1;
pub const STAGED: u8 = 1 << 2;
pub const UNSTAGED: u8 = 1 << 3;
pub const UNTRACKED: u8 = 1 << 4;

impl Query {
    pub fn from_environment(cwd: &Path) -> io::Result<Self> {
        let git_dir = env::var_os("GIT_DIR");
        let worktree = env::var_os("GIT_WORK_TREE");
        if git_dir.is_some() && worktree.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "gitstatusd cannot represent GIT_DIR together with GIT_WORK_TREE",
            ));
        }

        if let Some(git_dir) = git_dir {
            return Ok(Self::GitDir(absolute(cwd, Path::new(&git_dir))));
        }
        if let Some(worktree) = worktree {
            return Ok(Self::Directory(absolute(cwd, Path::new(&worktree))));
        }
        Ok(Self::Directory(cwd.to_path_buf()))
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Directory(path) | Self::GitDir(path) => path,
        }
    }

    pub const fn is_git_dir(&self) -> bool {
        matches!(self, Self::GitDir(_))
    }
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}
