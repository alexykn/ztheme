mod cache;
mod git;
mod gitstatus;
mod projects;
mod protocol;
mod runtimes;
mod theme;

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};

use projects::Runtime;

const HELPER_TIMEOUT: Duration = Duration::from_millis(550);
const ZSH_INTEGRATION: &str = include_str!("../shell/ztheme.zsh");

enum Request {
    Help,
    Version,
    InitZsh {
        instance: cache::Instance,
        theme: Option<String>,
    },
    Setup {
        assume_yes: bool,
    },
    ThemeList,
    ThemeEdit {
        selector: String,
    },
    ThemeApply {
        selector: String,
    },
    ThemeReload,
    ThemeZsh {
        instance: cache::Instance,
        selector: String,
        persist: bool,
    },
    Clear {
        instance: cache::Instance,
    },
    Snapshot {
        generation: u64,
        cwd: PathBuf,
        instance: cache::Instance,
        theme: Box<theme::AsyncTheme>,
    },
    Daemon {
        instance: cache::Instance,
    },
}

fn main() {
    let request = match arguments() {
        Ok(request) => request,
        Err(message) => {
            eprintln!("ztheme: {message}\nTry `ztheme --help` for usage.");
            std::process::exit(2);
        }
    };

    let result = match request {
        Request::Help => write_stdout(help()),
        Request::Version => write_stdout(concat!("ztheme ", env!("CARGO_PKG_VERSION"), "\n")),
        Request::InitZsh { instance, theme } => {
            init_zsh(&instance, theme.as_deref()).and_then(|script| write_stdout(&script))
        }
        Request::Setup { assume_yes } => setup(assume_yes),
        Request::ThemeList => theme::list().and_then(|output| write_stdout(&output)),
        Request::ThemeEdit { selector } => {
            theme::edit(&selector).and_then(|output| write_stdout(&output))
        }
        Request::ThemeApply { selector } => theme::apply(&selector).and_then(|path| {
            write_stdout(&format!(
                "Saved the theme selection to {}.\nThe current shell was not changed because the ztheme shell wrapper was bypassed.\n",
                path.display()
            ))
        }),
        Request::ThemeReload => Err(io::Error::other(
            "theme reload requires the active Zsh integration",
        )),
        Request::ThemeZsh {
            instance,
            selector,
            persist,
        } => theme_zsh(&instance, &selector, persist).and_then(|script| write_stdout(&script)),
        Request::Clear { instance } => run_async(cache::clear(&instance)),
        Request::Snapshot {
            generation,
            cwd,
            instance,
            theme,
        } => run_async(snapshot(generation, cwd, instance, theme)),
        Request::Daemon { instance } => run_async(cache::serve(&instance)),
    };

    if let Err(error) = result
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        eprintln!("ztheme: {error}");
        std::process::exit(1);
    }
}

async fn snapshot(
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

enum SnapshotResult {
    Git(io::Result<Option<gitstatus::Snapshot>>),
    Runtimes(io::Result<Vec<runtimes::RuntimeValue>>),
}

async fn runtime_values(
    instance: &cache::Instance,
    cwd: PathBuf,
    active: Vec<Runtime>,
) -> io::Result<Vec<runtimes::RuntimeValue>> {
    let git_root = git::worktree_root(&cwd);
    let project = projects::detect(&cwd, git_root.as_deref());
    let detected = active
        .into_iter()
        .filter(|runtime| project.runtimes.contains(runtime))
        .collect::<Vec<_>>();
    let key = runtimes::cache_key(&project, &detected);

    match cache::get(instance, key).await {
        Ok(Some(value)) => match runtimes::decode(&value) {
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

    let values = runtimes::snapshot(project, detected).await;
    let encoded = runtimes::encode(&values)?;
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

fn arguments() -> Result<Request, &'static str> {
    let mut arguments = env::args_os();
    let _program = arguments.next();
    let Some(command) = arguments.next() else {
        return Ok(Request::Help);
    };

    if command == "--help" || command == "-h" || command == "help" {
        return no_more(arguments, Request::Help);
    }
    if command == "--version" || command == "-V" {
        return no_more(arguments, Request::Version);
    }
    if command == "init" {
        if arguments.next().as_deref() != Some(std::ffi::OsStr::new("zsh")) {
            return Err("expected `ztheme init zsh`");
        }
        let (instance, theme) = init_arguments(arguments)?;
        return Ok(Request::InitZsh { instance, theme });
    }
    if command == "setup" {
        let assume_yes = match arguments.next() {
            None => false,
            Some(argument) if argument == "--yes" => true,
            Some(_) => return Err("expected `ztheme setup [--yes]`"),
        };
        return no_more(arguments, Request::Setup { assume_yes });
    }
    if command == "theme" {
        return theme_arguments(arguments);
    }
    if command == "clear" {
        let instance = instance_argument(arguments)?;
        return Ok(Request::Clear { instance });
    }
    if command == "__daemon" {
        let instance = instance_argument(arguments)?;
        return Ok(Request::Daemon { instance });
    }
    if command == "__theme-apply-zsh" || command == "__theme-reload-zsh" {
        let persist = command == "__theme-apply-zsh";
        let (selector, instance) = theme_zsh_arguments(arguments)?;
        return Ok(Request::ThemeZsh {
            instance,
            selector,
            persist,
        });
    }
    if command != "__snapshot" {
        return Err("unknown command");
    }

    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--generation")) {
        return Err("missing --generation");
    }
    let generation = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse().ok())
        .ok_or("invalid generation")?;

    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--cwd")) {
        return Err("missing --cwd");
    }
    let cwd = arguments.next().map(PathBuf::from).ok_or("missing cwd")?;
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("cwd must be an existing absolute directory");
    }
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--theme")) {
        return Err("missing --theme");
    }
    let theme = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("missing or non-UTF-8 compiled theme")
        .and_then(|value| {
            theme::AsyncTheme::decode_hex(&value).map_err(|_| "invalid compiled theme")
        })
        .map(Box::new)?;
    let instance = instance_argument(arguments)?;

    Ok(Request::Snapshot {
        generation,
        cwd,
        instance,
        theme,
    })
}

