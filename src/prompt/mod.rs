mod client;
mod planning;
mod protocol;

pub(crate) use client::serve_client;

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};

use crate::cache::Acquire;
use crate::environment::PromptEnvironment;
use crate::runtime::{self, Runtime, RuntimeOutcome, RuntimeValue};
use crate::{daemon, gitstatus, setup, theme};

pub(crate) use protocol::prompt_text;

const REQUEST_TIMEOUT: Duration = Duration::from_millis(550);
/// Bounds one Git exchange from its start. The request's `done` does not wait
/// for Git: a slow fresh result is still written for its generation, after
/// `done`, until this cap or a newer request supersedes it.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const ZSH_DEFAULTS: &str = include_str!("../../shell/defaults.zsh");
const ZSH_INTEGRATION: &str = include_str!("../../shell/ztheme.zsh");
const ZSH_DIRECTORY_SEGMENT: &str = include_str!("../../shell/segments/directory.zsh");
const ZSH_CLOCK_SEGMENT: &str = include_str!("../../shell/segments/clock.zsh");
const ZSH_STATUS_SEGMENT: &str = include_str!("../../shell/segments/status.zsh");
const ZSH_CHARACTER_SEGMENT: &str = include_str!("../../shell/segments/character.zsh");

/// The repository whose Git value the shell holds, shared by one client's
/// sequential requests: `None` when it holds no value or the client cannot
/// tell. The shell keeps a Git value across prompts and directory changes, so
/// a request for another repository must withdraw it before its own query
/// finishes. Updated only as records are written, so it never runs ahead of
/// what the shell received.
#[derive(Clone, Default)]
struct ShownRepository(Arc<Mutex<Option<gitstatus::Query>>>);

impl ShownRepository {
    fn holds(&self, query: Option<&gitstatus::Query>) -> bool {
        self.lock().as_ref() == query
    }

    fn set(&self, query: Option<gitstatus::Query>) {
        *self.lock() = query;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<gitstatus::Query>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Inputs shared by every request a client serves.
struct ClientState {
    instance: daemon::Instance,
    theme: Arc<theme::AsyncTheme>,
    executor: planning::PlanningExecutor,
    shown: ShownRepository,
}

async fn snapshot(
    generation: u64,
    cwd: PathBuf,
    environment: Arc<PromptEnvironment>,
    state: &ClientState,
) -> io::Result<()> {
    let theme = &*state.theme;
    let active_runtimes = theme.runtimes();
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    // Separate sets so the request deadline can stop runtime work while Git
    // continues; dropping this future (a newer request) aborts both.
    let mut git_task = JoinSet::new();
    let mut runtime_task = JoinSet::new();
    // The coordinator owns the only receiver. At most one completion per
    // selected runtime, a clear, a stale and a fresh Git result, and group
    // markers can be buffered for this generation.
    let (events, mut completions) = mpsc::channel(Runtime::ALL.len() + 5);

    if theme.git_enabled() {
        let started = spawn_git(
            &mut git_task,
            state,
            cwd.clone(),
            Arc::clone(&environment),
            events.clone(),
        );
        let _ = timeout_at(deadline, started).await;
    }

    if !active_runtimes.is_empty() {
        let instance = state.instance.clone();
        let request = planning::PlanningRequest {
            cwd,
            active: active_runtimes.clone(),
            environment,
            executor: state.executor.clone(),
        };
        let events = events.clone();
        runtime_task.spawn(async move {
            stream_runtimes(&instance, request, events).await;
        });
    }
    drop(events);

    let mut git_pending = !git_task.is_empty();
    let mut receiving = true;
    while receiving || !git_task.is_empty() || !runtime_task.is_empty() {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            event = completions.recv(), if receiving => {
                if let Some(event) = event {
                    git_pending &= !event.finishes_git();
                    write_result(event, generation, theme, &state.shown)?;
                } else {
                    receiving = false;
                }
            }
            Some(result) = git_task.join_next(), if !git_task.is_empty() => {
                write_join_error(result, generation)?;
            }
            Some(result) = runtime_task.join_next(), if !runtime_task.is_empty() => {
                write_join_error(result, generation)?;
            }
        }
    }

    // The deadline ends runtime work and the prompt's main phase. Git alone
    // may still be outstanding; its fresh result follows `done` for this same
    // generation, bounded by its own cap (runtime values queued before the
    // deadline are written on the way). A Git task that failed without a
    // final event closes the channel once the runtime task is gone.
    drop(runtime_task);
    protocol::write_done(&mut io::stdout().lock(), generation)?;
    while git_pending {
        let Some(event) = completions.recv().await else {
            break;
        };
        git_pending = !event.finishes_git();
        write_result(event, generation, theme, &state.shown)?;
    }
    Ok(())
}

fn write_join_error(result: Result<(), tokio::task::JoinError>, generation: u64) -> io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => protocol::write_error(
            &mut io::stdout().lock(),
            generation,
            "snapshot",
            &record_error(&io::Error::other(error)),
        ),
    }
}

