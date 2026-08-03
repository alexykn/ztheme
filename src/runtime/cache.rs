use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tokio::process::Command;

use super::detect::Project;
use super::{Runtime, RuntimeSpec};
use crate::cache::CacheKey;
use crate::environment::PromptEnvironment;

const MAX_SELECTOR_BYTES: usize = 64 * 1024;
const MAX_RUSTUP_DIRECTORY_ENTRIES: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct RuntimePlan {
    pub(crate) runtime: Runtime,
    pub(crate) spec: RuntimeSpec,
    pub(crate) program: PathBuf,
    pub(crate) cache: Cacheability,
}

impl RuntimePlan {
    pub(crate) fn is_cacheable(&self) -> bool {
        matches!(self.cache, Cacheability::Cacheable(_))
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Cacheability {
    Cacheable(ExecutableIdentity),
    Volatile(VolatileReason),
}

#[derive(Clone, Debug)]
pub(crate) enum VolatileReason {
    MissingExecutable,
    ScriptWrapper,
    UnknownShim,
    AmbiguousSelector,
    UnsupportedToolchainSelection,
    UnsupportedContextualSelection,
    UnreadableDependency,
    SearchDepthExceeded,
    NotYetCacheable,
}

#[derive(Clone, Debug)]
pub(crate) struct ExecutableIdentity {
    requested_path: PathBuf,
    link_metadata: MetadataIdentity,
    canonical_path: PathBuf,
    target_metadata: MetadataIdentity,
}

#[derive(Clone, Debug)]
struct MetadataIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    change_seconds: i64,
    change_nanos: i64,
    modified_seconds: i64,
    modified_nanos: i32,
}

#[derive(Clone)]
pub(crate) struct BasePlan {
    runtime: Runtime,
    spec: RuntimeSpec,
    program: PathBuf,
    available: bool,
}

pub(crate) fn resolve_base_plans(
    configured: &[Runtime],
    cwd: &Path,
    environment: &PromptEnvironment,
) -> Vec<BasePlan> {
    let mut plans = configured
        .iter()
        .copied()
        .map(|runtime| {
            if matches!(runtime, Runtime::C | Runtime::Cpp) {
                return compiler_base_plan(runtime, cwd, environment);
            }

            let spec = super::spec(runtime);
            let program = candidate_program(runtime, &spec, cwd, environment);
            let available = is_executable_file(&program);
            BasePlan {
                runtime,
                spec,
                program,
                available,
            }
        })
        .collect::<Vec<_>>();
    plans.sort_unstable_by_key(|plan| plan.runtime);
    plans
}

fn compiler_base_plan(runtime: Runtime, cwd: &Path, environment: &PromptEnvironment) -> BasePlan {
    let cpp = runtime == Runtime::Cpp;
    let clang = if cpp { "clang++" } else { "clang" };
    let gcc = if cpp { "g++" } else { "gcc" };
    let path = path_value(environment);

    if let Some(program) = resolve_on_path(OsStr::new(clang), path, cwd) {
        return BasePlan {
            runtime,
            spec: super::compiler_spec(cpp, true),
            program,
            available: true,
        };
    }

    let Some(program) = resolve_on_path(OsStr::new(gcc), path, cwd) else {
        return BasePlan {
            runtime,
            spec: super::compiler_spec(cpp, false),
            program: PathBuf::from(gcc),
            available: false,
        };
    };

    BasePlan {
        runtime,
        spec: super::compiler_spec(cpp, false),
        program,
        available: true,
    }
}

pub(crate) fn finalize_plans(
    cwd: &Path,
    project: &Project,
    base_plans: Vec<BasePlan>,
    environment: &PromptEnvironment,
) -> Vec<RuntimePlan> {
    debug_assert_eq!(project.cwd.as_path(), cwd);
    let mut plans = base_plans
        .into_iter()
        .filter(|plan| project.runtimes.contains(&plan.runtime))
        .map(|plan| finalize_plan(cwd, plan, environment))
        .collect::<Vec<_>>();
    plans.sort_unstable_by_key(|plan| plan.runtime);
    plans
}

fn finalize_plan(cwd: &Path, plan: BasePlan, environment: &PromptEnvironment) -> RuntimePlan {
    let BasePlan {
        runtime,
        spec,
        program,
        available,
    } = plan;

    if !available {
        return RuntimePlan {
            runtime,
            spec,
            program,
            cache: Cacheability::Volatile(VolatileReason::MissingExecutable),
        };
    }

    if runtime == Runtime::Go && !explicit_local_go(environment) {
        return volatile(
            runtime,
            spec,
            program,
            VolatileReason::UnsupportedToolchainSelection,
        );
    }

    if runtime == Runtime::Rust && is_rustup_proxy(&program) {
        return resolve_rustup(runtime, spec, program, cwd, environment);
    }

    if program.parent().and_then(Path::file_name) == Some(OsStr::new("shims")) {
        return match resolve_supported_shim(runtime, spec.clone(), &program, cwd, environment) {
            Ok(plan) => plan,
            Err(reason) => volatile(runtime, spec, program, reason),
        };
    }

    match runtime {
        Runtime::Dotnet => {
            return volatile(
                runtime,
                spec,
                program,
                VolatileReason::UnsupportedContextualSelection,
            );
        }
        Runtime::R | Runtime::Julia | Runtime::Elixir | Runtime::Haskell => {
            return volatile(runtime, spec, program, VolatileReason::NotYetCacheable);
        }
        _ => {}
    }

    classify_resolved_target(runtime, spec, program.clone(), program)
}

fn candidate_program(
    runtime: Runtime,
    spec: &RuntimeSpec,
    cwd: &Path,
    environment: &PromptEnvironment,
) -> PathBuf {
    match runtime {
        Runtime::Python => python_candidate(cwd, environment),
        Runtime::Java => resolve_on_path(&spec.program, path_value(environment), cwd)
            .unwrap_or_else(|| PathBuf::from(&spec.program)),
        Runtime::Dotnet => resolve_on_path(&spec.program, path_value(environment), cwd)
            .unwrap_or_else(|| PathBuf::from(&spec.program)),
        _ => {
            let requested = Path::new(&spec.program);
            if requested.components().count() > 1 {
                requested.to_path_buf()
            } else {
                resolve_on_path(&spec.program, path_value(environment), cwd)
                    .unwrap_or_else(|| requested.to_path_buf())
            }
        }
    }
}

fn python_candidate(cwd: &Path, environment: &PromptEnvironment) -> PathBuf {
    for prefix in [
        environment.virtual_env.as_deref(),
        environment.conda_prefix.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let candidate = PathBuf::from(prefix).join("bin/python");
        if is_executable_file(&candidate) {
            return candidate;
        }
    }

    if let Some(candidate) = resolve_on_path(OsStr::new("python"), path_value(environment), cwd) {
        return candidate;
    }
    resolve_on_path(OsStr::new("python3"), path_value(environment), cwd)
        .unwrap_or_else(|| PathBuf::from("python3"))
}

fn path_value(environment: &PromptEnvironment) -> &OsStr {
    environment.path.as_deref().unwrap_or_default()
}

pub(crate) fn request_path_entry(entry: &Path, cwd: &Path) -> PathBuf {
    if entry.as_os_str().is_empty() {
        return cwd.to_path_buf();
    }
    if entry.is_absolute() {
        return entry.to_path_buf();
    }
    cwd.join(entry)
}

fn request_path(value: &OsStr, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub(crate) fn resolve_on_path(program: &OsStr, path: &OsStr, cwd: &Path) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|directory| request_path_entry(&directory, cwd).join(program))
        .find(|candidate| is_executable_file(candidate))
}