fn theme_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Request, &'static str> {
    let Some(command) = arguments.next() else {
        return Err("expected `ztheme theme list|edit|apply|reload`");
    };
    if command == "list" {
        return no_more(arguments, Request::ThemeList);
    }
    if command == "reload" {
        return no_more(arguments, Request::ThemeReload);
    }
    if command != "edit" && command != "apply" {
        return Err("expected `ztheme theme list|edit|apply|reload`");
    }
    let selector = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("missing or non-UTF-8 theme selector")?;
    if arguments.next().is_some() {
        return Err("unexpected argument");
    }
    if command == "edit" {
        return Ok(Request::ThemeEdit { selector });
    }
    Ok(Request::ThemeApply { selector })
}

fn theme_zsh_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(String, cache::Instance), &'static str> {
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--theme")) {
        return Err("missing --theme");
    }
    let selector = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("missing or non-UTF-8 theme selector")?;
    let instance = instance_argument(arguments)?;
    Ok((selector, instance))
}

fn init_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(cache::Instance, Option<String>), &'static str> {
    let mut development = None;
    let mut theme = None;
    while let Some(argument) = arguments.next() {
        if argument == "--dev" {
            if development.is_some() {
                return Err("--dev may only be specified once");
            }
            development = Some(
                arguments
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or("missing or non-UTF-8 development instance name")?,
            );
            continue;
        }
        if argument == "--theme" {
            if theme.is_some() {
                return Err("--theme may only be specified once");
            }
            theme = Some(
                arguments
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or("missing or non-UTF-8 theme selector")?,
            );
            continue;
        }
        return Err("expected `--dev NAME` or `--theme THEME`");
    }
    let instance = match development {
        Some(name) => cache::Instance::development(name)?,
        None => cache::Instance::Production,
    };
    Ok((instance, theme))
}

fn instance_argument(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<cache::Instance, &'static str> {
    let Some(argument) = arguments.next() else {
        return Ok(cache::Instance::Production);
    };
    if argument != "--dev" {
        return Err("expected `--dev NAME`");
    }
    let name = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("missing or non-UTF-8 development instance name")?;
    if arguments.next().is_some() {
        return Err("unexpected argument");
    }
    cache::Instance::development(name)
}

fn no_more(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
    request: Request,
) -> Result<Request, &'static str> {
    if arguments.next().is_some() {
        return Err("unexpected argument");
    }
    Ok(request)
}

fn init_zsh(instance: &cache::Instance, selector: Option<&str>) -> io::Result<String> {
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
        .replace("@ZTHEME_COMPILED_THEME@", &theme_zsh))
}

fn theme_zsh(instance: &cache::Instance, selector: &str, persist: bool) -> io::Result<String> {
    let script = init_zsh(instance, Some(selector))?;
    if persist {
        theme::persist(selector)?;
    }
    Ok(script)
}

fn setup(assume_yes: bool) -> io::Result<()> {
    if !gitstatus::ensure_installed(assume_yes)? {
        return Err(io::Error::other("gitstatusd installation declined"));
    }
    println!("{}", gitstatus::managed_binary().display());
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_stdout(value: &str) -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(value.as_bytes())?;
    output.flush()
}

fn run_async(future: impl Future<Output = io::Result<()>>) -> io::Result<()> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?
        .block_on(future)
}

const fn help() -> &'static str {
    "Fast asynchronous Zsh prompt

Usage:
  ztheme init zsh [--theme THEME] [--dev NAME]
  ztheme theme list
  ztheme theme edit THEME
  ztheme theme apply THEME
  ztheme theme reload
  ztheme setup [--yes]
  ztheme clear [--dev NAME]
  ztheme --help
  ztheme --version

Install the shell integration with:
  eval \"$(ztheme init zsh)\"
"
}
