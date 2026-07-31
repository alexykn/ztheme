use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, CommandFactory as _, Parser, Subcommand, error::ErrorKind};
use tokio::runtime::Builder;

use crate::{daemon, prompt, setup, theme};

#[derive(Parser)]
#[command(name = "ztheme", version, about = "Fast asynchronous Zsh prompt")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Generate shell initialization code.
    Init(InitArgs),
    /// Install or repair prompt helpers.
    Setup(SetupArgs),
    /// List, edit, apply, or reload themes.
    Theme(ThemeArgs),
    /// Clear cached runtime values and reset Git status.
    Clear(InstanceArgs),

    #[command(name = "__daemon", hide = true)]
    Daemon(InstanceArgs),

    #[command(name = "__snapshot", hide = true)]
    Snapshot(SnapshotArgs),

    #[command(name = "__client-daemon", hide = true)]
    ClientDaemon(ClientDaemonArgs),

    #[command(name = "__theme-apply-zsh", hide = true)]
    ThemeApplyZsh(InternalThemeArgs),

    #[command(name = "__theme-reload-zsh", hide = true)]
    ThemeReloadZsh(InternalThemeArgs),
}

#[derive(Args)]
struct InitArgs {
    #[command(subcommand)]
    shell: InitCommand,
}

#[derive(Subcommand)]
enum InitCommand {
    /// Generate Zsh initialization code.
    Zsh(InitZshArgs),
}

#[derive(Args)]
struct InitZshArgs {
    #[arg(long, value_name = "THEME")]
    theme: Option<String>,

    #[command(flatten)]
    instance: InstanceArgs,
}

#[derive(Args)]
struct SetupArgs {
    #[arg(long)]
    yes: bool,
}

#[derive(Args)]
struct ThemeArgs {
    #[command(subcommand)]
    command: ThemeCommand,
}

#[derive(Subcommand)]
enum ThemeCommand {
    /// Preview available themes.
    List,
    /// Open a theme in the configured editor.
    Edit(ThemeSelector),
    /// Select a theme for future shells.
    Apply(ThemeSelector),
    /// Reload the selected theme through the active shell wrapper.
    Reload,
}

#[derive(Args)]
struct ThemeSelector {
    #[arg(value_name = "THEME")]
    selector: String,
}

#[derive(Args, Default)]
struct InstanceArgs {
    #[arg(long, value_name = "NAME")]
    dev: Option<String>,
}

#[derive(Args)]
struct SnapshotArgs {
    #[arg(long)]
    generation: u64,

    #[arg(long, value_name = "PATH")]
    cwd: PathBuf,

    #[arg(long, value_name = "HEX")]
    theme: String,

    #[command(flatten)]
    instance: InstanceArgs,
}

#[derive(Args)]
struct ClientDaemonArgs {
    #[arg(long, value_name = "HEX")]
    theme: String,

    #[command(flatten)]
    instance: InstanceArgs,
}

#[derive(Args)]
struct InternalThemeArgs {
    #[arg(long, value_name = "THEME")]
    theme: String,

    #[command(flatten)]
    instance: InstanceArgs,
}

enum Request {
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
    ClientDaemon {
        instance: daemon::Instance,
        theme: Box<theme::AsyncTheme>,
    },
    Daemon {
        instance: daemon::Instance,
    },
}

pub(crate) fn run() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            print_parser_result(&error);
            return;
        }
    };
    let Some(command) = cli.command else {
        let mut help = Cli::command().render_long_help().to_string();
        help.push('\n');
        finish(write_stdout(&help));
        return;
    };
    let request = match request(command) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("ztheme: {message}\nTry `ztheme --help` for usage.");
            std::process::exit(2);
        }
    };

    let result = match request {
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
                "Saved the theme selection to {}.\n\
                 The current shell was not changed because the ztheme shell wrapper was bypassed.\n",
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
        } => run_async(prompt::snapshot(generation, cwd, instance, &theme)),
        Request::ClientDaemon { instance, theme } => {
            run_async(prompt::serve_client(instance, Arc::new(*theme)))
        }
        Request::Daemon { instance } => run_async(daemon::serve(&instance)),
    };
    finish(result);
}

