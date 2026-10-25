mod install;
mod process;

use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use install::{ensure_installed, managed_binary};
pub use process::Client;

/// Also the daemon's Git cache key: the query kind and its exact path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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

/// `[git]` in `config.toml`: the tunable `gitstatusd` flags. The theme schema
/// embeds this table so `init` and `reload` validate it; the daemon reads only
/// this table, when it starts `gitstatusd` and when `ztheme clear` or a timed
/// out query restarts it. Replacing a crashed `gitstatusd` reuses the
/// options it ran with.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    /// Worker threads that scan the working tree (`-t`). Unset selects
    /// `DEFAULT_MAX_THREADS` or the number of available cores when fewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<NonZeroU32>,
    /// Index size above which `gitstatusd` stops scanning for unstaged and
    /// untracked files (`-m`). Unset or negative means no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_max_index_size: Option<i64>,
}

/// Measured on an 8-core Apple M1 Pro (6 performance cores) against a
/// 100k-file repository with 50k changes: 6 threads answered fastest (p50
/// 145-190 ms against 180-215 ms with 4 and 175-250 ms with 8, whose busy-
/// machine outliers reached seconds) at 0.78 s of CPU per query against 0.85 s
/// with 8. Small repositories answer in about 2 ms at any thread count.
const DEFAULT_MAX_THREADS: u32 = 6;

/// The minimal `config.toml` view the daemon needs. Unlike the theme schema it
/// ignores every other table, so the daemon never depends on theme settings.
#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    git: Options,
}

impl Options {
    /// Reads `[git]` from `config.toml`; a missing file or table yields the
    /// defaults, while an unreadable or invalid one is an explicit error.
    pub fn load() -> io::Result<Self> {
        let Some(root) = crate::setup::install::config_root() else {
            return Ok(Self::default());
        };
        let path = root.join("ztheme/config.toml");
        match std::fs::read_to_string(&path) {
            Ok(source) => Self::parse(&source).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid [git] in {}: {error}", path.display()),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            )),
        }
    }

    fn parse(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str::<ConfigFile>(source).map(|config| config.git)
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn thread_count(&self) -> u32 {
        self.threads.map_or_else(
            || {
                std::thread::available_parallelism()
                    .map_or(1, |cores| u32::try_from(cores.get()).unwrap_or(u32::MAX))
                    .min(DEFAULT_MAX_THREADS)
            },
            NonZeroU32::get,
        )
    }

    /// The configurable `gitstatusd` arguments, `-t` and `-m`.
    fn arguments(&self) -> [String; 4] {
        [
            "-t".to_owned(),
            self.thread_count().to_string(),
            "-m".to_owned(),
            self.dirty_max_index_size.unwrap_or(-1).to_string(),
        ]
    }
}
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

    use super::{Options, Query};

    #[test]
    fn git_options_ignore_other_tables_and_reject_invalid_values() {
        let defaults =
            Options::parse("version = 1\ntheme = \"x\"\n[async.lock]\ngit_segment = false\n")
                .unwrap();
        assert_eq!(defaults, Options::default());
        assert_eq!(
            defaults.arguments()[3],
            "-1",
            "no index-size limit by default"
        );

        let configured =
            Options::parse("[git]\nthreads = 3\ndirty_max_index_size = 1000\n").unwrap();
        assert_eq!(configured.arguments(), ["-t", "3", "-m", "1000"]);

        assert!(Options::parse("[git]\nthreads = 0\n").is_err());
        assert!(Options::parse("[git]\nthread = 3\n").is_err());
    }

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