/// Starts the Git task, which always ends with one final Git event, within
/// `GIT_TIMEOUT`. The receiver resolves once the task runs, so runtime work
/// starts after the Git request does.
fn spawn_git(
    tasks: &mut JoinSet<()>,
    state: &ClientState,
    cwd: PathBuf,
    environment: Arc<PromptEnvironment>,
    events: mpsc::Sender<SnapshotResult>,
) -> oneshot::Receiver<()> {
    let (started_tx, started_rx) = oneshot::channel();
    let instance = state.instance.clone();
    let executor = state.executor.clone();
    let shown = state.shown.clone();
    tasks.spawn(async move {
        let _ = started_tx.send(());
        let exchange = git_exchange(&instance, cwd, environment, &executor, &shown, &events);
        let result = tokio::time::timeout(GIT_TIMEOUT, exchange)
            .await
            .unwrap_or_else(|_| {
                let error = io::Error::new(io::ErrorKind::TimedOut, "Git status timed out");
                SnapshotResult::Git(None, Err(error))
            });
        let _ = events.send(result).await;
    });
    started_rx
}

/// Discovers the repository and runs its status exchange, returning the
/// final Git event. Intermediate events (a withdrawn value, a cached value)
/// are sent as they happen.
async fn git_exchange(
    instance: &daemon::Instance,
    cwd: PathBuf,
    environment: Arc<PromptEnvironment>,
    executor: &planning::PlanningExecutor,
    shown: &ShownRepository,
    events: &mpsc::Sender<SnapshotResult>,
) -> SnapshotResult {
    let query = match executor.git_query(cwd, environment).await {
        Ok(Some(query)) => query,
        // Outside a repository the empty final value replaces any old one.
        Ok(None) => return SnapshotResult::Git(None, Ok(None)),
        Err(error) => {
            if !shown.holds(None) {
                let _ = events.send(SnapshotResult::GitCleared).await;
            }
            return SnapshotResult::Git(None, Err(error));
        }
    };
    if !shown.holds(Some(&query)) {
        let _ = events.send(SnapshotResult::GitCleared).await;
    }
    let deadline = Instant::now() + GIT_TIMEOUT;
    match daemon::git_status(instance, &query, deadline).await {
        Ok(daemon::GitStatus::Fresh(snapshot)) => SnapshotResult::Git(Some(query), Ok(snapshot)),
        Ok(daemon::GitStatus::Stale { cached, pending }) => {
            let _ = events
                .send(SnapshotResult::GitStale(query.clone(), cached))
                .await;
            match pending.fresh(deadline).await {
                // The cached value stays on screen; a fresh result that never
                // came is not worth a message.
                Err(error) if error.kind() == io::ErrorKind::TimedOut => SnapshotResult::GitKept,
                result => SnapshotResult::Git(Some(query), result),
            }
        }
        Err(error) => SnapshotResult::Git(Some(query), Err(error)),
    }
}