fn request(command: Command) -> Result<Request, &'static str> {
    match command {
        Command::Init(InitArgs {
            shell: InitCommand::Zsh(arguments),
        }) => Ok(Request::InitZsh {
            instance: instance(arguments.instance)?,
            theme: arguments.theme,
        }),
        Command::Setup(arguments) => Ok(Request::Setup {
            assume_yes: arguments.yes,
        }),
        Command::Theme(ThemeArgs { command }) => match command {
            ThemeCommand::List => Ok(Request::ThemeList),
            ThemeCommand::Edit(arguments) => Ok(Request::ThemeEdit {
                selector: arguments.selector,
            }),
            ThemeCommand::Apply(arguments) => Ok(Request::ThemeApply {
                selector: arguments.selector,
            }),
            ThemeCommand::Reload => Ok(Request::ThemeReload),
        },
        Command::Clear(arguments) => Ok(Request::Clear {
            instance: instance(arguments)?,
        }),
        Command::Daemon(arguments) => Ok(Request::Daemon {
            instance: instance(arguments)?,
        }),
        Command::Snapshot(arguments) => {
            if !arguments.cwd.is_absolute() || !arguments.cwd.is_dir() {
                return Err("cwd must be an existing absolute directory");
            }
            let theme = theme::AsyncTheme::decode_hex(&arguments.theme)
                .map_err(|_| "invalid compiled theme")?;
            Ok(Request::Snapshot {
                generation: arguments.generation,
                cwd: arguments.cwd,
                instance: instance(arguments.instance)?,
                theme: Box::new(theme),
            })
        }
        Command::ClientDaemon(arguments) => {
            let theme = theme::AsyncTheme::decode_hex(&arguments.theme)
                .map_err(|_| "invalid compiled theme")?;
            Ok(Request::ClientDaemon {
                instance: instance(arguments.instance)?,
                theme: Box::new(theme),
            })
        }
        Command::ThemeApplyZsh(arguments) => Ok(Request::ThemeZsh {
            instance: instance(arguments.instance)?,
            selector: arguments.theme,
            persist: true,
        }),
        Command::ThemeReloadZsh(arguments) => Ok(Request::ThemeZsh {
            instance: instance(arguments.instance)?,
            selector: arguments.theme,
            persist: false,
        }),
    }
}

fn instance(arguments: InstanceArgs) -> Result<daemon::Instance, &'static str> {
    arguments
        .dev
        .map_or(Ok(daemon::Instance::Production), |name| {
            daemon::Instance::development(name)
        })
}

fn print_parser_result(error: &clap::Error) {
    let code = error.exit_code();
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        if let Err(write_error) = write_stdout(&error.to_string())
            && write_error.kind() != io::ErrorKind::BrokenPipe
        {
            eprintln!("ztheme: {write_error}");
            std::process::exit(1);
        }
        return;
    }
    eprint!("{error}");
    std::process::exit(code);
}

fn finish(result: io::Result<()>) {
    if let Err(error) = result
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        eprintln!("ztheme: {error}");
        std::process::exit(1);
    }
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

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, Parser as _};

    use super::{Cli, Command, InitCommand, ThemeCommand};

    #[test]
    fn public_and_internal_commands_parse() {
        let cases = [
            vec![
                "ztheme", "init", "zsh", "--theme", "vesper", "--dev", "test",
            ],
            vec!["ztheme", "setup", "--yes"],
            vec!["ztheme", "theme", "list"],
            vec!["ztheme", "theme", "edit", "vesper"],
            vec!["ztheme", "theme", "apply", "vesper"],
            vec!["ztheme", "theme", "reload"],
            vec!["ztheme", "clear", "--dev", "test"],
            vec!["ztheme", "__daemon", "--dev", "test"],
            vec![
                "ztheme",
                "__snapshot",
                "--generation",
                "4",
                "--cwd",
                "/",
                "--theme",
                "0000",
                "--dev",
                "test",
            ],
            vec![
                "ztheme",
                "__client-daemon",
                "--theme",
                "0000",
                "--dev",
                "test",
            ],
            vec![
                "ztheme",
                "__theme-apply-zsh",
                "--theme",
                "vesper",
                "--dev",
                "test",
            ],
            vec![
                "ztheme",
                "__theme-reload-zsh",
                "--theme",
                "vesper",
                "--dev",
                "test",
            ],
        ];

        for arguments in cases {
            assert!(Cli::try_parse_from(&arguments).is_ok(), "{arguments:?}");
        }
    }

    #[test]
    fn command_shapes_are_nested_as_expected() {
        let cli = Cli::try_parse_from(["ztheme", "init", "zsh"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Init(super::InitArgs {
                shell: InitCommand::Zsh(_)
            }))
        ));
        let cli = Cli::try_parse_from(["ztheme", "theme", "reload"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Theme(super::ThemeArgs {
                command: ThemeCommand::Reload
            }))
        ));
    }

    #[test]
    fn invalid_commands_and_duplicate_flags_are_rejected() {
        for arguments in [
            &["ztheme", "unknown"][..],
            &["ztheme", "theme", "unknown"],
            &["ztheme", "clear", "--dev"],
            &["ztheme", "clear", "--dev", "one", "--dev", "two"],
            &["ztheme", "__snapshot", "--generation", "x"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn internal_commands_are_hidden_from_public_help() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("init"));
        assert!(help.contains("theme"));
        assert!(!help.contains("__daemon"));
        assert!(!help.contains("__snapshot"));
        assert!(!help.contains("__client-daemon"));
        assert!(!help.contains("__theme-apply-zsh"));
    }
}