fn direct(runtime: Runtime, spec: RuntimeSpec, program: PathBuf) -> RuntimePlan {
    let cache = executable_identity(&program).map_or_else(
        |_| Cacheability::Volatile(VolatileReason::UnreadableDependency),
        Cacheability::Cacheable,
    );
    RuntimePlan {
        runtime,
        spec,
        program,
        cache,
    }
}

fn volatile(
    runtime: Runtime,
    spec: RuntimeSpec,
    program: PathBuf,
    reason: VolatileReason,
) -> RuntimePlan {
    RuntimePlan {
        runtime,
        spec,
        program,
        cache: Cacheability::Volatile(reason),
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.file_type().is_file() && metadata.mode() & 0o111 != 0
}

fn executable_identity(path: &Path) -> io::Result<ExecutableIdentity> {
    let link_metadata = fs::symlink_metadata(path)?;
    let canonical_path = fs::canonicalize(path)?;
    let target_metadata = fs::metadata(&canonical_path)?;
    if !target_metadata.file_type().is_file() || target_metadata.mode() & 0o111 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime executable is not executable",
        ));
    }
    Ok(ExecutableIdentity {
        requested_path: path.to_path_buf(),
        link_metadata: MetadataIdentity::from(&link_metadata),
        canonical_path,
        target_metadata: MetadataIdentity::from(&target_metadata),
    })
}

impl From<&fs::Metadata> for MetadataIdentity {
    fn from(metadata: &fs::Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok());
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            mode: metadata.mode(),
            change_seconds: metadata.ctime(),
            change_nanos: metadata.ctime_nsec(),
            modified_seconds: modified
                .as_ref()
                .map_or(0, std::time::Duration::as_secs)
                .try_into()
                .unwrap_or(i64::MAX),
            modified_nanos: modified
                .as_ref()
                .map_or(0, std::time::Duration::subsec_nanos)
                .try_into()
                .unwrap_or(i32::MAX),
        }
    }
}

fn has_shebang(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut prefix = [0; 2];
    let length = file.read(&mut prefix)?;
    Ok(length == prefix.len() && prefix == *b"#!")
}

fn classify_resolved_target(
    runtime: Runtime,
    spec: RuntimeSpec,
    original_program: PathBuf,
    selected_program: PathBuf,
) -> RuntimePlan {
    match has_shebang(&selected_program) {
        Ok(true) => {
            return volatile(
                runtime,
                spec,
                original_program,
                VolatileReason::ScriptWrapper,
            );
        }
        Ok(false) => {}
        Err(_) => {
            return volatile(
                runtime,
                spec,
                original_program,
                VolatileReason::UnreadableDependency,
            );
        }
    }

    match canonical_name_is_dispatcher(&selected_program) {
        Ok(true) => volatile(runtime, spec, original_program, VolatileReason::UnknownShim),
        Ok(false) => direct(runtime, spec, selected_program),
        Err(_) => volatile(
            runtime,
            spec,
            original_program,
            VolatileReason::UnreadableDependency,
        ),
    }
}

fn canonical_name_is_dispatcher(path: &Path) -> io::Result<bool> {
    let canonical = fs::canonicalize(path)?;
    Ok(canonical
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            matches!(
                name,
                "asdf" | "mise" | "pyenv" | "rbenv" | "nodenv" | "plenv" | "rustup"
            )
        }))
}

struct ShimFamily {
    manager: &'static str,
    program: &'static str,
    selector_file: &'static str,
    version_env: fn(&PromptEnvironment) -> Option<&OsStr>,
    directory_env: fn(&PromptEnvironment) -> Option<&OsStr>,
}

fn shim_family(runtime: Runtime) -> Option<ShimFamily> {
    let family = match runtime {
        Runtime::Python => ShimFamily {
            manager: "pyenv",
            program: "python",
            selector_file: ".python-version",
            version_env: |environment| environment.pyenv_version.as_deref(),
            directory_env: |environment| environment.pyenv_dir.as_deref(),
        },
        Runtime::Ruby => ShimFamily {
            manager: "rbenv",
            program: "ruby",
            selector_file: ".ruby-version",
            version_env: |environment| environment.rbenv_version.as_deref(),
            directory_env: |environment| environment.rbenv_dir.as_deref(),
        },
        Runtime::Node => ShimFamily {
            manager: "nodenv",
            program: "node",
            selector_file: ".node-version",
            version_env: |environment| environment.nodenv_version.as_deref(),
            directory_env: |environment| environment.nodenv_dir.as_deref(),
        },
        Runtime::Perl => ShimFamily {
            manager: "plenv",
            program: "perl",
            selector_file: ".perl-version",
            version_env: |environment| environment.plenv_version.as_deref(),
            directory_env: |environment| environment.plenv_dir.as_deref(),
        },
        _ => return None,
    };
    Some(family)
}

fn resolve_supported_shim(
    runtime: Runtime,
    spec: RuntimeSpec,
    shim: &Path,
    cwd: &Path,
    environment: &PromptEnvironment,
) -> Result<RuntimePlan, VolatileReason> {
    let Some(family) = shim_family(runtime) else {
        return Err(VolatileReason::UnknownShim);
    };
    let Some(root) = verified_manager_root(shim, &family) else {
        return Err(VolatileReason::UnknownShim);
    };
    let selected = select_manager_version(&family, &root, cwd, environment)?;
    if selected == OsStr::new("system") {
        let actual = resolve_after_directory(
            OsStr::new(family.program),
            path_value(environment),
            cwd,
            shim.parent().ok_or(VolatileReason::UnreadableDependency)?,
        )
        .ok_or(VolatileReason::AmbiguousSelector)?;
        return Ok(classify_resolved_target(
            runtime,
            spec,
            shim.to_path_buf(),
            actual,
        ));
    }

    let actual = root
        .join("versions")
        .join(&selected)
        .join("bin")
        .join(family.program);
    if !is_executable_file(&actual) {
        return Err(VolatileReason::AmbiguousSelector);
    }
    Ok(classify_resolved_target(
        runtime,
        spec,
        shim.to_path_buf(),
        actual,
    ))
}

