use std::ffi::OsStr;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

struct ChildGuard(Option<Child>);

impl Sandbox {
    fn new() -> Self {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ztheme-integration-test-{}-{sequence}",
            std::process::id()
        ));
        let sandbox = Self {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
            root,
        };
        for directory in [
            &sandbox.home,
            &sandbox.config,
            &sandbox.data,
            &sandbox.cache,
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        sandbox
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ztheme"));
        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("NO_COLOR", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("VIRTUAL_ENV")
            .env_remove("CONDA_PREFIX");
        command
    }

    fn zsh(&self, script: &str) -> Output {
        let mut command = Command::new("zsh");
        command
            .args(["-dfc", script])
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("NO_COLOR", "1")
            .env("ZTHEME_TEST_BIN", env!("CARGO_BIN_EXE_ztheme"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("VIRTUAL_ENV")
            .env_remove("CONDA_PREFIX");
        command.output().unwrap()
    }

    fn theme_path(&self, name: &str) -> PathBuf {
        self.config
            .join("ztheme/themes")
            .join(format!("{name}.toml"))
    }

    fn write_theme(&self, name: &str, source: &str) -> PathBuf {
        let path = self.theme_path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    fn install_fake_gitstatus(&self) -> PathBuf {
        let path = self.data.join("ztheme/gitstatus/v1.5/gitstatusd");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "#!/bin/sh\nexec cat\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn install_fake_input_plugins(&self) {
        let autosuggestions = self
            .data
            .join("ztheme/zsh-autosuggestions/0.7.1/zsh-autosuggestions.zsh");
        fs::create_dir_all(autosuggestions.parent().unwrap()).unwrap();
        fs::write(
            autosuggestions,
            "typeset -gi ZTHEME_TEST_AUTOSUGGEST_LOADS=$(( ${ZTHEME_TEST_AUTOSUGGEST_LOADS:-0} + 1 ))\n\
             _ztheme_test_accept() { :; }\n\
             _zsh_autosuggest_start() {\n\
               typeset -gi ZTHEME_TEST_AUTOSUGGEST_STARTS=$(( ${ZTHEME_TEST_AUTOSUGGEST_STARTS:-0} + 1 ))\n\
               zle -N autosuggest-accept _ztheme_test_accept\n\
             }\n",
        )
        .unwrap();

        let highlighting = self
            .data
            .join("ztheme/zsh-syntax-highlighting/0.8.0/zsh-syntax-highlighting.zsh");
        fs::create_dir_all(highlighting.parent().unwrap()).unwrap();
        fs::write(
            highlighting,
            "typeset -gi ZTHEME_TEST_HIGHLIGHT_LOADS=$(( ${ZTHEME_TEST_HIGHLIGHT_LOADS:-0} + 1 ))\n\
             typeset -g ZSH_HIGHLIGHT_VERSION=0.8.0-test\n",
        )
        .unwrap();
    }

    fn install_fake_editor(&self) -> PathBuf {
        let path = self.root.join("editor");
        fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ZTHEME_TEST_EDITOR_LOG\"\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }

    fn wait(mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.take().unwrap().wait()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(child) = self.0.as_mut() else {
            return;
        };
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn minimal_theme(extra: &str) -> String {
    format!(
        "version = 1\n\
         \n\
         [layout]\n\
         lines = [[\"directory\"], [\"character\"]]\n\
         right = [\"status\"]\n\
         separator = \" | \"\n\
         blank_line_before = false\n\
         {extra}"
    )
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_socket(child: &mut Child) -> PathBuf {
    let pid = child.id();
    let directory = PathBuf::from(format!("/tmp/ztheme-{}", user_id()));
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon {pid} exited before creating its socket: {status}");
        }
        if let Ok(entries) = fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension() != Some(OsStr::new("lock")) {
                    continue;
                }
                let Ok(owner) = fs::read_to_string(&path) else {
                    continue;
                };
                if owner.trim() == pid.to_string() {
                    return path.with_extension("sock");
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "daemon {pid} did not create its socket"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn shutdown_outdated_daemon(socket: &Path) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.write_all(b"ZT").unwrap();
    stream.write_all(&2_u16.to_be_bytes()).unwrap();
    let mut response = [0];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(response[0], 0xfe);
}

fn user_id() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }

    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { getuid() }
}

#[test]
fn cli_help_version_and_invalid_arguments_have_stable_exit_classes() {
    let sandbox = Sandbox::new();

    let version = sandbox.command().arg("--version").output().unwrap();
    assert_success(&version);
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("ztheme {}\n", env!("CARGO_PKG_VERSION"))
    );

    let help = sandbox.command().arg("--help").output().unwrap();
    assert_success(&help);
    assert!(String::from_utf8_lossy(&help.stdout).contains("ztheme init zsh"));

    let invalid = sandbox.command().arg("unknown").output().unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unknown command"));
}

#[test]
fn theme_management_lists_and_atomically_persists_valid_themes() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    sandbox.write_theme("amber", &minimal_theme(""));

    let applied = sandbox
        .command()
        .args(["theme", "apply", "amber"])
        .output()
        .unwrap();
    assert_success(&applied);
    let config = sandbox.config.join("ztheme/config.toml");
    let saved = fs::read_to_string(&config).unwrap();
    assert!(saved.contains("theme = \"amber\""));

    let listed = sandbox.command().args(["theme", "list"]).output().unwrap();
    assert_success(&listed);
    let listing = String::from_utf8(listed.stdout).unwrap();
    let catppuccin = listing
        .find("\n○ catppuccin-mocha (default) - builtin")
        .unwrap();
    let vesper = listing.find("\n○ vesper - builtin").unwrap();
    let amber = listing.find("\n● amber").unwrap();
    assert!(catppuccin < vesper);
    assert!(vesper < amber);
    for section in ["palette", "layout", "symbols", "example"] {
        assert!(listing.contains(section));
    }
    assert!(!listing.contains('\u{1b}'));

    sandbox.write_theme("broken", "version = 1\n[layout]\nright = [\"git\"]\n");
    let invalid = sandbox
        .command()
        .args(["theme", "apply", "broken"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(fs::read_to_string(config).unwrap(), saved);
}

#[test]
fn theme_edit_uses_visual_and_passes_the_resolved_theme_path() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let theme = sandbox.write_theme("amber", &minimal_theme(""));
    let editor = sandbox.install_fake_editor();
    let log = sandbox.root.join("editor.log");

    let edited = sandbox
        .command()
        .args(["theme", "edit", "amber"])
        .env("VISUAL", format!("{} --visual", editor.display()))
        .env("EDITOR", "false")
        .env("ZTHEME_TEST_EDITOR_LOG", &log)
        .output()
        .unwrap();
    assert_success(&edited);
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        format!("--visual\n{}\n", theme.display())
    );
}

#[test]
fn generated_zsh_is_complete_and_syntactically_valid() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    sandbox.write_theme("minimal", &minimal_theme(""));

    let generated = sandbox
        .command()
        .args(["init", "zsh", "--theme", "minimal"])
        .output()
        .unwrap();
    assert_success(&generated);
    let source = String::from_utf8(generated.stdout).unwrap();
    assert!(!source.contains("@ZTHEME_"));
    assert!(source.contains("_ztheme_render_layout"));
    assert!(source.contains("ZSH_HIGHLIGHT_STYLES[command]"));

    let path = sandbox.root.join("generated.zsh");
    fs::write(&path, source).unwrap();
    let checked = Command::new("zsh").arg("-n").arg(path).output().unwrap();
    assert_success(&checked);
}

#[test]
fn zsh_renders_immediate_segments_and_shell_defaults() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    sandbox.write_theme("minimal", &minimal_theme(""));

    let script = r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --theme minimal)" || exit 10
cd "$HOME"
COLUMNS=20
_ztheme_format_directory
_ztheme_format_status 7
_ztheme_render_layout
print -r -- "directory=$ZTHEME_SEGMENT_DIRECTORY"
print -r -- "character=$ZTHEME_SEGMENT_CHARACTER"
print -r -- "status=$ZTHEME_SEGMENT_STATUS"
print -r -- "prompt=$ZTHEME_PROMPT"
print -r -- "right=$ZTHEME_RPROMPT"
[[ -o autocd ]] || exit 11
[[ -o sharehistory ]] || exit 12
[[ "$HISTSIZE" == 100000 && "$SAVEHIST" == 100000 ]] || exit 13
[[ "$(bindkey '^[[A')" == *history-beginning-search-backward-end* ]] || exit 14
"#;
    let output = sandbox.zsh(script);
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("directory="));
    assert!(stdout.contains("%24<"));
    assert!(stdout.contains("character="));
    assert!(stdout.contains("status="));
    assert!(stdout.contains('7'));
    assert!(stdout.contains("right="));

    let disabled = sandbox.zsh(
        r#"
ZTHEME_SHELL_DEFAULTS=0
eval "$("$ZTHEME_TEST_BIN" init zsh --theme minimal)" || exit 20
[[ ! -o autocd ]] || exit 21
"#,
    );
    assert_success(&disabled);

    let preserved = sandbox.zsh(
        r#"
HISTFILE="$HOME/custom-history"
HISTSIZE=321
SAVEHIST=123
eval "$("$ZTHEME_TEST_BIN" init zsh --theme minimal)" || exit 22
[[ "$HISTFILE" == "$HOME/custom-history" ]] || exit 23
[[ "$HISTSIZE" == 321 && "$SAVEHIST" == 123 ]] || exit 24
"#,
    );
    assert_success(&preserved);
}

