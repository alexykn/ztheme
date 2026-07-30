use std::env;
use std::io::{self, Write};
use std::path::PathBuf;

use tokio::runtime::Builder;

use crate::{daemon, prompt, setup, theme};

enum Request {
    Help,
    Version,
    InitZsh {
        instance: daemon::Instance,
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
        instance: daemon::Instance,
        selector: String,
        persist: bool,
    },
    Clear {
        instance: daemon::Instance,
    },
    Snapshot {
        generation: u64,
        cwd: PathBuf,
        instance: daemon::Instance,
        theme: Box<theme::AsyncTheme>,
    },
    Daemon {
        instance: daemon::Instance,
    },
}

pub fn run() {
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
            prompt::init_zsh(&instance, theme.as_deref()).and_then(|script| write_stdout(&script))
        }
        Request::Setup { assume_yes } => setup::run(assume_yes),
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
        } => prompt::theme_zsh(&instance, &selector, persist)
            .and_then(|script| write_stdout(&script)),
        Request::Clear { instance } => run_async(daemon::reset(&instance)),
        Request::Snapshot {
            generation,
            cwd,
            instance,
            theme,
        } => run_async(prompt::snapshot(generation, cwd, instance, theme)),
        Request::Daemon { instance } => run_async(daemon::serve(&instance)),
    };

    if let Err(error) = result
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        eprintln!("ztheme: {error}");
        std::process::exit(1);
    }
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
) -> Result<(String, daemon::Instance), &'static str> {
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
) -> Result<(daemon::Instance, Option<String>), &'static str> {
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
        Some(name) => daemon::Instance::development(name)?,
        None => daemon::Instance::Production,
    };
    Ok((instance, theme))
}

fn instance_argument(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<daemon::Instance, &'static str> {
    let Some(argument) = arguments.next() else {
        return Ok(daemon::Instance::Production);
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
    daemon::Instance::development(name)
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
