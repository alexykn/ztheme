mod protocol;

use std::env;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};

use crate::context::{self, Runtime, RuntimeValue};
use crate::{cache, gitstatus, setup, theme};

pub(crate) use protocol::prompt_text;

const HELPER_TIMEOUT: Duration = Duration::from_millis(550);
const ZSH_DEFAULTS: &str = include_str!("../../shell/defaults.zsh");
const ZSH_INTEGRATION: &str = include_str!("../../shell/ztheme.zsh");

pub async fn snapshot(
    generation: u64,
    cwd: PathBuf,
    instance: cache::Instance,
    theme: Box<theme::AsyncTheme>,
) -> io::Result<()> {
    let git_enabled = theme.git_enabled();
    let active_runtimes = theme.runtimes();
    let mut tasks = JoinSet::new();

    if git_enabled {
        let git_instance = instance.clone();
        let git_cwd = cwd.clone();
        tasks.spawn(async move {
            let result = match gitstatus::Query::from_environment(&git_cwd) {
                Ok(query) => cache::git(&git_instance, &query).await,
                Err(error) => Err(error),
            };
            SnapshotResult::Git(result)
        });
    }
    if !active_runtimes.is_empty() {
        let runtime_instance = instance.clone();
        let runtime_cwd = cwd.clone();
        let requested = active_runtimes.clone();
        tasks.spawn(async move {
            SnapshotResult::Runtimes(
                runtime_values(&runtime_instance, runtime_cwd, requested).await,
            )
        });
    }

    let deadline = Instant::now() + HELPER_TIMEOUT;
    let mut output = io::stdout().lock();
    while !tasks.is_empty() {
        let Ok(Some(result)) = timeout_at(deadline, tasks.join_next()).await else {
            break;
        };
        match result {
            Ok(SnapshotResult::Git(Ok(snapshot))) => {
                protocol::write_segment(
                    &mut output,
                    generation,
                    "git",
                    &theme.render_git(snapshot.as_ref()),
                )?;
            }
            Ok(SnapshotResult::Git(Err(error))) => {
                protocol::write_error(&mut output, generation, "git", &record_error(&error))?;
            }
            Ok(SnapshotResult::Runtimes(Ok(values))) => {
                for runtime in &active_runtimes {
                    let fragment = values
                        .iter()
                        .find(|value| value.runtime == *runtime)
                        .and_then(|value| theme.render_runtime(value))
                        .unwrap_or_default();
                    protocol::write_segment(&mut output, generation, runtime.name(), &fragment)?;
                }
            }
            Ok(SnapshotResult::Runtimes(Err(error))) => {
                protocol::write_error(&mut output, generation, "runtime", &record_error(&error))?;
            }
            Err(error) => {
                protocol::write_error(
                    &mut output,
                    generation,
                    "snapshot",
                    &record_error(&io::Error::other(error)),
                )?;
            }
        }
    }

    tasks.abort_all();
    protocol::write_done(&mut output, generation)
}

pub fn init_zsh(instance: &cache::Instance, selector: Option<&str>) -> io::Result<String> {
    let theme = theme::CompiledTheme::load(selector)?;
    let theme_zsh = theme.zsh()?;
    if !gitstatus::ensure_installed(false)? {
        return Err(io::Error::other(
            "gitstatusd is required; initialization skipped (`ztheme setup --yes`)",
        ));
    }
    let binary = env::current_exe()?;
    let binary = shell_quote(&binary.to_string_lossy());
    let instance_arguments = instance
        .development_name()
        .map_or_else(String::new, |name| format!("--dev {}", shell_quote(name)));
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
        .replace("@ZTHEME_SHELL_DEFAULTS@", ZSH_DEFAULTS)
        .replace("@ZTHEME_COMPILED_THEME@", &theme_zsh))
}

pub fn theme_zsh(instance: &cache::Instance, selector: &str, persist: bool) -> io::Result<String> {
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
    instance: &cache::Instance,
    cwd: PathBuf,
    active: Vec<Runtime>,
) -> io::Result<Vec<RuntimeValue>> {
    let git_root = context::worktree_root(&cwd);
    let project = context::detect(&cwd, git_root.as_deref());
    let detected = active
        .into_iter()
        .filter(|runtime| project.runtimes.contains(runtime))
        .collect::<Vec<_>>();
    let key = context::cache_key(&project, &detected);

    match cache::get(instance, key).await {
        Ok(Some(value)) => match context::decode(&value) {
            Ok(values) => return Ok(values),
            Err(error) => eprintln!("ztheme: invalid runtime cache entry: {error}"),
        },
        Ok(None) => {}
        Err(error) => {
            if let Err(start_error) = cache::spawn_daemon(instance) {
                eprintln!("ztheme: runtime cache daemon failed to start: {start_error}");
            } else {
                eprintln!("ztheme: runtime cache unavailable: {error}");
            }
        }
    }

    let values = context::snapshot(project, detected).await;
    let encoded = context::encode(&values)?;
    if let Err(error) = cache::put(instance, key, &encoded).await {
        eprintln!("ztheme: runtime cache write failed: {error}");
    }
    Ok(values)
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