fn verified_manager_root(shim: &Path, family: &ShimFamily) -> Option<PathBuf> {
    let shims = shim.parent()?;
    if shims.file_name() != Some(OsStr::new("shims")) {
        return None;
    }
    let root = shims.parent()?.to_path_buf();
    let manager_entry = [
        root.join("bin").join(family.manager),
        root.join(family.manager),
    ]
    .into_iter()
    .find(|path| is_executable_file(path))?;
    let versions = root.join("versions");
    if !is_executable_file(&manager_entry) || !versions.is_dir() {
        return None;
    }
    Some(root)
}

fn select_manager_version(
    family: &ShimFamily,
    root: &Path,
    cwd: &Path,
    environment: &PromptEnvironment,
) -> Result<OsString, VolatileReason> {
    if let Some(value) = (family.version_env)(environment) {
        return parse_single_token(value).ok_or(VolatileReason::AmbiguousSelector);
    }

    let start = (family.directory_env)(environment)
        .map_or_else(|| cwd.to_path_buf(), |value| request_path(value, cwd));
    if let Some(selector) = find_up_bounded(&start, family.selector_file)? {
        return read_single_token(&selector).map_err(|_| VolatileReason::UnreadableDependency);
    }
    read_single_token(&root.join("version")).map_err(|_| VolatileReason::AmbiguousSelector)
}

fn find_up_bounded(start: &Path, file_name: &str) -> Result<Option<PathBuf>, VolatileReason> {
    let mut directory = start.to_path_buf();
    for depth in 0..32 {
        let candidate = directory.join(file_name);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        let Some(parent) = directory.parent() else {
            return Ok(None);
        };
        if parent == directory {
            return Ok(None);
        }
        if depth == 31 {
            return Err(VolatileReason::SearchDepthExceeded);
        }
        directory = parent.to_path_buf();
    }
    Err(VolatileReason::SearchDepthExceeded)
}

fn read_single_token(path: &Path) -> io::Result<OsString> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(MAX_SELECTOR_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SELECTOR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selector file is too large",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "selector is not UTF-8"))?;
    let mut tokens = text.split_whitespace();
    let Some(token) = tokens.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selector is empty",
        ));
    };
    if tokens.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selector has multiple tokens",
        ));
    }
    Ok(OsString::from(token))
}

fn parse_single_token(value: &OsStr) -> Option<OsString> {
    let text = value.to_str()?;
    let mut tokens = text.split_whitespace();
    let token = tokens.next()?;
    tokens.next().is_none().then(|| OsString::from(token))
}

fn resolve_after_directory(
    program: &OsStr,
    path: &OsStr,
    cwd: &Path,
    excluded: &Path,
) -> Option<PathBuf> {
    let mut after = false;
    let canonical_excluded = fs::canonicalize(excluded).ok();
    for directory in env::split_paths(path).map(|entry| request_path_entry(&entry, cwd)) {
        let canonical_directory = fs::canonicalize(&directory).ok();
        let is_excluded = directory == excluded
            || canonical_excluded
                .as_deref()
                .zip(canonical_directory.as_deref())
                .is_some_and(|(left, right)| left == right);
        if is_excluded {
            after = true;
            continue;
        }
        if after && is_executable_file(&directory.join(program)) {
            return Some(directory.join(program));
        }
    }
    None
}

fn is_rustup_proxy(path: &Path) -> bool {
    if path.file_name() != Some(OsStr::new("rustc")) {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let rustup = parent.join("rustup");
    let Ok(proxy) = fs::metadata(path) else {
        return false;
    };
    if !is_executable_file(&rustup) {
        return false;
    }
    let Ok(manager) = fs::metadata(rustup) else {
        return false;
    };
    proxy.dev() == manager.dev() && proxy.ino() == manager.ino()
}

fn resolve_rustup(
    runtime: Runtime,
    spec: RuntimeSpec,
    proxy: PathBuf,
    cwd: &Path,
    environment: &PromptEnvironment,
) -> RuntimePlan {
    let explicit = match environment.rustup_toolchain.as_deref() {
        Some(value) => match parse_single_token(value) {
            Some(value) => Some(ToolchainSelection::Name(value)),
            None => {
                return volatile(runtime, spec, proxy, VolatileReason::AmbiguousSelector);
            }
        },
        None => None,
    };

    let Some(home) = environment
        .rustup_home
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .home
                .as_deref()
                .map(|home| PathBuf::from(home).join(".rustup"))
        })
    else {
        return volatile(
            runtime,
            spec,
            proxy,
            VolatileReason::UnsupportedToolchainSelection,
        );
    };

    let selected = match explicit {
        Some(selected) => selected,
        None => match rustup_selection(&home, cwd) {
            Ok(selected) => selected,
            Err(reason) => return volatile(runtime, spec, proxy, reason),
        },
    };

    let Some(rustc) = installed_rustc(&home, selected) else {
        return volatile(runtime, spec, proxy, VolatileReason::AmbiguousSelector);
    };
    classify_resolved_target(runtime, spec, proxy, rustc)
}

enum ToolchainSelection {
    Name(OsString),
    Path(PathBuf),
}

fn rustup_selection(home: &Path, cwd: &Path) -> Result<ToolchainSelection, VolatileReason> {
    let settings_path = home.join("settings.toml");
    let settings = match read_text_file(&settings_path) {
        Ok(settings) => Some(
            toml::from_str::<toml::Table>(&settings)
                .map_err(|_| VolatileReason::UnreadableDependency)?,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err(VolatileReason::UnreadableDependency),
    };

    let mut directory = cwd.to_path_buf();
    for depth in 0..32 {
        if let Some(selection) = override_for_directory(settings.as_ref(), &directory)? {
            return Ok(ToolchainSelection::Name(selection));
        }

        let plain = directory.join("rust-toolchain");
        if selector_file_exists(&plain)? {
            return parse_toolchain_file(&plain);
        }

        let toml = directory.join("rust-toolchain.toml");
        if selector_file_exists(&toml)? {
            return parse_toolchain_file(&toml);
        }

        let Some(parent) = directory.parent() else {
            break;
        };
        if parent == directory {
            break;
        }
        if depth == 31 {
            return Err(VolatileReason::SearchDepthExceeded);
        }
        directory = parent.to_path_buf();
    }

    let default = settings
        .as_ref()
        .and_then(|value| value.get("default_toolchain"))
        .and_then(toml::Value::as_str)
        .ok_or(VolatileReason::AmbiguousSelector)?;
    Ok(ToolchainSelection::Name(OsString::from(default)))
}

fn override_for_directory(
    settings: Option<&toml::Table>,
    cwd: &Path,
) -> Result<Option<OsString>, VolatileReason> {
    let Some(settings) = settings else {
        return Ok(None);
    };

    let Some(overrides) = settings.get("overrides") else {
        return Ok(None);
    };
    let overrides = overrides
        .as_table()
        .ok_or(VolatileReason::UnreadableDependency)?;
    let Some(value) = overrides.get(&cwd.to_string_lossy().to_string()) else {
        return Ok(None);
    };
    let value = value.as_str().ok_or(VolatileReason::AmbiguousSelector)?;
    parse_single_token(OsStr::new(value))
        .map(Some)
        .ok_or(VolatileReason::AmbiguousSelector)
}

fn selector_file_exists(path: &Path) -> Result<bool, VolatileReason> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Ok(_) | Err(_) => Err(VolatileReason::UnreadableDependency),
    }
}