#[test]
fn theme_apply_and_reload_update_the_current_shell() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    sandbox.write_theme("minimal", &minimal_theme(""));
    let amber = sandbox.write_theme(
        "amber",
        &format!(
            "{}\n[input.syntax]\ncommand = {{ foreground = \"#112233\" }}\n",
            minimal_theme("")
        ),
    );
    let updated = sandbox.root.join("updated.toml");
    fs::write(
        &updated,
        format!(
            "{}\n[input.syntax]\ncommand = {{ foreground = \"#445566\" }}\n",
            minimal_theme("")
        ),
    )
    .unwrap();

    let script = format!(
        r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --theme minimal)" || exit 30
ztheme theme apply amber >/dev/null || exit 31
[[ "$__ZTHEME_THEME_SELECTOR" == amber ]] || exit 32
[[ "${{ZSH_HIGHLIGHT_STYLES[command]}}" == "fg=#112233" ]] || exit 33
command cp {} {} || exit 34
ztheme theme reload >/dev/null || exit 35
[[ "${{ZSH_HIGHLIGHT_STYLES[command]}}" == "fg=#445566" ]] || exit 36
"#,
        shell_word(&updated),
        shell_word(&amber)
    );
    let output = sandbox.zsh(&script);
    assert_success(&output);
    assert!(
        fs::read_to_string(sandbox.config.join("ztheme/config.toml"))
            .unwrap()
            .contains("theme = \"amber\"")
    );
}

