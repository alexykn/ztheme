use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
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

    repository_root(cwd, environment)
}

/// Request-side discovery must finish before querying the shared gitstatusd,
/// whose environment deliberately has no request-specific ceiling settings.
/// Walking ancestors is bounded by the components of the canonical cwd.
pub(crate) fn repository_root(cwd: &Path, environment: &PromptEnvironment) -> Option<PathBuf> {
    let cwd = fs::canonicalize(cwd).ok()?;
    let ceiling = ceiling_directory(&cwd, environment);
    for directory in cwd.ancestors() {
        if ceiling.as_deref() == Some(directory) {
            break;
        }
        let dot_git = directory.join(".git");
        if dot_git.is_dir() && is_git_directory(&dot_git) {
            return Some(directory.to_path_buf());
        }
        if dot_git.is_file() {
            // A malformed gitfile stops Git discovery rather than permitting
            // another ancestor search in the shared process.
            let contents = read_control_file(&dot_git)?;
            let target = trim_line_end(contents.strip_prefix(b"gitdir: ")?);
            let target = absolute(directory, Path::new(OsStr::from_bytes(target)));
            return is_git_directory(&target).then(|| directory.to_path_buf());
        }
        // Git also discovers bare repositories (and a cwd inside .git).
        if is_git_directory(directory) {
            return Some(directory.to_path_buf());
        }
    }
    None
}

/// Check Git's directory signatures rather than treating any `.git` directory
/// as a repository. Otherwise gitstatusd could skip that invalid marker and
/// find a real repository beyond this request's ceiling. Linked worktrees keep
/// objects/refs in the directory named by `commondir`.
fn is_git_directory(directory: &Path) -> bool {
    let head = directory.join("HEAD");
    let valid_head = fs::read_link(&head)
        .is_ok_and(|target| target.as_os_str().as_bytes().starts_with(b"refs/"))
        || read_control_file(&head).is_some_and(|contents| {
            if let Some(reference) = contents.strip_prefix(b"ref:") {
                reference.trim_ascii_start().starts_with(b"refs/")
            } else {
                contents.len() >= 40 && contents[..40].iter().all(u8::is_ascii_hexdigit)
            }
        });
    if !valid_head {
        return false;
    }
    let common = directory.join("commondir");
    let common = if common.exists() {
        let Some(contents) = read_control_file(&common) else {
            return false;
        };
        absolute(
            directory,
            Path::new(OsStr::from_bytes(trim_line_end(&contents))),
        )
    } else {
        directory.to_path_buf()
    };
    common.join("objects").is_dir() && common.join("refs").is_dir()
}

fn read_control_file(path: &Path) -> Option<Vec<u8>> {
    let file = crate::filesystem::open_regular_file(path).ok()?;
    let mut contents = Vec::new();
    file.take(4097).read_to_end(&mut contents).ok()?;
    (contents.len() <= 4096).then_some(contents)
}

fn trim_line_end(contents: &[u8]) -> &[u8] {
    let mut contents = contents;
    while matches!(contents.last(), Some(b'\r' | b'\n')) {
        contents = &contents[..contents.len() - 1];
    }
    contents
}

/// Git canonicalizes absolute ceilings until an empty list entry, after which
/// entries are trusted to contain no symlinks and compared literally. Relative
/// entries are ignored. Only strict ancestors count: a ceiling at cwd itself
/// must not prevent discovery in cwd *or* in its parents.
fn ceiling_directory(cwd: &Path, environment: &PromptEnvironment) -> Option<PathBuf> {
    let value = environment.git_ceilings.as_deref()?;
    let cwd_bytes = cwd.as_os_str().as_bytes();
    let mut canonicalize = true;
    let mut longest = None;
    for path in env::split_paths(value) {
        if path.as_os_str().is_empty() {
            canonicalize = false;
            continue;
        }
        if !path.is_absolute() {
            continue;
        }
        let path = if canonicalize {
            let Ok(path) = fs::canonicalize(path) else {
                continue;
            };
            path
        } else {
            path
        };
        let bytes = path.as_os_str().as_bytes();
        // Mirror Git's longest_ancestor_length, including its single trailing
        // slash removal and the special zero-length boundary for `/`.
        let length = bytes.len() - usize::from(bytes.last() == Some(&b'/'));
        if cwd_bytes.starts_with(&bytes[..length])
            && cwd_bytes.get(length) == Some(&b'/')
            && cwd_bytes.get(length + 1).is_some()
        {
            longest = Some(longest.map_or(length, |previous: usize| previous.max(length)));
        }
    }
    longest.map(|length| {
        if length == 0 {
            PathBuf::from("/")
        } else {
            PathBuf::from(OsStr::from_bytes(&cwd_bytes[..length]))
        }
    })
}

