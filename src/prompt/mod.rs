mod client;
mod protocol;

pub(crate) use client::serve_client;

use std::env;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
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

pub async fn snapshot(
    generation: u64,
    cwd: PathBuf,
    instance: daemon::Instance,
    environment: Arc<PromptEnvironment>,
    theme: &theme::AsyncTheme,
) -> io::Result<()> {
    let git_enabled = theme.git_enabled();
    let active_runtimes = theme.runtimes();
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut tasks = JoinSet::new();

    let git_started = if git_enabled {
        let (started_tx, started_rx) = oneshot::channel();
        let git_instance = instance.clone();
        let git_cwd = cwd.clone();
        let git_environment = Arc::clone(&environment);
        tasks.spawn(async move {
            let _ = started_tx.send(());
            let result = match gitstatus::Query::from_values(
                &git_cwd,
                git_environment.git_dir.as_deref(),
                git_environment.git_work_tree.as_deref(),
            ) {
                Ok(query) => daemon::git_status(&git_instance, &query).await,
                Err(error) => Err(error),
            };
            SnapshotResult::Git(result)
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
        tasks.spawn(async move {
            SnapshotResult::Runtimes(
                runtime_values(
                    &runtime_instance,
                    runtime_cwd,
                    requested,
                    runtime_environment,
                )
                .await,
            )
        });
    }

    while !tasks.is_empty() {
        let Ok(Some(result)) = timeout_at(deadline, tasks.join_next()).await else {
            break;
        };
        write_result(result, generation, &active_runtimes, theme)?;
    }

    tasks.abort_all();
    protocol::write_done(&mut io::stdout().lock(), generation)
}

/// Renders one completed task result as protocol records: a Git snapshot or
/// error segment, then the runtime segments that have values. `done` is
/// written separately once every task has finished or the deadline passed.
fn write_result(
    result: Result<SnapshotResult, tokio::task::JoinError>,
    generation: u64,
    active_runtimes: &[Runtime],
    theme: &theme::AsyncTheme,
) -> io::Result<()> {
    let mut output = io::stdout().lock();
    match result {
        Ok(SnapshotResult::Git(Ok(snapshot))) => {
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
        Ok(SnapshotResult::Git(Err(error))) => {
            protocol::write_error(&mut output, generation, "git", &record_error(&error))?;
            protocol::write_complete(&mut output, generation, "git")
        }
        Ok(SnapshotResult::Runtimes(Ok(values))) => {
            for runtime in active_runtimes {
                let fragment = values
                    .iter()
                    .find(|value| value.runtime == *runtime)
                    .and_then(|value| theme.render_runtime(value))
                    .unwrap_or_default();
                protocol::write_segment(&mut output, generation, runtime.name(), &fragment)?;
            }
            protocol::write_complete(&mut output, generation, "runtime")
        }
        Ok(SnapshotResult::Runtimes(Err(error))) => {
            protocol::write_error(&mut output, generation, "runtime", &record_error(&error))?;
            protocol::write_complete(&mut output, generation, "runtime")
        }
        Err(error) => protocol::write_error(
            &mut output,
            generation,
            "snapshot",
            &record_error(&io::Error::other(error)),
        ),
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
        ))
}

/// Renders a boolean lock flag as a `0`/`1` zsh integer literal.
fn lock_flag(enabled: bool) -> String {
    if enabled { "1" } else { "0" }.to_owned()
}

/// Emits one `source` line and a declared-function check per resolved custom
/// segment. Paths are single-quoted with the shared shell quoting; ids are
/// validated identifiers, safe to interpolate into generated function names.
fn custom_segment_block(sources: &[theme::ResolvedCustomSegment]) -> String {
    let mut output = String::new();
    for source in sources {
        writeln!(
            output,
            "builtin source -- {} || return 1",
            shell_quote(&source.path.to_string_lossy())
        )
        .expect("writing to a String cannot fail");
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
    Runtimes(io::Result<Vec<RuntimeValue>>),
}

async fn runtime_values(
    instance: &daemon::Instance,
    cwd: PathBuf,
    active: Vec<Runtime>,
    environment: Arc<PromptEnvironment>,
) -> io::Result<Vec<RuntimeValue>> {
    let plans = build_plans(&cwd, &active, Arc::clone(&environment)).await?;
    let (cacheable, volatile) = partition_plans(plans);
    let cached = cached_runtime_values(
        instance,
        cwd.clone(),
        active.clone(),
        Arc::clone(&environment),
        cacheable,
        true,
    );
    let volatile_count = volatile.len();
    let volatile = runtime::execute_plans(volatile, cwd, Arc::clone(&environment));
    let (cached, volatile) = tokio::join!(cached, volatile);
    let mut values = cached?;
    values.extend(materialize_executions(
        volatile,
        volatile_count,
        &environment,
    ));
    Ok(merge_runtime_values(values))
}

async fn build_plans(
    cwd: &Path,
    active: &[Runtime],
    environment: Arc<PromptEnvironment>,
) -> io::Result<Vec<runtime::cache::RuntimePlan>> {
    let detection_cwd = cwd.to_path_buf();
    let detection_active = active.to_vec();
    let detection_environment = Arc::clone(&environment);
    let detection = tokio::task::spawn_blocking(move || {
        let git_root = runtime::detect::worktree_root(&detection_cwd, &detection_environment);
        runtime::detect::detect(
            &detection_cwd,
            git_root.as_deref(),
            &detection_active,
            &detection_environment,
        )
    });

    let base_active = active.to_vec();
    let base_cwd = cwd.to_path_buf();
    let base_environment = Arc::clone(&environment);
    let base_plans = tokio::task::spawn_blocking(move || {
        runtime::cache::resolve_base_plans(&base_active, &base_cwd, &base_environment)
    });

    let (project, base_plans) = tokio::try_join!(detection, base_plans)
        .map_err(|error| io::Error::other(format!("runtime planning task failed: {error}")))?;
    Ok(runtime::cache::finalize_plans(
        cwd,
        &project,
        base_plans,
        &environment,
    ))
}

fn partition_plans(
    plans: Vec<runtime::cache::RuntimePlan>,
) -> (
    Vec<runtime::cache::RuntimePlan>,
    Vec<runtime::cache::RuntimePlan>,
) {
    plans.into_iter().partition(|plan| match &plan.cache {
        runtime::cache::Cacheability::Cacheable(_) => true,
        runtime::cache::Cacheability::Volatile(reason) => {
            let _ = reason;
            false
        }
    })
}

async fn cached_runtime_values(
    instance: &daemon::Instance,
    cwd: PathBuf,
    active: Vec<Runtime>,
    environment: Arc<PromptEnvironment>,
    plans: Vec<runtime::cache::RuntimePlan>,
    retry_selection: bool,
) -> io::Result<Vec<RuntimeValue>> {
    if plans.is_empty() {
        return Ok(Vec::new());
    }
    let key = runtime::cache::cache_key(&plans, &environment);
    let acquire = daemon::runtime_cache_acquire(instance, key).await;
    let mut acquire = match acquire {
        Ok(value) => value,
        Err(error) => {
            eprintln!("ztheme: runtime cache unavailable: {error}");
            let expected = plans.len();
            let executions = runtime::execute_plans(plans, cwd, Arc::clone(&environment)).await;
            return Ok(materialize_executions(executions, expected, &environment));
        }
    };

    let mut corrupt_retried = false;
    loop {
        match acquire {
            Acquire::Hit(value) => match runtime::decode(&value) {
                Ok(values) if same_runtime_set(&plans, &values) => {
                    return Ok(values
                        .into_iter()
                        .map(|value| runtime::materialize(value, &environment))
                        .collect());
                }
                Ok(_) | Err(_) if !corrupt_retried => {
                    corrupt_retried = true;
                    let _ = daemon::runtime_cache_remove(instance, key).await;
                    acquire =
                        daemon::runtime_cache_acquire(instance, key)
                            .await
                            .map_err(|error| {
                                io::Error::other(format!(
                                    "runtime cache reacquire failed: {error:?}"
                                ))
                            })?;
                }
                Ok(_) | Err(_) => {
                    let expected = plans.len();
                    let executions =
                        runtime::execute_plans(plans, cwd, Arc::clone(&environment)).await;
                    return Ok(materialize_executions(executions, expected, &environment));
                }
            },
            Acquire::Owner(token) => {
                let before = key;
                let executions =
                    runtime::execute_plans(plans.clone(), cwd.clone(), Arc::clone(&environment))
                        .await;
                let (values, complete) = execution_values(executions, plans.len(), &environment);
                if !complete {
                    let _ = daemon::runtime_cache_release(instance, key, token).await;
                    return Ok(values);
                }

                let refreshed = build_plans(&cwd, &active, Arc::clone(&environment)).await?;
                let (refreshed_cacheable, _) = partition_plans(refreshed);
                let after = runtime::cache::cache_key(&refreshed_cacheable, &environment);
                if before != after {
                    let _ = daemon::runtime_cache_release(instance, key, token).await;
                    if retry_selection {
                        return Box::pin(refreshed_runtime_values(
                            instance,
                            cwd,
                            active,
                            environment,
                        ))
                        .await;
                    }
                    return Ok(values);
                }

                let cached_values = values
                    .iter()
                    .filter_map(|value| {
                        let version = value.version.as_ref()?;
                        Some(runtime::CachedRuntimeValue {
                            runtime: value.runtime,
                            version: version.clone(),
                            label: value.label.clone(),
                        })
                    })
                    .collect::<Vec<_>>();
                let encoded = runtime::encode(&cached_values)?;
                let _ = daemon::runtime_cache_put_owned(instance, key, token, &encoded).await;
                return Ok(values);
            }
        }
    }
}

async fn refreshed_runtime_values(
    instance: &daemon::Instance,
    cwd: PathBuf,
    active: Vec<Runtime>,
    environment: Arc<PromptEnvironment>,
) -> io::Result<Vec<RuntimeValue>> {
    let plans = build_plans(&cwd, &active, Arc::clone(&environment)).await?;
    let (cacheable, volatile) = partition_plans(plans);
    let cached = cached_runtime_values(
        instance,
        cwd.clone(),
        active,
        Arc::clone(&environment),
        cacheable,
        false,
    );
    let volatile_count = volatile.len();
    let volatile = runtime::execute_plans(volatile, cwd, Arc::clone(&environment));
    let (cached, volatile) = tokio::join!(cached, volatile);
    let mut values = cached?;
    values.extend(materialize_executions(
        volatile,
        volatile_count,
        &environment,
    ));
    Ok(merge_runtime_values(values))
}

fn same_runtime_set(
    plans: &[runtime::cache::RuntimePlan],
    values: &[runtime::CachedRuntimeValue],
) -> bool {
    let expected = plans.iter().map(|plan| plan.runtime).collect::<Vec<_>>();
    let actual = values.iter().map(|value| value.runtime).collect::<Vec<_>>();
    expected == actual
}

fn execution_values(
    executions: Vec<runtime::RuntimeExecution>,
    expected: usize,
    environment: &PromptEnvironment,
) -> (Vec<RuntimeValue>, bool) {
    let mut complete = executions.len() == expected;
    let mut values = Vec::new();
    for execution in executions {
        match execution.outcome {
            RuntimeOutcome::Value(value) => values.push(runtime::materialize(value, environment)),
            RuntimeOutcome::MissingExecutable => {
                // The runtime is detected but its executable is not installed.
                // Surface an empty-version value so the segment can still render
                // its symbol and language name (without a version).
                complete = false;
                values.push(RuntimeValue {
                    runtime: execution.runtime,
                    version: None,
                    label: None,
                    environment: None,
                });
            }
            RuntimeOutcome::TransientFailure => {
                complete = false;
            }
        }
    }
    (values, complete)
}

fn materialize_executions(
    executions: Vec<runtime::RuntimeExecution>,
    expected: usize,
    environment: &PromptEnvironment,
) -> Vec<RuntimeValue> {
    execution_values(executions, expected, environment).0
}

fn merge_runtime_values(mut values: Vec<RuntimeValue>) -> Vec<RuntimeValue> {
    values.sort_unstable_by_key(|value| value.runtime);
    values.dedup_by_key(|value| value.runtime);
    values
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