#[test]
fn deferred_plugins_load_once_and_worker_cleanup_preserves_standard_streams() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    sandbox.install_fake_input_plugins();
    sandbox.write_theme("minimal", &minimal_theme(""));

    let script = r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --theme minimal)" || exit 40
_ztheme_load_shell_plugins
_ztheme_initialize_autosuggestions
_ztheme_load_shell_plugins
[[ "$ZTHEME_TEST_AUTOSUGGEST_LOADS" == 1 ]] || exit 41
[[ "$ZTHEME_TEST_AUTOSUGGEST_STARTS" == 1 ]] || exit 42
[[ "$ZTHEME_TEST_HIGHLIGHT_LOADS" == 1 ]] || exit 43
(( $+widgets[autosuggest-accept] )) || exit 44
exec {ZTHEME_ASYNC_FD}< <(sleep 1)
_ztheme_preexec
print -r -- stdout-sentinel
print -u2 -r -- stderr-sentinel
"#;
    let output = sandbox.zsh(script);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("stdout-sentinel"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("stderr-sentinel"));
}

#[test]
fn daemon_enforces_single_ownership_and_restarts_after_version_shutdown() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("integration-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));

    let first = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut first = ChildGuard::new(first);
    let socket = wait_for_socket(first.child());

    let second = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .output()
        .unwrap();
    assert_success(&second);
    assert!(first.child().try_wait().unwrap().is_none());

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(first.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    first.wait().unwrap();

    let replacement = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut replacement = ChildGuard::new(replacement);
    let replacement_socket = wait_for_socket(replacement.child());
    shutdown_outdated_daemon(&replacement_socket);
    assert!(
        wait_for_exit(replacement.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    replacement.wait().unwrap();
}

fn shell_word(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
