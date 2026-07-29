use std::env;
use std::path::{Path, PathBuf};

pub fn worktree_root(cwd: &Path) -> Option<PathBuf> {
    if let Some(worktree) = env::var_os("GIT_WORK_TREE") {
        return Some(absolute(cwd, Path::new(&worktree)));
    }
    if env::var_os("GIT_DIR").is_some() {
        return None;
    }

    let mut directory = cwd;
    loop {
        let dot_git = directory.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(directory.to_path_buf());
        }
        directory = directory.parent()?;
    }
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}
