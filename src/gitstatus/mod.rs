mod install;
mod process;

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
    /// Pure explicit selection, before any filesystem planning. Both supported
    /// environment selections bypass automatic discovery and its ceilings.
    pub(crate) fn explicit(
        cwd: &Path,
        environment: &crate::environment::PromptEnvironment,
    ) -> io::Result<Option<Self>> {
        Self::from_values(
            cwd,
            environment.git_dir.as_deref(),
            environment.git_work_tree.as_deref(),
        )
    }

    /// Called only for automatic selection, on a bounded filesystem worker.
    pub(crate) fn discover(
        cwd: &Path,
        environment: &crate::environment::PromptEnvironment,
    ) -> Option<Self> {
        crate::runtime::detect::repository_root(cwd, environment).map(Self::Directory)
    }

    fn from_values(
        cwd: &Path,
        git_dir: Option<&std::ffi::OsStr>,
        worktree: Option<&std::ffi::OsStr>,
    ) -> io::Result<Option<Self>> {
        if git_dir.is_some() && worktree.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "gitstatusd cannot represent GIT_DIR together with GIT_WORK_TREE",
            ));
        }

        if let Some(git_dir) = git_dir {
            return Ok(Some(Self::GitDir(absolute(cwd, Path::new(git_dir)))));
        }
        if let Some(worktree) = worktree {
            return Ok(Some(Self::Directory(absolute(cwd, Path::new(worktree)))));
        }
        Ok(None)
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

        assert!(Query::from_values(cwd, None, None).unwrap().is_none());

        let git_dir = Query::from_values(cwd, Some(OsStr::new("../repo.git")), None)
            .unwrap()
            .unwrap();
        assert!(git_dir.is_git_dir());
        assert_eq!(git_dir.path(), Path::new("/work/project/../repo.git"));

        let worktree = Query::from_values(cwd, None, Some(OsStr::new("checkout")))
            .unwrap()
            .unwrap();
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

    #[test]
    fn explicit_git_dir_bypasses_ceiling_and_filesystem_discovery() {
        let cwd = Path::new("/nonexistent/work/project");
        let mut environment = crate::environment::PromptEnvironment {
            git_dir: Some("../repo.git".into()),
            git_ceilings: Some("/nonexistent/work".into()),
            ..crate::environment::PromptEnvironment::default()
        };
        let query = Query::explicit(cwd, &environment).unwrap().unwrap();
        assert!(query.is_git_dir());
        assert_eq!(
            query.path(),
            Path::new("/nonexistent/work/project/../repo.git")
        );
        environment.git_dir = Some("/absolute/repo.git".into());
        assert_eq!(
            Query::explicit(cwd, &environment).unwrap().unwrap().path(),
            Path::new("/absolute/repo.git")
        );
        environment.git_work_tree = Some("checkout".into());
        assert_eq!(
            Query::explicit(cwd, &environment).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn explicit_worktree_bypasses_ceilings_and_nonexistent_cwd() {
        let cwd = Path::new("/nonexistent/work/project");
        let mut environment = crate::environment::PromptEnvironment {
            git_work_tree: Some("/selected/checkout".into()),
            git_ceilings: Some("/nonexistent/work".into()),
            ..crate::environment::PromptEnvironment::default()
        };
        let query = Query::explicit(cwd, &environment).unwrap().unwrap();
        assert!(!query.is_git_dir());
        assert_eq!(query.path(), Path::new("/selected/checkout"));
        environment.git_work_tree = Some("checkout".into());
        assert_eq!(
            Query::explicit(cwd, &environment).unwrap().unwrap().path(),
            Path::new("/nonexistent/work/project/checkout")
        );
    }
}