pub(crate) fn detect(
    cwd: &Path,
    git_root: Option<&Path>,
    configured: &[Runtime],
    environment: &PromptEnvironment,
) -> Project {
    let mut runtimes = HashSet::new();
    let configured = configured.iter().copied().collect::<HashSet<_>>();
    let home = environment
        .home
        .as_deref()
        .map(|home| fs::canonicalize(home).unwrap_or_else(|_| PathBuf::from(home)));
    let mut directory = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let ceiling = ceiling_directory(&directory, environment);
    let git_root =
        git_root.map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
    let mut javascript = None;

    if (environment.virtual_env.is_some() || environment.conda_prefix.is_some())
        && configured.contains(&Runtime::Python)
    {
        runtimes.insert(Runtime::Python);
    }

    for depth in 0..32 {
        if depth > 0
            && (home.as_deref() == Some(directory.as_path())
                || ceiling.as_deref() == Some(directory.as_path()))
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

        if git_root.as_deref() == Some(directory.as_path()) {
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
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Runtime, detect, repository_root, worktree_root};
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

    fn git(cwd: &Path, ceiling: &OsStr, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .env_clear()
            .env("HOME", cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", ceiling)
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
    }

    #[test]
    fn git_control_reads_reject_special_files_and_allow_regular_symlinks() {
        let directory = TestDirectory::new();
        let fifo = directory.path().join("HEAD");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let link = directory.path().join("control-link");
        symlink(&fifo, &link).unwrap();
        for path in [&fifo, &link, directory.path(), Path::new("/dev/null")] {
            assert!(super::read_control_file(path).is_none());
        }
        fs::remove_file(&fifo).unwrap();
        fs::write(&fifo, b"ref: refs/heads/main\n").unwrap();
        assert_eq!(
            super::read_control_file(&link).unwrap(),
            b"ref: refs/heads/main\n"
        );
        fs::write(&fifo, vec![b'x'; 4097]).unwrap();
        assert!(super::read_control_file(&link).is_none());
    }

    #[test]
    fn repository_ceiling_discovery_matches_git_and_runtime_boundaries() {
        let directory = TestDirectory::new();
        let repo = fs::canonicalize(directory.path()).unwrap();
        assert!(git(&repo, OsStr::new(""), &["init", "-q"]).status.success());
        let child = repo.join("child");
        fs::create_dir(&child).unwrap();
        fs::write(repo.join("Cargo.toml"), "").unwrap();
        let alias = repo.join("repo-alias");
        symlink(&repo, &alias).unwrap();
        let cwd_alias = repo.join("child-alias");
        symlink(&child, &cwd_alias).unwrap();
        let cases = [
            ("ordinary", child.clone(), OsString::new(), true),
            (
                "parent repo",
                child.clone(),
                repo.clone().into_os_string(),
                false,
            ),
            (
                "initial cwd",
                child.clone(),
                child.clone().into_os_string(),
                true,
            ),
            (
                "initial repo cwd",
                repo.clone(),
                repo.clone().into_os_string(),
                true,
            ),
            (
                "relative ignored",
                child.clone(),
                OsString::from(".."),
                true,
            ),
            (
                "symlink ceiling",
                child.clone(),
                alias.clone().into_os_string(),
                false,
            ),
            (
                "symlink cwd",
                cwd_alias,
                repo.clone().into_os_string(),
                false,
            ),
            (
                "literal symlink after empty",
                child.clone(),
                OsString::from(format!(":{}", alias.display())),
                true,
            ),
            (
                "literal canonical after empty",
                child.clone(),
                OsString::from(format!(":{}", repo.display())),
                false,
            ),
            (
                "canonical dot components",
                child.clone(),
                OsString::from(format!("{}/child/..", repo.display())),
                false,
            ),
            (
                "literal dot components",
                child.clone(),
                OsString::from(format!(":{}/child/..", repo.display())),
                true,
            ),
            (
                "nearest of multiple",
                child.clone(),
                OsString::from(format!(
                    "{}:{}",
                    repo.parent().unwrap().display(),
                    repo.display()
                )),
                false,
            ),
        ];
        for (name, cwd, ceiling, expected) in cases {
            assert_ceiling_discovery(&repo, name, &cwd, ceiling, expected);
        }
    }

    fn assert_ceiling_discovery(
        repo: &Path,
        name: &str,
        cwd: &Path,
        ceiling: OsString,
        expected: bool,
    ) {
        let actual_git = git(cwd, &ceiling, &["rev-parse", "--show-toplevel"]);
        assert_eq!(actual_git.status.success(), expected, "Git: {name}");
        let environment = PromptEnvironment {
            git_ceilings: Some(ceiling),
            ..PromptEnvironment::default()
        };
        let discovered = repository_root(cwd, &environment);
        assert_eq!(
            discovered.as_deref(),
            expected.then_some(repo),
            "discovery: {name}"
        );
        assert_eq!(worktree_root(cwd, &environment), discovered);
        let project = detect(cwd, discovered.as_deref(), &[Runtime::Rust], &environment);
        assert_eq!(
            project.runtimes.contains(&Runtime::Rust),
            expected,
            "runtime: {name}"
        );
        let query = crate::gitstatus::Query::discover(cwd, &environment);
        assert_eq!(
            query.as_ref().map(crate::gitstatus::Query::path),
            expected.then_some(repo),
            "query: {name}"
        );
    }

    #[test]
    fn discovery_preserves_git_files_and_bare_repositories() {
        let directory = TestDirectory::new();
        let root = fs::canonicalize(directory.path()).unwrap();
        let worktree = root.join("worktree");
        let git_dir = root.join("separate.git ");
        assert!(
            git(
                &root,
                OsStr::new(""),
                &[
                    "init",
                    "-q",
                    "--separate-git-dir",
                    git_dir.to_str().unwrap(),
                    worktree.to_str().unwrap()
                ]
            )
            .status
            .success()
        );
        assert_eq!(
            repository_root(&worktree, &PromptEnvironment::default()),
            Some(worktree)
        );
        let bare = root.join("bare.git");
        assert!(
            git(
                &root,
                OsStr::new(""),
                &["init", "-q", "--bare", bare.to_str().unwrap()]
            )
            .status
            .success()
        );
        let child = bare.join("child");
        fs::create_dir(&child).unwrap();
        assert!(
            git(&child, OsStr::new(""), &["rev-parse", "--git-dir"])
                .status
                .success()
        );
        assert_eq!(
            repository_root(&child, &PromptEnvironment::default()),
            Some(bare)
        );
    }

    #[test]
    fn invalid_git_directory_does_not_allow_a_query_past_the_ceiling() {
        let directory = TestDirectory::new();
        let repo = fs::canonicalize(directory.path()).unwrap();
        assert!(git(&repo, OsStr::new(""), &["init", "-q"]).status.success());
        let child = repo.join("child");
        fs::create_dir_all(child.join(".git/objects")).unwrap();
        fs::create_dir(child.join(".git/refs")).unwrap();
        fs::write(child.join(".git/HEAD"), "not a valid HEAD\n").unwrap();
        let environment = PromptEnvironment {
            git_ceilings: Some(repo.clone().into_os_string()),
            ..PromptEnvironment::default()
        };
        assert!(
            !git(&child, repo.as_os_str(), &["rev-parse", "--show-toplevel"])
                .status
                .success()
        );
        assert!(crate::gitstatus::Query::discover(&child, &environment).is_none());
        assert_eq!(
            repository_root(&child, &PromptEnvironment::default()),
            Some(repo)
        );
    }

    #[test]
    fn linked_worktree_is_discovered_below_a_ceiling() {
        let directory = TestDirectory::new();
        let root = fs::canonicalize(directory.path()).unwrap();
        assert!(git(&root, OsStr::new(""), &["init", "-q"]).status.success());
        assert!(
            git(
                &root,
                OsStr::new(""),
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "test"
                ]
            )
            .status
            .success()
        );
        let worktree = root.join("linked");
        assert!(
            git(
                &root,
                OsStr::new(""),
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "linked",
                    worktree.to_str().unwrap()
                ]
            )
            .status
            .success()
        );
        let child = worktree.join("child");
        fs::create_dir(&child).unwrap();
        let environment = PromptEnvironment {
            git_ceilings: Some(root.clone().into_os_string()),
            ..PromptEnvironment::default()
        };
        assert!(
            git(&child, root.as_os_str(), &["rev-parse", "--show-toplevel"])
                .status
                .success()
        );
        assert_eq!(repository_root(&child, &environment), Some(worktree));
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