/// Renders one typed completion immediately. Runtime group completion is a
/// separate event, sent only after every independent runtime owner has finished.
/// Errors leave the shell's current value in place: only a value replaces a
/// value.
fn write_result(
    result: SnapshotResult,
    generation: u64,
    theme: &theme::AsyncTheme,
    shown: &ShownRepository,
) -> io::Result<()> {
    let mut output = io::stdout().lock();
    match result {
        SnapshotResult::GitCleared => {
            protocol::write_segment(&mut output, generation, "git", "")?;
            shown.set(None);
            Ok(())
        }
        // A cached value renders like any Git segment but leaves the group
        // open: the fresh result follows with the group's `complete`.
        SnapshotResult::GitStale(query, snapshot) => {
            protocol::write_segment(
                &mut output,
                generation,
                "git",
                &theme.render_git(snapshot.as_ref()),
            )?;
            shown.set(Some(query));
            Ok(())
        }
        SnapshotResult::Git(query, Ok(snapshot)) => {
            protocol::write_segment(
                &mut output,
                generation,
                "git",
                &theme.render_git(snapshot.as_ref()),
            )?;
            shown.set(query);
            // Each group finishes with a `complete` marker so the shell can
            // stop waiting for that group as soon as it is done.
            protocol::write_complete(&mut output, generation, "git")
        }
        SnapshotResult::Git(_, Err(error)) => {
            protocol::write_error(&mut output, generation, "git", &record_error(&error))?;
            protocol::write_complete(&mut output, generation, "git")
        }
        SnapshotResult::GitKept => protocol::write_complete(&mut output, generation, "git"),
        SnapshotResult::Runtime { runtime, value } => match value {
            Ok(value) => {
                let fragment = value
                    .as_ref()
                    .and_then(|value| theme.render_runtime(value))
                    .unwrap_or_default();
                protocol::write_segment(&mut output, generation, runtime.name(), &fragment)
            }
            // A per-runtime failure must not clear completed siblings.
            Err(error) => protocol::write_error(
                &mut output,
                generation,
                runtime.name(),
                &record_error(&error),
            ),
        },
        SnapshotResult::RuntimeComplete => {
            protocol::write_complete(&mut output, generation, "runtime")
        }
        SnapshotResult::RuntimePlanningFailed(error) => {
            protocol::write_error(&mut output, generation, "runtime", &record_error(&error))?;
            protocol::write_complete(&mut output, generation, "runtime")
        }
    }
}

pub fn init_zsh(instance: &daemon::Instance, selector: Option<&str>) -> io::Result<String> {
    let theme = theme::CompiledTheme::load(selector)?;
    let theme_zsh = theme.zsh()?;
    // gitstatusd is required only when the selected layout actually includes a
    // Git segment; a runtime-only or fully synchronous theme initializes
    // without the managed binary.
    if theme.git_enabled() && !gitstatus::ensure_installed(false)? {
        return Err(io::Error::other(
            "gitstatusd is required; initialization skipped (`ztheme setup --yes`)",
        ));
    }
    let binary = env::current_exe()?;
    let binary = shell_quote(&binary.to_string_lossy());
    let instance_arguments = instance
        .development_name()
        .map_or_else(String::new, |name| format!("--dev {}", shell_quote(name)));
    // Discover and validate enabled custom segments against config.toml and
    // the segments directory; this is the only point that touches the
    // filesystem or the config outside the prompt hot path.
    let custom_segments = theme.custom_sources()?;
    let modes = theme::async_modes()?;
    let bundled = format!(
        "{ZSH_DIRECTORY_SEGMENT}{ZSH_CLOCK_SEGMENT}{ZSH_STATUS_SEGMENT}{ZSH_CHARACTER_SEGMENT}"
    );
    Ok(ZSH_INTEGRATION
        .replace("@ZTHEME_BIN@", &binary)
        .replace("@ZTHEME_INSTANCE_ARGS@", &instance_arguments)
        .replace(
            "@ZTHEME_AUTOSUGGESTIONS@",
            &shell_quote(&setup::autosuggestions_script().to_string_lossy()),
        )
        .replace(
            "@ZTHEME_SYNTAX_HIGHLIGHTING@",
            &shell_quote(&setup::syntax_highlighting_script().to_string_lossy()),
        )
        .replace("@ZTHEME_MODE_GIT@", modes.git.name())
        .replace("@ZTHEME_MODE_RUNTIME@", modes.runtime.name())
        .replace("@ZTHEME_ASYNC_BUDGET_MS@", &modes.budget_ms.to_string())
        .replace("@ZTHEME_SHELL_DEFAULTS@", ZSH_DEFAULTS)
        .replace("@ZTHEME_COMPILED_THEME@", &theme_zsh)
        .replace("@ZTHEME_BUNDLED_SEGMENTS@", &bundled)
        .replace(
            "@ZTHEME_CUSTOM_SEGMENTS@",
            &custom_segment_block(&custom_segments),
        )
        .replace("@ZTHEME_REQUEST_VERSION@", protocol::REQUEST_VERSION)
        .replace(
            "    @ZTHEME_REQUEST_FIELDS@",
            &protocol::request_field_lines(),
        )
        .replace(
            "    @ZTHEME_CONTEXT_FIELDS@",
            &protocol::context_field_lines(),
        ))
}