fn parse_toolchain_file(path: &Path) -> Result<ToolchainSelection, VolatileReason> {
    let text = read_text_file(path).map_err(|_| VolatileReason::UnreadableDependency)?;
    if let Some(name) = parse_single_token(OsStr::new(&text)) {
        return Ok(ToolchainSelection::Name(name));
    }
    let value =
        toml::from_str::<toml::Table>(&text).map_err(|_| VolatileReason::AmbiguousSelector)?;
    let toolchain = value
        .get("toolchain")
        .and_then(toml::Value::as_table)
        .ok_or(VolatileReason::AmbiguousSelector)?;
    if let Some(path) = toolchain.get("path").and_then(toml::Value::as_str) {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(ToolchainSelection::Path(path));
        }
        return Err(VolatileReason::AmbiguousSelector);
    }
    let channel = toolchain
        .get("channel")
        .and_then(toml::Value::as_str)
        .ok_or(VolatileReason::AmbiguousSelector)?;
    parse_single_token(OsStr::new(channel))
        .map(ToolchainSelection::Name)
        .ok_or(VolatileReason::AmbiguousSelector)
}

fn installed_rustc(home: &Path, selection: ToolchainSelection) -> Option<PathBuf> {
    match selection {
        ToolchainSelection::Path(path) => {
            let candidate = path.join("bin/rustc");
            is_executable_file(&candidate).then_some(candidate)
        }
        ToolchainSelection::Name(name) => {
            let name_path = PathBuf::from(&name);
            if name_path.is_absolute() {
                let candidate = name_path.join("bin/rustc");
                return is_executable_file(&candidate).then_some(candidate);
            }
            let toolchains = home.join("toolchains");
            let exact = toolchains.join(&name).join("bin/rustc");
            if is_executable_file(&exact) {
                return Some(exact);
            }
            let text = name.to_str()?;
            let host = toolchains
                .join(format!("{text}-{}", host_triple()))
                .join("bin/rustc");
            if is_executable_file(&host) {
                return Some(host);
            }
            let mut matches = Vec::new();
            for (index, entry) in fs::read_dir(toolchains).ok()?.enumerate() {
                if index >= MAX_RUSTUP_DIRECTORY_ENTRIES {
                    return None;
                }
                let entry = entry.ok()?;
                let file_name = entry.file_name();
                let candidate_name = file_name.to_str()?;
                if candidate_name.starts_with(text)
                    && candidate_name.as_bytes().get(text.len()) == Some(&b'-')
                {
                    let candidate = entry.path().join("bin/rustc");
                    if is_executable_file(&candidate) {
                        matches.push(candidate);
                        if matches.len() > 1 {
                            return None;
                        }
                    }
                }
            }
            matches.pop()
        }
    }
}

fn host_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        os => format!("{arch}-unknown-{os}"),
    }
}

fn read_file_limited(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(MAX_SELECTOR_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SELECTOR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dependency file is too large",
        ));
    }
    Ok(bytes)
}

fn read_text_file(path: &Path) -> io::Result<String> {
    String::from_utf8(read_file_limited(path)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "dependency file is not UTF-8"))
}

fn explicit_local_go(environment: &PromptEnvironment) -> bool {
    environment
        .gotoolchain
        .as_deref()
        .is_some_and(|value| value == OsStr::new("local"))
}

pub(crate) fn cache_key(plans: &[RuntimePlan], environment: &PromptEnvironment) -> CacheKey {
    let mut digest = Sha256::new();
    add_field(&mut digest, b"domain", b"ztheme-runtime-snapshot-v2");
    add_u64(&mut digest, b"runtime-snapshot-version", 2);
    let cacheable_count = plans.iter().filter(|plan| plan.is_cacheable()).count();
    add_u64(
        &mut digest,
        b"cacheable-count",
        u64::try_from(cacheable_count).unwrap_or(u64::MAX),
    );
    for plan in plans.iter().filter(|plan| plan.is_cacheable()) {
        add_u64(&mut digest, b"runtime", u64::from(plan.runtime.id()));
        add_os(&mut digest, b"program-request", &plan.spec.program);
        add_u64(
            &mut digest,
            b"argument-count",
            u64::try_from(plan.spec.arguments.len()).unwrap_or(u64::MAX),
        );
        for argument in plan.spec.arguments {
            add_field(&mut digest, b"argument", argument.as_bytes());
        }
        add_optional_str(&mut digest, b"command-label", plan.spec.label);
        add_u64(
            &mut digest,
            b"merge-stderr",
            u64::from(plan.spec.merge_stderr),
        );
        match &plan.cache {
            Cacheability::Cacheable(identity) => {
                add_field(&mut digest, b"context", b"direct");
                add_identity(&mut digest, identity);
            }
            Cacheability::Volatile(_) => {}
        }
        for_declared_command_environment(plan.runtime, environment, |name, value| {
            add_field(&mut digest, b"declared-environment-name", name.as_bytes());
            add_optional_os(&mut digest, b"declared-environment-value", value);
        });
    }
    CacheKey::from_digest(digest.finalize().into())
}

pub(crate) fn apply_cacheable_environment(
    command: &mut Command,
    plan: &RuntimePlan,
    environment: &PromptEnvironment,
) {
    PromptEnvironment::prepare_command(command);
    for_declared_command_environment(plan.runtime, environment, |name, value| {
        if let Some(value) = value {
            command.env(name, value);
        }
    });
}

