mod client;
mod planning;
mod protocol;

pub(crate) use client::serve_client;

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
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
const ZSH_DEFAULTS: &str = include_str!("../../shell/defaults.zsh");
const ZSH_INTEGRATION: &str = include_str!("../../shell/ztheme.zsh");
const ZSH_DIRECTORY_SEGMENT: &str = include_str!("../../shell/segments/directory.zsh");
const ZSH_CLOCK_SEGMENT: &str = include_str!("../../shell/segments/clock.zsh");
const ZSH_STATUS_SEGMENT: &str = include_str!("../../shell/segments/status.zsh");
const ZSH_CHARACTER_SEGMENT: &str = include_str!("../../shell/segments/character.zsh");

async fn snapshot(
    generation: u64,
    cwd: PathBuf,
    instance: daemon::Instance,
    environment: Arc<PromptEnvironment>,
    theme: &theme::AsyncTheme,
    executor: planning::PlanningExecutor,
) -> io::Result<()> {
    let git_enabled = theme.git_enabled();
    let active_runtimes = theme.runtimes();
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut tasks = JoinSet::new();
    // The coordinator owns the only receiver. At most one completion per
    // selected runtime plus group markers can be buffered for this generation.
    let (events, mut completions) = mpsc::channel(Runtime::ALL.len() + 3);

    let git_started = if git_enabled {
        let (started_tx, started_rx) = oneshot::channel();
        let git_instance = instance.clone();
        let git_cwd = cwd.clone();
        let git_environment = Arc::clone(&environment);
        let git_executor = executor.clone();
        let git_events = events.clone();
        tasks.spawn(async move {
            let _ = started_tx.send(());
            let result = match git_executor.git_query(git_cwd, git_environment).await {
                Ok(Some(query)) => daemon::git_status(&git_instance, &query).await,
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            let _ = git_events.send(SnapshotResult::Git(result)).await;
        });
        Some(started_rx)
    } else {
        None
    };

    if let Some(started) = git_started {
        let _ = timeout_at(deadline, started).await;
    }

    if !active_runtimes.is_empty() {
        let runtime_instance = instance.clone();
        let runtime_cwd = cwd.clone();
        let requested = active_runtimes.clone();
        let runtime_environment = Arc::clone(&environment);
        let runtime_events = events.clone();
        tasks.spawn(async move {
            stream_runtimes(
                &runtime_instance,
                planning::PlanningRequest {
                    cwd: runtime_cwd,
                    active: requested,
                    environment: runtime_environment,
                    executor,
                },
                runtime_events,
            )
            .await;
        });
    }
    drop(events);

    let mut receiving = true;
    while receiving || !tasks.is_empty() {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            event = completions.recv(), if receiving => {
                if let Some(event) = event {
                    write_result(event, generation, &active_runtimes, theme)?;
                } else {
                    receiving = false;
                }
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    protocol::write_error(
                        &mut io::stdout().lock(), generation, "snapshot",
                        &record_error(&io::Error::other(error)),
                    )?;
                }
            }
        }
    }

    tasks.abort_all();
    protocol::write_done(&mut io::stdout().lock(), generation)
}

/// Renders one typed completion immediately. Runtime group completion is a
/// separate event, sent only after every independent runtime owner has finished.
fn write_result(
    result: SnapshotResult,
    generation: u64,
    active_runtimes: &[Runtime],
    theme: &theme::AsyncTheme,
) -> io::Result<()> {
    let mut output = io::stdout().lock();
    match result {
        SnapshotResult::Git(Ok(snapshot)) => {
            protocol::write_segment(
                &mut output,
                generation,
                "git",
                &theme.render_git(snapshot.as_ref()),
            )?;
            // Each group finishes with a `complete` marker so the shell can
            // release that group's rendering lock as soon as it is done,
            // instead of holding the prompt blank until the final `done`.
            protocol::write_complete(&mut output, generation, "git")
        }
        SnapshotResult::Git(Err(error)) => {
            protocol::write_error(&mut output, generation, "git", &record_error(&error))?;
            protocol::write_complete(&mut output, generation, "git")
        }
        SnapshotResult::Runtime { runtime, value } => {
            let value = match value {
                Ok(value) => value,
                Err(error) => {
                    // A per-runtime failure must not clear completed siblings.
                    protocol::write_error(
                        &mut output,
                        generation,
                        runtime.name(),
                        &record_error(&error),
                    )?;
                    None
                }
            };
            let fragment = value
                .as_ref()
                .and_then(|value| theme.render_runtime(value))
                .unwrap_or_default();
            protocol::write_segment(&mut output, generation, runtime.name(), &fragment)
        }
        SnapshotResult::RuntimeComplete => {
            protocol::write_complete(&mut output, generation, "runtime")
        }
        SnapshotResult::RuntimePlanningFailed(error) => {
            protocol::write_error(&mut output, generation, "runtime", &record_error(&error))?;
            for runtime in active_runtimes {
                protocol::write_segment(&mut output, generation, runtime.name(), "")?;
            }
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
    let lock = theme::async_lock()?;
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
        .replace("@ZTHEME_LOCK_GIT@", &lock_flag(lock.git_segment))
        .replace("@ZTHEME_LOCK_RUNTIME@", &lock_flag(lock.runtime_segment))
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

/// Renders a boolean lock flag as a `0`/`1` zsh integer literal.
fn lock_flag(enabled: bool) -> String {
    if enabled { "1" } else { "0" }.to_owned()
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
    Git(io::Result<Option<gitstatus::Snapshot>>),
    Runtime {
        runtime: Runtime,
        value: io::Result<Option<RuntimeValue>>,
    },
    RuntimeComplete,
    RuntimePlanningFailed(io::Error),
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
            Ok(result) => result,
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