/// Sources each custom file with its old symbol absent, then stages only the
/// freshly declared function. The shell preparation wrapper restores current
/// functions and owned state before installing any staged definitions. Paths
/// use shared shell quoting; ids are validated identifiers.
fn custom_segment_block(sources: &[theme::ResolvedCustomSegment]) -> String {
    let mut output = String::new();
    for source in sources {
        writeln!(
            output,
            "builtin unfunction ztheme_segment_{} 2>/dev/null",
            source.id
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "if ! builtin source -- {}; then",
            shell_quote(&source.path.to_string_lossy())
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "    builtin print -u2 -r -- {}",
            shell_quote(&format!(
                "ztheme: failed to source custom segment `{}`",
                source.id
            ))
        )
        .expect("writing to a String cannot fail");
        writeln!(output, "    return 1\nfi").expect("writing to a String cannot fail");
        writeln!(
            output,
            "if (( ! $+functions[ztheme_segment_{}] )); then",
            source.id
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "    builtin print -u2 -r -- {}",
            shell_quote(&format!(
                "ztheme: custom segment `{}` did not define ztheme_segment_{}",
                source.id, source.id
            ))
        )
        .expect("writing to a String cannot fail");
        writeln!(output, "    return 1").expect("writing to a String cannot fail");
        writeln!(output, "fi").expect("writing to a String cannot fail");
        writeln!(
            output,
            "ztheme_custom_definitions[ztheme_segment_{}]=$functions[ztheme_segment_{}]",
            source.id, source.id
        )
        .expect("writing to a String cannot fail");
    }
    output
}

pub fn theme_zsh(instance: &daemon::Instance, selector: &str, persist: bool) -> io::Result<String> {
    let script = init_zsh(instance, Some(selector))?;
    if persist {
        theme::persist(selector)?;
    }
    Ok(script)
}

enum SnapshotResult {
    /// The shell's Git value may belong to another repository: withdraw it.
    GitCleared,
    /// The daemon's cached value for this repository, before the fresh one.
    GitStale(gitstatus::Query, Option<gitstatus::Snapshot>),
    /// The final Git result; the query is the repository it describes.
    Git(
        Option<gitstatus::Query>,
        io::Result<Option<gitstatus::Snapshot>>,
    ),
    /// Final: the fresh result timed out behind a cached value already sent.
    GitKept,
    Runtime {
        runtime: Runtime,
        value: io::Result<Option<RuntimeValue>>,
    },
    RuntimeComplete,
    RuntimePlanningFailed(io::Error),
}

impl SnapshotResult {
    const fn finishes_git(&self) -> bool {
        matches!(self, Self::Git(..) | Self::GitKept)
    }
}

async fn stream_runtimes(
    instance: &daemon::Instance,
    request: planning::PlanningRequest,
    events: mpsc::Sender<SnapshotResult>,
) {
    // Detect the project once. Every selected runtime then owns its acquire,
    // command, retry, and persistence independently of its siblings.
    let plans = match request.build().await {
        Ok(plans) => plans,
        Err(error) => {
            let _ = events
                .send(SnapshotResult::RuntimePlanningFailed(error))
                .await;
            return;
        }
    };
    for runtime in &request.active {
        if !plans.iter().any(|plan| plan.runtime == *runtime)
            && events
                .send(SnapshotResult::Runtime {
                    runtime: *runtime,
                    value: Ok(None),
                })
                .await
                .is_err()
        {
            return;
        }
    }
    let mut tasks = JoinSet::new();
    let mut owners = HashMap::new();
    for plan in plans {
        let runtime = plan.runtime;
        let instance = instance.clone();
        let request = request.clone();
        let owner = tasks.spawn(async move {
            runtime_execution(&instance, &request, plan)
                .await
                .map(|execution| execution_value(execution, &request.environment))
        });
        owners.insert(owner.id(), runtime);
    }

    while let Some(result) = tasks.join_next_with_id().await {
        let (id, value) = match result {
            Ok((id, Ok(Some(value)))) => (id, Ok(Some(value))),
            // A transient failure sends nothing: the shell keeps the value
            // it shows rather than erasing it for one failed command.
            Ok((id, Ok(None))) => {
                owners.remove(&id);
                continue;
            }
            Ok((id, Err(error))) => (id, Err(error)),
            Err(error) => (error.id(), Err(io::Error::other(error))),
        };
        let runtime = owners.remove(&id).expect("every runtime task has an owner");
        if events
            .send(SnapshotResult::Runtime { runtime, value })
            .await
            .is_err()
        {
            return;
        }
    }
    let _ = events.send(SnapshotResult::RuntimeComplete).await;
}