fn for_declared_command_environment(
    runtime: Runtime,
    environment: &PromptEnvironment,
    mut visit: impl FnMut(&'static str, Option<&OsStr>),
) {
    match runtime {
        Runtime::Python => {
            visit("VIRTUAL_ENV", environment.virtual_env.as_deref());
            visit("CONDA_PREFIX", environment.conda_prefix.as_deref());
        }
        Runtime::Perl => {
            visit("PERLBREW_PERL", environment.perlbrew_perl.as_deref());
            visit("PLENV_VERSION", environment.plenv_version.as_deref());
        }
        Runtime::Java => visit("JAVA_HOME", environment.java_home.as_deref()),
        Runtime::Rust => visit("RUSTUP_TOOLCHAIN", environment.rustup_toolchain.as_deref()),
        Runtime::Go => visit("GOTOOLCHAIN", environment.gotoolchain.as_deref()),
        Runtime::Ruby => {
            visit("RBENV_VERSION", environment.rbenv_version.as_deref());
            visit("RUBY_VERSION", environment.ruby_version.as_deref());
        }
        _ => {}
    }
}

fn add_identity(digest: &mut Sha256, identity: &ExecutableIdentity) {
    add_path(digest, b"requested-path", &identity.requested_path);
    add_metadata_identity(digest, b"link-metadata", &identity.link_metadata);
    add_path(digest, b"canonical-path", &identity.canonical_path);
    add_metadata_identity(digest, b"target-metadata", &identity.target_metadata);
}

fn add_metadata_identity(digest: &mut Sha256, label: &[u8], metadata: &MetadataIdentity) {
    add_field(digest, label, b"present");
    add_u64(digest, b"metadata-device", metadata.device);
    add_u64(digest, b"metadata-inode", metadata.inode);
    add_u64(digest, b"metadata-size", metadata.size);
    add_u64(digest, b"metadata-mode", u64::from(metadata.mode));
    add_field(
        digest,
        b"metadata-change-seconds",
        &metadata.change_seconds.to_be_bytes(),
    );
    add_field(
        digest,
        b"metadata-change-nanos",
        &metadata.change_nanos.to_be_bytes(),
    );
    add_field(
        digest,
        b"metadata-modified-seconds",
        &metadata.modified_seconds.to_be_bytes(),
    );
    add_field(
        digest,
        b"metadata-modified-nanos",
        &metadata.modified_nanos.to_be_bytes(),
    );
}

fn add_path(digest: &mut Sha256, label: &[u8], path: &Path) {
    add_field(digest, label, path.as_os_str().as_bytes());
}

fn add_os(digest: &mut Sha256, label: &[u8], value: &OsStr) {
    add_field(digest, label, value.as_bytes());
}

fn add_optional_os(digest: &mut Sha256, label: &[u8], value: Option<&OsStr>) {
    match value {
        Some(value) => {
            add_field(digest, b"presence", b"present");
            add_os(digest, label, value);
        }
        None => add_field(digest, b"presence", b"absent"),
    }
}

fn add_optional_str(digest: &mut Sha256, label: &[u8], value: Option<&str>) {
    match value {
        Some(value) => {
            add_field(digest, b"presence", b"present");
            add_field(digest, label, value.as_bytes());
        }
        None => add_field(digest, b"presence", b"absent"),
    }
}

fn add_u64(digest: &mut Sha256, label: &[u8], value: u64) {
    add_field(digest, label, &value.to_be_bytes());
}

fn add_field(digest: &mut Sha256, label: &[u8], value: &[u8]) {
    digest.update(u64::try_from(label.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(label);
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        Cacheability, Runtime, VolatileReason, cache_key, finalize_plans, resolve_base_plans,
    };
    use crate::environment::PromptEnvironment;
    use crate::runtime::{CachedRuntimeValue, detect, materialize};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ztheme-runtime-cache-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
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

    fn python_plan(directory: &Path, path: &str) -> super::RuntimePlan {
        let environment = PromptEnvironment {
            path: Some(path.into()),
            ..PromptEnvironment::default()
        };
        plan_for(directory, &[Runtime::Python], &environment)
            .into_iter()
            .next()
            .unwrap()
    }

    fn plan_for(
        directory: &Path,
        runtimes: &[Runtime],
        environment: &PromptEnvironment,
    ) -> Vec<super::RuntimePlan> {
        let project = detect::detect(directory, Some(directory), runtimes, environment);
        let base = resolve_base_plans(runtimes, directory, environment);
        finalize_plans(directory, &project, base, environment)
    }

    fn executable(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"native").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn script(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn path_resolution_uses_shell_empty_and_relative_entries() {
        let directory = TestDirectory::new();
        let cwd = directory.path().join("project");
        executable(&cwd.join("python"));
        executable(&cwd.join("tools/python"));

        assert_eq!(
            super::resolve_on_path(OsStr::new("python"), OsStr::new(""), &cwd),
            Some(cwd.join("python"))
        );
        assert_eq!(
            super::resolve_on_path(OsStr::new("python"), OsStr::new("tools"), &cwd),
            Some(cwd.join("tools/python"))
        );
    }

    #[test]
    fn compiler_selects_clang_before_gcc() {
        let directory = TestDirectory::new();
        let cwd = directory.path().join("project");
        let bin = directory.path().join("bin");
        executable(&bin.join("clang"));
        executable(&bin.join("gcc"));
        let environment = PromptEnvironment {
            path: Some(bin.into()),
            ..PromptEnvironment::default()
        };

        let plan = resolve_base_plans(&[Runtime::C], &cwd, &environment)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(plan.program, directory.path().join("bin/clang"));
        assert_eq!(plan.spec.label, Some("clang"));
        assert!(plan.available);
    }

    #[test]
    fn compiler_falls_back_to_executable_gcc_when_clang_is_missing_or_not_executable() {
        let directory = TestDirectory::new();
        let cwd = directory.path().join("project");
        let bin = directory.path().join("bin");
        executable(&bin.join("gcc"));
        let environment = PromptEnvironment {
            path: Some(bin.clone().into()),
            ..PromptEnvironment::default()
        };

        let missing_clang = resolve_base_plans(&[Runtime::C], &cwd, &environment)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(missing_clang.program, bin.join("gcc"));
        assert_eq!(missing_clang.spec.label, Some("gcc"));
        assert!(missing_clang.available);

        fs::write(bin.join("clang"), b"not executable").unwrap();
        let non_executable_clang = resolve_base_plans(&[Runtime::C], &cwd, &environment)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(non_executable_clang.program, bin.join("gcc"));
        assert_eq!(non_executable_clang.spec.label, Some("gcc"));
        assert!(non_executable_clang.available);
    }

    #[test]
    fn compiler_path_entries_use_the_request_cwd() {
        let directory = TestDirectory::new();
        let cwd = directory.path().join("project");
        executable(&cwd.join("clang"));
        let empty_path = PromptEnvironment {
            path: Some("".into()),
            ..PromptEnvironment::default()
        };
        let empty_entry = resolve_base_plans(&[Runtime::C], &cwd, &empty_path)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(empty_entry.program, cwd.join("clang"));

        fs::remove_file(cwd.join("clang")).unwrap();
        executable(&cwd.join("tools/clang"));
        let relative_path = PromptEnvironment {
            path: Some("tools".into()),
            ..PromptEnvironment::default()
        };
        let relative_entry = resolve_base_plans(&[Runtime::C], &cwd, &relative_path)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(relative_entry.program, cwd.join("tools/clang"));
    }

    #[test]
    fn java_and_dotnet_select_the_first_path_executable() {
        let directory = TestDirectory::new();
        let cwd = directory.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let java_home = directory.path().join("java-home");
        let dotnet_root = directory.path().join("dotnet-root");
        executable(&first.join("java"));
        executable(&second.join("java"));
        executable(&java_home.join("bin/java"));
        executable(&first.join("dotnet"));
        executable(&second.join("dotnet"));
        executable(&dotnet_root.join("dotnet"));

        let java_environment = PromptEnvironment {
            path: Some(std::env::join_paths([first.as_path(), second.as_path()]).unwrap()),
            java_home: Some(java_home.into()),
            ..PromptEnvironment::default()
        };
        fs::write(cwd.join("pom.xml"), b"<project/>\n").unwrap();
        let java = plan_for(&cwd, &[Runtime::Java], &java_environment);
        assert_eq!(java[0].program, first.join("java"));

        let dotnet_environment = PromptEnvironment {
            path: Some(std::env::join_paths([first.as_path(), second.as_path()]).unwrap()),
            dotnet_root: Some(dotnet_root.into()),
            ..PromptEnvironment::default()
        };
        fs::write(cwd.join("global.json"), b"{}\n").unwrap();
        let dotnet = plan_for(&cwd, &[Runtime::Dotnet], &dotnet_environment);
        assert_eq!(dotnet[0].program, first.join("dotnet"));
        assert!(matches!(
            dotnet[0].cache,
            Cacheability::Volatile(VolatileReason::UnsupportedContextualSelection)
        ));
    }

    #[test]
    fn relative_manager_directory_is_resolved_from_request_cwd() {
        let directory = TestDirectory::new();
        let cwd = directory.path().join("project");
        let root = directory.path().join("manager");
        fs::create_dir_all(&cwd).unwrap();
        executable(&root.join("bin/pyenv"));
        executable(&root.join("shims/python"));
        executable(&root.join("versions/3.12/bin/python"));
        fs::write(root.join("version"), b"3.11\n").unwrap();
        fs::write(root.join(".python-version"), b"3.12\n").unwrap();
        fs::write(cwd.join("pyproject.toml"), b"[]\n").unwrap();
        let environment = PromptEnvironment {
            path: Some(root.join("shims").into()),
            pyenv_dir: Some("../manager".into()),
            ..PromptEnvironment::default()
        };

        let plan = plan_for(&cwd, &[Runtime::Python], &environment);
        assert_eq!(plan[0].program, root.join("versions/3.12/bin/python"));
    }

    #[test]
    fn direct_key_ignores_an_unused_path_suffix() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let bin = directory.path().join("bin");
        let unused = directory.path().join("unused");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&unused).unwrap();
        let program = bin.join("python");
        fs::write(&program, b"native").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();

        let first = python_plan(directory.path(), bin.to_str().unwrap());
        let suffixed_path = std::env::join_paths([bin.as_path(), unused.as_path()]).unwrap();
        let second = python_plan(directory.path(), suffixed_path.to_str().unwrap());
        let environment = PromptEnvironment::default();
        let first_key = cache_key(&[first], &environment);
        let second_key = cache_key(&[second], &environment);
        assert_eq!(first_key, second_key);
    }

    #[test]
    fn direct_key_changes_when_path_selects_another_executable() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let first_bin = directory.path().join("first");
        let second_bin = directory.path().join("second");
        let first_program = first_bin.join("python");
        let second_program = second_bin.join("python");
        executable(&first_program);
        executable(&second_program);

        let first = python_plan(directory.path(), first_bin.to_str().unwrap());
        let second = python_plan(directory.path(), second_bin.to_str().unwrap());
        assert_ne!(
            cache_key(&[first], &PromptEnvironment::default()),
            cache_key(&[second], &PromptEnvironment::default())
        );
    }

    #[test]
    fn direct_key_changes_when_the_executable_is_replaced() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let bin = directory.path().join("bin");
        let program = bin.join("python");
        executable(&program);
        let first = python_plan(directory.path(), bin.to_str().unwrap());
        let first_key = cache_key(&[first], &PromptEnvironment::default());

        fs::write(&program, b"replacement-with-a-different-size").unwrap();
        let second = python_plan(directory.path(), bin.to_str().unwrap());
        let second_key = cache_key(&[second], &PromptEnvironment::default());
        assert_ne!(first_key, second_key);
    }

    #[test]
    fn python_key_ignores_rustup_selection() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let bin = directory.path().join("bin");
        executable(&bin.join("python"));
        let first = python_plan(directory.path(), bin.to_str().unwrap());
        let first_environment = PromptEnvironment {
            rustup_toolchain: Some("stable".into()),
            ..PromptEnvironment::default()
        };
        let second_environment = PromptEnvironment {
            rustup_toolchain: Some("nightly".into()),
            ..PromptEnvironment::default()
        };
        assert_eq!(
            cache_key(std::slice::from_ref(&first), &first_environment),
            cache_key(std::slice::from_ref(&first), &second_environment)
        );
    }

    #[test]
    fn presentation_environment_is_materialized_after_a_cache_hit() {
        let first_environment = PromptEnvironment {
            conda_default_env: Some("base".into()),
            ..PromptEnvironment::default()
        };
        let second_environment = PromptEnvironment {
            conda_default_env: Some("work".into()),
            ..PromptEnvironment::default()
        };
        let cached = CachedRuntimeValue {
            runtime: Runtime::Python,
            version: "3.14.6".into(),
            label: None,
        };

        let first = materialize(cached.clone(), &first_environment);
        let second = materialize(cached, &second_environment);
        assert_eq!(first.environment.as_deref(), Some("base"));
        assert_eq!(second.environment.as_deref(), Some("work"));
    }

    #[test]
    fn materializing_a_compiler_value_does_not_resolve_a_compiler() {
        let environment = PromptEnvironment {
            path: Some("missing/compiler/path".into()),
            ..PromptEnvironment::default()
        };
        let value = materialize(
            CachedRuntimeValue {
                runtime: Runtime::C,
                version: "17.0.0".into(),
                label: Some("clang".into()),
            },
            &environment,
        );
        assert_eq!(value.environment, None);
    }

    #[test]
    fn standard_pyenv_selection_uses_environment_file_global_and_system_precedence() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let root = directory.path().join("pyenv");
        executable(&root.join("bin/pyenv"));
        executable(&root.join("shims/python"));
        executable(&root.join("versions/3.11/bin/python"));
        executable(&root.join("versions/3.12/bin/python"));
        fs::write(root.join("version"), b"3.11\n").unwrap();
        let shims = root.join("shims");
        let base_environment = PromptEnvironment {
            path: Some(shims.to_string_lossy().into_owned().into()),
            ..PromptEnvironment::default()
        };

        fs::write(directory.path().join(".python-version"), b"3.12\n").unwrap();
        let file_plan = plan_for(directory.path(), &[Runtime::Python], &base_environment);
        assert_eq!(file_plan[0].program, root.join("versions/3.12/bin/python"));
        assert!(file_plan[0].is_cacheable());

        let mut environment_plan = base_environment.clone();
        environment_plan.pyenv_version = Some("3.11".into());
        let environment_plan = plan_for(directory.path(), &[Runtime::Python], &environment_plan);
        assert_eq!(
            environment_plan[0].program,
            root.join("versions/3.11/bin/python")
        );

        fs::remove_file(directory.path().join(".python-version")).unwrap();
        let global_plan = plan_for(directory.path(), &[Runtime::Python], &base_environment);
        assert_eq!(
            global_plan[0].program,
            root.join("versions/3.11/bin/python")
        );

        fs::write(root.join("version"), b"system\n").unwrap();
        let system_bin = directory.path().join("system");
        executable(&system_bin.join("python"));
        let path = std::env::join_paths([shims.as_path(), system_bin.as_path()]).unwrap();
        let system_environment = PromptEnvironment {
            path: Some(path),
            ..base_environment
        };
        let system_plan = plan_for(directory.path(), &[Runtime::Python], &system_environment);
        assert_eq!(system_plan[0].program, system_bin.join("python"));
        assert!(system_plan[0].is_cacheable());
    }

    #[test]
    fn ambiguous_pyenv_selection_is_volatile() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let root = directory.path().join("pyenv");
        executable(&root.join("bin/pyenv"));
        executable(&root.join("shims/python"));
        executable(&root.join("versions/3.11/bin/python"));
        fs::write(root.join("version"), b"3.11\n").unwrap();
        fs::write(directory.path().join(".python-version"), b"3.99\n").unwrap();
        let environment = PromptEnvironment {
            path: Some(root.join("shims").to_string_lossy().into_owned().into()),
            ..PromptEnvironment::default()
        };
        let plan = plan_for(directory.path(), &[Runtime::Python], &environment);
        assert!(matches!(
            plan[0].cache,
            Cacheability::Volatile(VolatileReason::AmbiguousSelector)
        ));
    }

    #[test]
    fn rustup_selection_uses_explicit_file_override_and_default() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("Cargo.toml"), b"[package]\n").unwrap();
        let home = directory.path().join("home");
        let cargo_bin = home.join(".cargo/bin");
        let rustup_home = home.join(".rustup");
        let proxy = cargo_bin.join("rustc");
        executable(&proxy);
        fs::remove_file(&proxy).unwrap();
        executable(&cargo_bin.join("rustup"));
        fs::hard_link(cargo_bin.join("rustup"), &proxy).unwrap();
        executable(&rustup_home.join("toolchains/stable/bin/rustc"));
        executable(&rustup_home.join("toolchains/nightly/bin/rustc"));
        fs::write(
            rustup_home.join("settings.toml"),
            "default_toolchain = \"stable\"\n",
        )
        .unwrap();
        let base = PromptEnvironment {
            home: Some(home.clone().into()),
            path: Some(cargo_bin.to_string_lossy().into_owned().into()),
            rustup_home: Some(rustup_home.clone().into()),
            ..PromptEnvironment::default()
        };

        let mut explicit = base.clone();
        explicit.rustup_toolchain = Some("nightly".into());
        let explicit_plan = plan_for(directory.path(), &[Runtime::Rust], &explicit);
        assert_eq!(
            explicit_plan[0].program,
            rustup_home.join("toolchains/nightly/bin/rustc")
        );

        fs::write(directory.path().join("rust-toolchain"), b"nightly\n").unwrap();
        let file_plan = plan_for(directory.path(), &[Runtime::Rust], &base);
        assert_eq!(
            file_plan[0].program,
            rustup_home.join("toolchains/nightly/bin/rustc")
        );

        fs::remove_file(directory.path().join("rust-toolchain")).unwrap();
        fs::write(
            rustup_home.join("settings.toml"),
            format!(
                "default_toolchain = \"stable\"\n[overrides]\n\"{}\" = \"nightly\"\n",
                directory.path().display()
            ),
        )
        .unwrap();
        let override_plan = plan_for(directory.path(), &[Runtime::Rust], &base);
        assert_eq!(
            override_plan[0].program,
            rustup_home.join("toolchains/nightly/bin/rustc")
        );

        fs::write(
            rustup_home.join("settings.toml"),
            "default_toolchain = \"stable\"\n",
        )
        .unwrap();
        let default_plan = plan_for(directory.path(), &[Runtime::Rust], &base);
        assert_eq!(
            default_plan[0].program,
            rustup_home.join("toolchains/stable/bin/rustc")
        );
    }

    #[test]
    fn rustup_selection_prefers_the_closest_selector_and_plain_file() {
        let directory = TestDirectory::new();
        let cwd = directory.path().join("nested/child");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(directory.path().join("Cargo.toml"), b"[package]\n").unwrap();
        fs::write(cwd.join("Cargo.toml"), b"[package]\n").unwrap();
        let home = directory.path().join("home");
        let cargo_bin = home.join(".cargo/bin");
        let rustup_home = home.join(".rustup");
        executable(&cargo_bin.join("rustup"));
        fs::hard_link(cargo_bin.join("rustup"), cargo_bin.join("rustc")).unwrap();
        for name in ["stable", "nightly", "beta"] {
            executable(&rustup_home.join(format!("toolchains/{name}/bin/rustc")));
        }
        fs::write(
            rustup_home.join("settings.toml"),
            format!(
                "default_toolchain = \"stable\"\n[overrides]\n\"{}\" = \"nightly\"\n",
                directory.path().display()
            ),
        )
        .unwrap();
        let base = PromptEnvironment {
            home: Some(home.clone().into()),
            path: Some(cargo_bin.clone().into()),
            rustup_home: Some(rustup_home.clone().into()),
            ..PromptEnvironment::default()
        };

        // A closer file beats a farther directory override.
        fs::write(cwd.join("rust-toolchain"), b"stable\n").unwrap();
        let plan = plan_for(&cwd, &[Runtime::Rust], &base);
        assert_eq!(
            plan[0].program,
            rustup_home.join("toolchains/stable/bin/rustc")
        );

        // A closer directory override beats a farther toolchain file.
        fs::remove_file(cwd.join("rust-toolchain")).unwrap();
        fs::write(directory.path().join("rust-toolchain"), b"stable\n").unwrap();
        fs::write(
            rustup_home.join("settings.toml"),
            format!(
                "default_toolchain = \"stable\"\n[overrides]\n\"{}\" = \"nightly\"\n",
                cwd.display()
            ),
        )
        .unwrap();
        let plan = plan_for(&cwd, &[Runtime::Rust], &base);
        assert_eq!(
            plan[0].program,
            rustup_home.join("toolchains/nightly/bin/rustc")
        );

        // At one directory, an override wins both files, and plain wins TOML
        // when the override is removed.
        fs::write(cwd.join("rust-toolchain"), b"nightly\n").unwrap();
        fs::write(
            cwd.join("rust-toolchain.toml"),
            b"[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        fs::write(
            rustup_home.join("settings.toml"),
            format!(
                "default_toolchain = \"stable\"\n[overrides]\n\"{}\" = \"beta\"\n",
                cwd.display()
            ),
        )
        .unwrap();
        let plan = plan_for(&cwd, &[Runtime::Rust], &base);
        assert_eq!(
            plan[0].program,
            rustup_home.join("toolchains/beta/bin/rustc")
        );

        fs::write(
            rustup_home.join("settings.toml"),
            "default_toolchain = \"stable\"\n",
        )
        .unwrap();
        let plan = plan_for(&cwd, &[Runtime::Rust], &base);
        assert_eq!(
            plan[0].program,
            rustup_home.join("toolchains/nightly/bin/rustc")
        );

        // More than one bounded prefix match is not cacheable.
        fs::remove_file(cwd.join("rust-toolchain")).unwrap();
        fs::remove_file(cwd.join("rust-toolchain.toml")).unwrap();
        fs::remove_file(directory.path().join("rust-toolchain")).unwrap();
        fs::write(
            rustup_home.join("settings.toml"),
            "default_toolchain = \"beta\"\n",
        )
        .unwrap();
        fs::remove_dir_all(rustup_home.join("toolchains/beta")).unwrap();
        executable(&rustup_home.join("toolchains/beta-one/bin/rustc"));
        executable(&rustup_home.join("toolchains/beta-two/bin/rustc"));
        let plan = plan_for(&cwd, &[Runtime::Rust], &base);
        assert!(
            matches!(
                plan[0].cache,
                Cacheability::Volatile(VolatileReason::AmbiguousSelector)
            ),
            "unexpected rustup plan: {:?}",
            plan[0]
        );
    }

    #[test]
    fn new_runtimes_volatility_and_cacheability() {
        let directory = TestDirectory::new();
        for marker in [
            "DESCRIPTION",
            "Project.toml",
            "mix.exs",
            "pubspec.yaml",
            "stack.yaml",
            "build.zig",
        ] {
            fs::write(directory.path().join(marker), b"").unwrap();
        }
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for program in ["R", "julia", "elixir", "dart", "ghc", "zig"] {
            executable(&bin.join(program));
        }
        let environment = PromptEnvironment {
            path: Some(bin.clone().into()),
            ..PromptEnvironment::default()
        };

        // R, Julia, Elixir, and Haskell stay volatile while their caching
        // model is being evaluated.
        for runtime in [
            Runtime::R,
            Runtime::Julia,
            Runtime::Elixir,
            Runtime::Haskell,
        ] {
            let plan = plan_for(directory.path(), &[runtime], &environment)
                .into_iter()
                .next()
                .unwrap();
            assert!(
                matches!(
                    plan.cache,
                    Cacheability::Volatile(VolatileReason::NotYetCacheable)
                ),
                "unexpected plan for {}: {:?}",
                runtime.name(),
                plan
            );
        }

        // Direct native Dart and Zig SDK executables are identity-stable and
        // therefore cacheable.
        for runtime in [Runtime::Dart, Runtime::Zig] {
            let plan = plan_for(directory.path(), &[runtime], &environment)
                .into_iter()
                .next()
                .unwrap();
            assert!(
                matches!(plan.cache, Cacheability::Cacheable(_)),
                "unexpected plan for {}: {:?}",
                runtime.name(),
                plan
            );
        }

        // Script-based Dart/Zig frontends (e.g. Flutter or version-manager
        // wrappers) stay volatile through shebang detection.
        script(&bin.join("dart"));
        script(&bin.join("zig"));
        for runtime in [Runtime::Dart, Runtime::Zig] {
            let plan = plan_for(directory.path(), &[runtime], &environment)
                .into_iter()
                .next()
                .unwrap();
            assert!(
                matches!(
                    plan.cache,
                    Cacheability::Volatile(VolatileReason::ScriptWrapper)
                ),
                "unexpected plan for {}: {:?}",
                runtime.name(),
                plan
            );
        }
    }

    #[test]
    fn dotnet_projects_are_volatile_without_contextual_cache_state() {
        let directory = TestDirectory::new();
        let root = directory.path().join("dotnet");
        executable(&root.join("dotnet"));
        fs::write(directory.path().join("global.json"), b"{}\n").unwrap();
        let environment = PromptEnvironment {
            path: Some(root.to_string_lossy().into_owned().into()),
            dotnet_root: Some(directory.path().join("other-root").into()),
            ..PromptEnvironment::default()
        };
        let plan = plan_for(directory.path(), &[Runtime::Dotnet], &environment);
        assert_eq!(plan[0].program, root.join("dotnet"));
        assert!(matches!(
            plan[0].cache,
            Cacheability::Volatile(VolatileReason::UnsupportedContextualSelection)
        ));
    }

    #[test]
    fn manager_selected_scripts_are_volatile_and_keep_the_original_shim() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let root = directory.path().join("pyenv");
        executable(&root.join("bin/pyenv"));
        script(&root.join("shims/python"));
        script(&root.join("versions/3.12/bin/python"));
        fs::write(root.join("version"), b"3.12\n").unwrap();
        let environment = PromptEnvironment {
            path: Some(root.join("shims").into()),
            ..PromptEnvironment::default()
        };

        let plan = plan_for(directory.path(), &[Runtime::Python], &environment);
        assert_eq!(plan[0].program, root.join("shims/python"));
        assert!(matches!(
            plan[0].cache,
            Cacheability::Volatile(VolatileReason::ScriptWrapper)
        ));
    }

    #[test]
    fn rustup_selected_scripts_are_volatile_and_keep_the_original_proxy() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("Cargo.toml"), b"[package]\n").unwrap();
        let home = directory.path().join("home");
        let cargo_bin = home.join(".cargo/bin");
        let rustup_home = home.join(".rustup");
        executable(&cargo_bin.join("rustup"));
        fs::hard_link(cargo_bin.join("rustup"), cargo_bin.join("rustc")).unwrap();
        fs::create_dir_all(&rustup_home).unwrap();
        fs::write(
            rustup_home.join("settings.toml"),
            "default_toolchain = \"stable\"\n",
        )
        .unwrap();
        script(&rustup_home.join("toolchains/stable/bin/rustc"));
        let environment = PromptEnvironment {
            home: Some(home.clone().into()),
            path: Some(cargo_bin.into()),
            rustup_home: Some(rustup_home.into()),
            ..PromptEnvironment::default()
        };

        let plan = plan_for(directory.path(), &[Runtime::Rust], &environment);
        assert_eq!(plan[0].program, home.join(".cargo/bin/rustc"));
        assert!(matches!(
            plan[0].cache,
            Cacheability::Volatile(VolatileReason::ScriptWrapper)
        ));
    }

    #[test]
    fn unknown_shims_are_volatile() {
        let directory = TestDirectory::new();
        fs::write(directory.path().join("pyproject.toml"), b"[project]\n").unwrap();
        let root = directory.path().join("manager");
        let shims = root.join("shims");
        fs::create_dir_all(&shims).unwrap();
        let shim = shims.join("python");
        fs::write(&shim, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
        let environment = PromptEnvironment {
            path: Some(shims.to_string_lossy().into_owned().into()),
            ..PromptEnvironment::default()
        };
        let project = detect::detect(
            directory.path(),
            Some(directory.path()),
            &[Runtime::Python],
            &environment,
        );
        let base = resolve_base_plans(&[Runtime::Python], directory.path(), &environment);
        let plan = finalize_plans(directory.path(), &project, base, &environment)
            .into_iter()
            .next()
            .unwrap();
        assert!(matches!(
            plan.cache,
            Cacheability::Volatile(
                VolatileReason::UnknownShim
                    | VolatileReason::ScriptWrapper
                    | VolatileReason::MissingExecutable
            )
        ));
    }
}
