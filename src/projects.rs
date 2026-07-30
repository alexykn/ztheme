use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cache::Fingerprint;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Runtime {
    Python,
    Perl,
    Java,
    Kotlin,
    Scala,
    Rust,
    Go,
    Bun,
    Deno,
    Node,
    Ruby,
    Php,
    Dotnet,
    C,
    Cpp,
    Swift,
    Lua,
}

#[derive(Clone)]
pub struct Project {
    pub cwd: PathBuf,
    pub runtimes: Vec<Runtime>,
    pub fingerprint: Fingerprint,
}

pub fn detect(cwd: &Path, git_root: Option<&Path>) -> Project {
    let mut fingerprint = Fingerprint::new(b"ztheme-project-v1");
    let mut runtimes = HashSet::new();
    let home = env::var_os("HOME").map(PathBuf::from);
    let ceilings = git_ceilings();
    let mut directory = cwd.to_path_buf();
    let mut javascript = None;

    fingerprint.add_path(b"cwd", cwd);
    for variable in [
        "PATH",
        "VIRTUAL_ENV",
        "CONDA_PREFIX",
        "CONDA_DEFAULT_ENV",
        "PERLBREW_PERL",
        "PLENV_VERSION",
        "RUSTUP_TOOLCHAIN",
        "RBENV_VERSION",
        "RUBY_VERSION",
    ] {
        let value = env::var_os(variable);
        fingerprint.add_bytes(b"environment-name", variable.as_bytes());
        fingerprint.add_optional_os(b"environment-value", value.as_deref());
    }

    if env::var_os("VIRTUAL_ENV").is_some() || env::var_os("CONDA_PREFIX").is_some() {
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
        fingerprint.add_path(b"scanned-directory", &directory);
        hash_version_selectors(&directory, &names, &mut fingerprint);

        if javascript.is_none() {
            javascript = detect_javascript(&names);
        }

        detect_project_extensions(&names, &mut runtimes);
        if names.contains(OsStr::new("project"))
            && directory.join("project/build.properties").is_file()
        {
            runtimes.insert(Runtime::Scala);
            hash_metadata(
                &directory.join("project/build.properties"),
                &mut fingerprint,
            );
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
        fingerprint.add_u64(b"runtime", u64::from(runtime.id()));
    }
    Project {
        cwd: cwd.to_path_buf(),
        runtimes,
        fingerprint,
    }
}

impl Runtime {
    pub const ALL: [Self; 17] = [
        Self::Python,
        Self::Perl,
        Self::Java,
        Self::Kotlin,
        Self::Scala,
        Self::Rust,
        Self::Go,
        Self::Bun,
        Self::Deno,
        Self::Node,
        Self::Ruby,
        Self::Php,
        Self::Dotnet,
        Self::C,
        Self::Cpp,
        Self::Swift,
        Self::Lua,
    ];

    pub const fn id(self) -> u8 {
        self as u8
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Perl => "perl",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Scala => "scala",
            Self::Rust => "rust",
            Self::Go => "go",
            Self::Bun => "bun",
            Self::Deno => "deno",
            Self::Node => "node",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Dotnet => "dotnet",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Swift => "swift",
            Self::Lua => "lua",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "python" => Some(Self::Python),
            "perl" => Some(Self::Perl),
            "java" => Some(Self::Java),
            "kotlin" => Some(Self::Kotlin),
            "scala" => Some(Self::Scala),
            "rust" => Some(Self::Rust),
            "go" => Some(Self::Go),
            "bun" => Some(Self::Bun),
            "deno" => Some(Self::Deno),
            "node" => Some(Self::Node),
            "ruby" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "dotnet" => Some(Self::Dotnet),
            "c" => Some(Self::C),
            "cpp" => Some(Self::Cpp),
            "swift" => Some(Self::Swift),
            "lua" => Some(Self::Lua),
            _ => None,
        }
    }

    pub const fn from_id(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Python),
            1 => Some(Self::Perl),
            2 => Some(Self::Java),
            3 => Some(Self::Kotlin),
            4 => Some(Self::Scala),
            5 => Some(Self::Rust),
            6 => Some(Self::Go),
            7 => Some(Self::Bun),
            8 => Some(Self::Deno),
            9 => Some(Self::Node),
            10 => Some(Self::Ruby),
            11 => Some(Self::Php),
            12 => Some(Self::Dotnet),
            13 => Some(Self::C),
            14 => Some(Self::Cpp),
            15 => Some(Self::Swift),
            16 => Some(Self::Lua),
            _ => None,
        }
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

fn git_ceilings() -> HashSet<PathBuf> {
    env::var_os("GIT_CEILING_DIRECTORIES")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default()
}

fn hash_version_selectors(
    directory: &Path,
    names: &HashSet<OsString>,
    fingerprint: &mut Fingerprint,
) {
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
            fingerprint.add_bytes(b"selector-name", marker.as_bytes());
            hash_metadata(&directory.join(marker), fingerprint);
        }
    }
}

fn hash_metadata(path: &Path, fingerprint: &mut Fingerprint) {
    fingerprint.add_path(b"metadata-path", path);
    let metadata = path.metadata().ok();
    fingerprint.add_metadata(b"metadata", metadata.as_ref());
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Runtime, detect};

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

        let project = detect(&nested, Some(directory.path()));
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

        let project = detect(&nested, Some(directory.path()));
        assert!(project.runtimes.contains(&Runtime::Node));
        assert!(!project.runtimes.contains(&Runtime::Bun));
    }

    #[test]
    fn cpp_source_suppresses_the_redundant_c_runtime() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("main.c"), b"").unwrap();
        fs::write(directory.path().join("main.cpp"), b"").unwrap();

        let project = detect(directory.path(), Some(directory.path()));
        assert!(project.runtimes.contains(&Runtime::Cpp));
        assert!(!project.runtimes.contains(&Runtime::C));
    }

    #[test]
    fn selector_changes_invalidate_the_project_fingerprint() {
        let directory = TestDirectory::new();
        let before = detect(directory.path(), Some(directory.path()))
            .fingerprint
            .finish();
        fs::write(directory.path().join(".python-version"), b"3.14\n").unwrap();
        let after = detect(directory.path(), Some(directory.path()))
            .fingerprint
            .finish();

        assert_ne!(before, after);
    }
}
