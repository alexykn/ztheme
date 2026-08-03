use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use super::Runtime;
use crate::environment::PromptEnvironment;

#[derive(Clone)]
pub(crate) struct Project {
    pub(crate) cwd: PathBuf,
    pub(crate) runtimes: Vec<Runtime>,
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
    configured: &[Runtime],
    environment: &PromptEnvironment,
) -> Project {
    let mut runtimes = HashSet::new();
    let configured = configured.iter().copied().collect::<HashSet<_>>();
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

    if (environment.virtual_env.is_some() || environment.conda_prefix.is_some())
        && configured.contains(&Runtime::Python)
    {
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

        if javascript.is_none() {
            javascript = detect_javascript(&names);
        }

        detect_project_extensions(&names, &mut runtimes);
        if names.contains(OsStr::new("project"))
            && directory.join("project/build.properties").is_file()
        {
            runtimes.insert(Runtime::Scala);
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

    runtimes.retain(|runtime| configured.contains(runtime));
    let mut runtimes: Vec<_> = runtimes.into_iter().collect();
    runtimes.sort_unstable();
    Project {
        cwd: cwd.to_path_buf(),
        runtimes,
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

/// Marker files that identify a project as using a runtime. Detection is a
/// set membership test: any single marker selects the runtime. Kept as data
/// so the detection loop stays a small linear scan over the table.
const RUNTIME_MARKERS: &[(Runtime, &[&str])] = &[
    (
        Runtime::Python,
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
    ),
    (
        Runtime::Perl,
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
    ),
    (
        Runtime::Java,
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
    ),
    (
        Runtime::Kotlin,
        &["build.gradle.kts", "settings.gradle.kts"],
    ),
    (
        Runtime::Scala,
        &[
            "build.sbt",
            "build.properties",
            ".scalaenv",
            ".sbtenv",
            ".metals",
        ],
    ),
    (
        Runtime::Rust,
        &[
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain",
            "rust-toolchain.toml",
        ],
    ),
    (Runtime::Go, &["go.mod", "go.work"]),
    (Runtime::Ruby, &["Gemfile", "Rakefile", ".ruby-version"]),
    (
        Runtime::Php,
        &["composer.json", "composer.lock", ".php-version"],
    ),
    (
        Runtime::Dotnet,
        &[
            "global.json",
            "Directory.Build.props",
            "Directory.Build.targets",
            "Directory.Packages.props",
        ],
    ),
    (Runtime::Swift, &["Package.swift"]),
    (
        Runtime::Lua,
        &[".luarc.json", ".luacheckrc", ".lua-version"],
    ),
    (
        Runtime::R,
        &[
            "DESCRIPTION",
            "NAMESPACE",
            "renv.lock",
            "packrat.lock",
            ".Rprofile",
        ],
    ),
    (
        Runtime::Julia,
        &["Project.toml", "Manifest.toml", "JuliaProject.toml"],
    ),
    (Runtime::Elixir, &["mix.exs", "mix.lock", ".elixir-version"]),
    (
        Runtime::Dart,
        &[
            "pubspec.yaml",
            "pubspec.lock",
            "analysis_options.yaml",
            ".dart_tool",
        ],
    ),
    (
        Runtime::Haskell,
        &[
            "cabal.project",
            "stack.yaml",
            "stack.yaml.lock",
            "package.yaml",
            "Setup.hs",
        ],
    ),
    (Runtime::Zig, &["build.zig", "build.zig.zon", "zig-out"]),
];

fn detect_markers(names: &HashSet<OsString>, runtimes: &mut HashSet<Runtime>) {
    for (runtime, markers) in RUNTIME_MARKERS {
        if has_any(names, markers) {
            runtimes.insert(*runtime);
        }
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
            "Rproj" => {
                runtimes.insert(Runtime::R);
            }
            "cabal" => {
                runtimes.insert(Runtime::Haskell);
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
            &Runtime::ALL,
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
            &Runtime::ALL,
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
            &Runtime::ALL,
            &PromptEnvironment::default(),
        );
        assert!(project.runtimes.contains(&Runtime::Cpp));
        assert!(!project.runtimes.contains(&Runtime::C));
    }

    #[test]
    fn detects_new_volatile_runtime_markers() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("DESCRIPTION"), b"Package: foo\n").unwrap();
        fs::write(directory.path().join("Project.toml"), b"").unwrap();
        fs::write(directory.path().join("mix.exs"), b"").unwrap();
        fs::write(directory.path().join("pubspec.yaml"), b"").unwrap();
        fs::write(directory.path().join("stack.yaml"), b"").unwrap();
        fs::write(directory.path().join("build.zig"), b"").unwrap();

        let project = detect(
            directory.path(),
            Some(directory.path()),
            &Runtime::ALL,
            &PromptEnvironment::default(),
        );
        assert!(project.runtimes.contains(&Runtime::R));
        assert!(project.runtimes.contains(&Runtime::Julia));
        assert!(project.runtimes.contains(&Runtime::Elixir));
        assert!(project.runtimes.contains(&Runtime::Dart));
        assert!(project.runtimes.contains(&Runtime::Haskell));
        assert!(project.runtimes.contains(&Runtime::Zig));
    }

    #[test]
    fn rproj_extension_detects_the_r_runtime() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("project.Rproj"), b"").unwrap();

        let project = detect(
            directory.path(),
            Some(directory.path()),
            &Runtime::ALL,
            &PromptEnvironment::default(),
        );
        assert!(project.runtimes.contains(&Runtime::R));
    }

    #[test]
    fn cabal_extension_detects_haskell() {
        let directory = TestDirectory::new();
        fs::write(
            directory.path().join("my-package.cabal"),
            b"cabal-version: 3.0\n",
        )
        .unwrap();

        let project = detect(
            directory.path(),
            Some(directory.path()),
            &Runtime::ALL,
            &PromptEnvironment::default(),
        );
        assert!(project.runtimes.contains(&Runtime::Haskell));
    }

    #[test]
    fn detection_is_fresh_without_a_project_fingerprint() {
        let directory = TestDirectory::new();
        let before = detect(
            directory.path(),
            Some(directory.path()),
            &Runtime::ALL,
            &PromptEnvironment::default(),
        )
        .runtimes;
        fs::write(directory.path().join(".python-version"), b"3.14\n").unwrap();
        let after = detect(
            directory.path(),
            Some(directory.path()),
            &Runtime::ALL,
            &PromptEnvironment::default(),
        )
        .runtimes;

        assert!(!before.contains(&Runtime::Python));
        assert!(after.contains(&Runtime::Python));
    }
}