async fn runtime_execution(
    instance: &daemon::Instance,
    request: &planning::PlanningRequest,
    mut plan: runtime::cache::RuntimePlan,
) -> io::Result<runtime::RuntimeExecution> {
    // One selection retry belongs to this runtime, not to the whole prompt.
    for attempt in 0..=1 {
        let Some(key) = runtime::cache::cache_key(&plan, &request.environment) else {
            return Ok(runtime::execute_plan(plan, &request.cwd, &request.environment).await);
        };
        let acquired = acquire_runtime(instance, key, plan.runtime).await;
        match acquired {
            Ok(RuntimeAcquire::Hit(value)) => {
                return Ok(runtime::RuntimeExecution {
                    runtime: plan.runtime,
                    outcome: RuntimeOutcome::Value(value),
                });
            }
            Ok(RuntimeAcquire::Owner(token)) => {
                let execution =
                    runtime::execute_plan(plan.clone(), &request.cwd, &request.environment).await;
                let RuntimeOutcome::Value(value) = &execution.outcome else {
                    let _ = daemon::runtime_cache_release(instance, key, token).await;
                    return Ok(execution);
                };
                let refreshed = match request.refresh(plan.runtime).await {
                    Ok(refreshed) => refreshed,
                    Err(error) => {
                        let _ = daemon::runtime_cache_release(instance, key, token).await;
                        return Err(error);
                    }
                };
                if runtime::cache::cache_key(&refreshed, &request.environment) != Some(key) {
                    let _ = daemon::runtime_cache_release(instance, key, token).await;
                    if attempt == 0 {
                        plan = refreshed;
                        continue;
                    }
                    // Neither command proved a stable selection. Do not render
                    // an old selected version as the current one or cache it.
                    return Ok(runtime::RuntimeExecution {
                        runtime: plan.runtime,
                        outcome: RuntimeOutcome::TransientFailure,
                    });
                }
                let encoded = match runtime::encode(std::slice::from_ref(value)) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        let _ = daemon::runtime_cache_release(instance, key, token).await;
                        return Err(error);
                    }
                };
                let _ = daemon::runtime_cache_put_owned(instance, key, token, &encoded).await;
                return Ok(execution);
            }
            Err(error) => {
                eprintln!("ztheme: runtime cache unavailable: {error}");
                return Ok(runtime::execute_plan(plan, &request.cwd, &request.environment).await);
            }
        }
    }
    unreachable!("the final selection attempt always returns")
}

/// Validate the opaque cache boundary, including the singleton's stable ID.
/// A corrupt entry is removed and reacquired once; transport/corruption failure
/// then falls back to uncached execution for this runtime only.
async fn acquire_runtime(
    instance: &daemon::Instance,
    key: crate::cache::CacheKey,
    runtime: Runtime,
) -> io::Result<RuntimeAcquire> {
    for attempt in 0..=1 {
        match daemon::runtime_cache_acquire(instance, key).await? {
            Acquire::Owner(token) => return Ok(RuntimeAcquire::Owner(token)),
            Acquire::Hit(encoded) => match decode_runtime(&encoded, runtime) {
                Ok(value) => return Ok(RuntimeAcquire::Hit(value)),
                Err(_) if attempt == 0 => {
                    let _ = daemon::runtime_cache_remove(instance, key).await;
                }
                Err(error) => return Err(error),
            },
        }
    }
    unreachable!("the final acquire attempt always returns")
}

enum RuntimeAcquire {
    Hit(runtime::CachedRuntimeValue),
    Owner(u64),
}

fn decode_runtime(encoded: &[u8], runtime: Runtime) -> io::Result<runtime::CachedRuntimeValue> {
    let mut values = runtime::decode(encoded)?;
    if values.len() != 1 || values[0].runtime != runtime {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime cache value does not match the requested runtime",
        ));
    }
    Ok(values.pop().expect("singleton cache value verified"))
}

