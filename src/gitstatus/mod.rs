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
        Self::from_values(
            cwd,
            env::var_os("GIT_DIR").as_deref(),
            env::var_os("GIT_WORK_TREE").as_deref(),
        )
    }

    fn from_values(
        cwd: &Path,
        git_dir: Option<&std::ffi::OsStr>,
        worktree: Option<&std::ffi::OsStr>,
    ) -> io::Result<Self> {
        if git_dir.is_some() && worktree.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "gitstatusd cannot represent GIT_DIR together with GIT_WORK_TREE",
            ));
        }

        if let Some(git_dir) = git_dir {
            return Ok(Self::GitDir(absolute(cwd, Path::new(git_dir))));
        }
        if let Some(worktree) = worktree {
            return Ok(Self::Directory(absolute(cwd, Path::new(worktree))));
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::io;
    use std::path::Path;

    use super::Query;

    #[test]
    fn environment_values_select_the_correct_query_kind() {
        let cwd = Path::new("/work/project");

        let default = Query::from_values(cwd, None, None).unwrap();
        assert!(!default.is_git_dir());
        assert_eq!(default.path(), cwd);

        let git_dir = Query::from_values(cwd, Some(OsStr::new("../repo.git")), None).unwrap();
        assert!(git_dir.is_git_dir());
        assert_eq!(git_dir.path(), Path::new("/work/project/../repo.git"));

        let worktree = Query::from_values(cwd, None, Some(OsStr::new("checkout"))).unwrap();
        assert!(!worktree.is_git_dir());
        assert_eq!(worktree.path(), Path::new("/work/project/checkout"));
    }

    #[test]
    fn environment_values_reject_git_dir_with_worktree() {
        let error = Query::from_values(
            Path::new("/work"),
            Some(OsStr::new(".git")),
            Some(OsStr::new(".")),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
}
