use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use super::Runtime;
use crate::environment::PromptEnvironment;
use crate::utils::HashBuilder;

#[derive(Clone)]
pub(crate) struct Project {
    pub(crate) cwd: PathBuf,
    pub(crate) runtimes: Vec<Runtime>,
    pub(super) hash: HashBuilder,
}

pub(crate) fn worktree_root(cwd: &Path, environment: &PromptEnvironment) -> Option<PathBuf> {
    if let Some(worktree) = environment.git_work_tree.as_deref() {
        return Some(absolute(cwd, Path::new(worktree)));
    }
    if environment.git_dir.is_some() {
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

pub(crate) fn detect(
    cwd: &Path,
    git_root: Option<&Path>,
    environment: &PromptEnvironment,
) -> Project {
    let mut hash = HashBuilder::new(b"ztheme-project-v1");
    let mut runtimes = HashSet::new();
    let home = environment.home.as_deref().map(PathBuf::from);
    let ceilings = environment
        .git_ceilings
        .as_deref()
        .map(env::split_paths)
        .into_iter()
        .flatten()
        .collect::<HashSet<_>>();
    let mut directory = cwd.to_path_buf();
    let mut javascript = None;

    hash.add_path(b"cwd", cwd);
    environment.add_runtime_fingerprint(&mut hash);

    if environment.virtual_env.is_some() || environment.conda_prefix.is_some() {
        runtimes.insert(Runtime::Python);
    }

    for depth in 0..32 {
        if depth > 0
            && (home.as_deref() == Some(directory.as_path())
                || ceilings.contains(directory.as_path()))
        {
            break;
        }

        let names = directory_names(&directory);
        detect_markers(&names, &mut runtimes);
        hash.add_path(b"scanned-directory", &directory);
        hash_version_selectors(&directory, &names, &mut hash);

        if javascript.is_none() {
            javascript = detect_javascript(&names);
        }

        detect_project_extensions(&names, &mut runtimes);
        if names.contains(OsStr::new("project"))
            && directory.join("project/build.properties").is_file()
        {
            runtimes.insert(Runtime::Scala);
            hash_metadata(&directory.join("project/build.properties"), &mut hash);
        }

        if depth == 0 {
            detect_source_extensions(&names, &mut runtimes);
        }

        if git_root == Some(directory.as_path()) {
            break;
        }

        let Some(parent) = directory.parent() else {
            break;
        };
        directory = parent.to_path_buf();
    }

    if let Some(runtime) = javascript {
        runtimes.insert(runtime);
    }

    if runtimes.contains(&Runtime::Cpp) {
        runtimes.remove(&Runtime::C);
    }

    let mut runtimes: Vec<_> = runtimes.into_iter().collect();
    runtimes.sort_unstable();
    for runtime in &runtimes {
        hash.add_u64(b"runtime", u64::from(runtime.id()));
    }
    Project {
        cwd: cwd.to_path_buf(),
        runtimes,
        hash,
    }
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn directory_names(directory: &Path) -> HashSet<OsString> {
    let Ok(entries) = fs::read_dir(directory) else {
        return HashSet::new();
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect()
}

fn detect_markers(names: &HashSet<OsString>, runtimes: &mut HashSet<Runtime>) {
    if has_any(
        names,
        &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "poetry.lock",
            "uv.lock",
            "tox.ini",
            ".python-version",
            "__init__.py",
        ],
    ) {
        runtimes.insert(Runtime::Python);
    }

    if has_any(
        names,
        &[
            "Makefile.PL",
            "Build.PL",
            "cpanfile",
            "cpanfile.snapshot",
            "META.json",
            "META.yml",
            "dist.ini",
            ".perl-version",
        ],
    ) {
        runtimes.insert(Runtime::Perl);
    }

    if has_any(
        names,
        &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "gradlew",
            ".java-version",
            ".sdkmanrc",
        ],
    ) {
        runtimes.insert(Runtime::Java);
    }

    if has_any(names, &["build.gradle.kts", "settings.gradle.kts"]) {
        runtimes.insert(Runtime::Kotlin);
    }

    if has_any(
        names,
        &[
            "build.sbt",
            "build.properties",
            ".scalaenv",
            ".sbtenv",
            ".metals",
        ],
    ) {
        runtimes.insert(Runtime::Scala);
    }

    if has_any(
        names,
        &[
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain",
            "rust-toolchain.toml",
        ],
    ) {
        runtimes.insert(Runtime::Rust);
    }

    if has_any(names, &["go.mod", "go.work"]) {
        runtimes.insert(Runtime::Go);
    }

    if has_any(names, &["Gemfile", "Rakefile", ".ruby-version"]) {
        runtimes.insert(Runtime::Ruby);
    }

    if has_any(names, &["composer.json", "composer.lock", ".php-version"]) {
        runtimes.insert(Runtime::Php);
    }

    if has_any(
        names,
        &[
            "global.json",
            "Directory.Build.props",
            "Directory.Build.targets",
            "Directory.Packages.props",
        ],
    ) {
        runtimes.insert(Runtime::Dotnet);
    }

    if has_any(names, &["Package.swift"]) {
        runtimes.insert(Runtime::Swift);
    }

    if has_any(names, &[".luarc.json", ".luacheckrc", ".lua-version"]) {
        runtimes.insert(Runtime::Lua);
    }
}

fn detect_javascript(names: &HashSet<OsString>) -> Option<Runtime> {
    if has_any(names, &["bun.lock", "bun.lockb", "bunfig.toml"]) {
        return Some(Runtime::Bun);
    }

    if has_any(
        names,
        &["deno.json", "deno.jsonc", "deno.lock", "mod.ts", "deps.ts"],
    ) {
        return Some(Runtime::Deno);
    }

    if has_any(
        names,
        &[
            "package.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            ".nvmrc",
            ".node-version",
            "node_modules",
        ],
    ) {
        return Some(Runtime::Node);
    }

    None
}

fn detect_project_extensions(names: &HashSet<OsString>, runtimes: &mut HashSet<Runtime>) {
    for name in names {
        let path = Path::new(name);
        let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();

        match extension {
            "csproj" | "fsproj" | "vbproj" | "sln" | "slnx" => {
                runtimes.insert(Runtime::Dotnet);
            }
            "xcodeproj" | "xcworkspace" => {
                runtimes.insert(Runtime::Swift);
            }
            "rockspec" => {
                runtimes.insert(Runtime::Lua);
            }
            _ => {}
        }
    }
}

fn detect_source_extensions(names: &HashSet<OsString>, runtimes: &mut HashSet<Runtime>) {
    for name in names {
        match Path::new(name)
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
        {
            "cpp" | "cc" | "cxx" | "hpp" | "hh" => {
                runtimes.insert(Runtime::Cpp);
            }
            "c" | "h" => {
                runtimes.insert(Runtime::C);
            }
            _ => {}
        }
    }
}

fn has_any(names: &HashSet<OsString>, markers: &[&str]) -> bool {
    markers
        .iter()
        .any(|marker| names.contains(OsStr::new(marker)))
}

fn hash_version_selectors(directory: &Path, names: &HashSet<OsString>, hash: &mut HashBuilder) {
    for marker in [
        ".python-version",
        ".perl-version",
        ".java-version",
        ".sdkmanrc",
        ".scalaenv",
        ".sbtenv",
        "rust-toolchain",
        "rust-toolchain.toml",
        ".nvmrc",
        ".node-version",
        ".ruby-version",
        ".php-version",
        "global.json",
        ".lua-version",
    ] {
        if names.contains(OsStr::new(marker)) {
            hash.add_bytes(b"selector-name", marker.as_bytes());
            hash_metadata(&directory.join(marker), hash);
        }
    }
}

fn hash_metadata(path: &Path, hash: &mut HashBuilder) {
    hash.add_path(b"metadata-path", path);
    let metadata = path.metadata().ok();
    hash.add_metadata(b"metadata", metadata.as_ref());
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Runtime, detect};
    use crate::environment::PromptEnvironment;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ztheme-project-test-{}-{sequence}",
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

    #[test]
    fn detects_parent_markers_until_the_git_root() {
        let directory = TestDirectory::new();
        let nested = directory.path().join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::write(directory.path().join("Cargo.toml"), b"[package]\n").unwrap();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();

        let project = detect(
            &nested,
            Some(directory.path()),
            &PromptEnvironment::default(),
        );
        assert!(project.runtimes.contains(&Runtime::Rust));
        assert!(project.runtimes.contains(&Runtime::Python));
    }

    #[test]
    fn nearest_javascript_ecosystem_wins() {
        let directory = TestDirectory::new();
        let nested = directory.path().join("app");
        fs::create_dir(&nested).unwrap();
        fs::write(directory.path().join("bun.lock"), b"").unwrap();
        fs::write(nested.join("package.json"), b"{}").unwrap();

        let project = detect(
            &nested,
            Some(directory.path()),
            &PromptEnvironment::default(),
        );
        assert!(project.runtimes.contains(&Runtime::Node));
        assert!(!project.runtimes.contains(&Runtime::Bun));
    }

    #[test]
    fn cpp_source_suppresses_the_redundant_c_runtime() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("main.c"), b"").unwrap();
        fs::write(directory.path().join("main.cpp"), b"").unwrap();

        let project = detect(
            directory.path(),
            Some(directory.path()),
            &PromptEnvironment::default(),
        );
        assert!(project.runtimes.contains(&Runtime::Cpp));
        assert!(!project.runtimes.contains(&Runtime::C));
    }

    #[test]
    fn selector_changes_invalidate_the_project_fingerprint() {
        let directory = TestDirectory::new();
        let before = detect(
            directory.path(),
            Some(directory.path()),
            &PromptEnvironment::default(),
        )
        .hash
        .finish();
        fs::write(directory.path().join(".python-version"), b"3.14\n").unwrap();
        let after = detect(
            directory.path(),
            Some(directory.path()),
            &PromptEnvironment::default(),
        )
        .hash
        .finish();

        assert_ne!(before, after);
    }
}