fn execution_value(
    execution: runtime::RuntimeExecution,
    environment: &PromptEnvironment,
) -> Option<RuntimeValue> {
    match execution.outcome {
        RuntimeOutcome::Value(value) => Some(runtime::materialize(value, environment)),
        // Keep the symbol/name when detected but not installed.
        RuntimeOutcome::MissingExecutable => Some(RuntimeValue {
            runtime: execution.runtime,
            version: None,
            label: None,
            environment: None,
        }),
        // No value and no record: the shell keeps what it shows.
        RuntimeOutcome::TransientFailure => None,
    }
}

fn record_error(error: &io::Error) -> String {
    error
        .to_string()
        .chars()
        .map(|character| {
            if matches!(character, '\t' | '\r' | '\n') || character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ZSH_INTEGRATION, init_zsh};
    use crate::daemon::Instance;
    use crate::environment::REQUEST_FIELDS;
    use crate::prompt::protocol::{CONTEXT_EXCLUDED, REQUEST_VERSION};

    #[test]
    fn runtime_cache_boundary_requires_one_value_with_the_selected_stable_id() {
        use crate::runtime::{CachedRuntimeValue, Runtime};

        let value = |runtime| CachedRuntimeValue {
            runtime,
            version: "1.2.3".to_owned(),
            label: None,
        };
        let singleton = crate::runtime::encode(&[value(Runtime::Node)]).unwrap();
        assert_eq!(
            super::decode_runtime(&singleton, Runtime::Node)
                .unwrap()
                .runtime,
            Runtime::Node
        );
        assert!(super::decode_runtime(&singleton, Runtime::Python).is_err());
        assert!(
            super::decode_runtime(&crate::runtime::encode(&[]).unwrap(), Runtime::Node).is_err()
        );
        let aggregate =
            crate::runtime::encode(&[value(Runtime::Node), value(Runtime::Python)]).unwrap();
        assert!(super::decode_runtime(&aggregate, Runtime::Node).is_err());
    }

    /// A scratch directory for init-time artifacts, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ztheme-prompt-protocol-tests-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn shell_template_carries_the_request_protocol_placeholders() {
        for token in [
            "@ZTHEME_REQUEST_VERSION@",
            "@ZTHEME_REQUEST_FIELDS@",
            "@ZTHEME_CONTEXT_FIELDS@",
        ] {
            assert!(ZSH_INTEGRATION.contains(token), "missing {token}");
        }
    }

    #[test]
    fn generated_shell_derives_the_request_protocol_from_the_central_definition() {
        let scratch = Scratch::new();
        let theme = scratch.0.join("theme.toml");
        // A runtime-free layout avoids the gitstatusd prerequisite in init_zsh.
        std::fs::write(
            &theme,
            "version = 1\n[layout]\nlines = [[\"directory\"]]\nright = []\nseparator = \" | \"\nblank_line_before = false\n",
        )
        .unwrap();
        let script = init_zsh(
            &Instance::Development("protocol-test".to_owned()),
            Some(theme.to_str().unwrap()),
        )
        .unwrap();

        let version_line = format!("\"ZTREQ\"$'\\0'\"{REQUEST_VERSION}\"$'\\0'");
        assert!(script.contains(&version_line), "version not spliced");

        for field in REQUEST_FIELDS {
            let line = format!("request_line+=\"${{{}:-}}\"$'\\0'", field.name);
            assert!(script.contains(&line), "missing request field line {line}");
        }
        for field in REQUEST_FIELDS
            .iter()
            .map(|field| field.name)
            .filter(|field| !CONTEXT_EXCLUDED.contains(field))
        {
            let line = format!("context_key+=\"|${{{field}:-}}\"");
            assert!(script.contains(&line), "missing context field line {line}");
        }
        assert!(script.contains("context_key+=\"|${NVM_BIN:-}|$PATH\""));
        // Independent contract checks: these inputs affect selection even
        // though PATH has a separate raw suffix in the key.
        assert!(script.contains("context_key+=\"|${HOME:-}\""));
        assert!(script.contains("context_key+=\"|${GIT_CEILING_DIRECTORIES:-}\""));

        for token in [
            "@ZTHEME_REQUEST_VERSION@",
            "@ZTHEME_REQUEST_FIELDS@",
            "@ZTHEME_CONTEXT_FIELDS@",
        ] {
            assert!(!script.contains(token), "placeholder leaked: {token}");
        }
    }
}
