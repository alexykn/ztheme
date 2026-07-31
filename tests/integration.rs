use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
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
            .env("TERM", "xterm-256color")
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
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("Usage: ztheme [COMMAND]"));
    assert!(help.contains("Commands:"));
    assert!(!help.contains("__daemon"));

    let no_command = sandbox.command().output().unwrap();
    assert_success(&no_command);
    assert!(String::from_utf8_lossy(&no_command.stdout).contains("Usage: ztheme [COMMAND]"));

    let help_command = sandbox.command().arg("help").output().unwrap();
    assert_success(&help_command);
    assert!(String::from_utf8_lossy(&help_command.stdout).contains("Commands:"));

    let invalid = sandbox.command().arg("unknown").output().unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unrecognized subcommand"));
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
fn deferred_plugins_load_once() {
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

const REQUEST_ENV_NAMES: [&str; 13] = [
    "PATH",
    "HOME",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_CEILING_DIRECTORIES",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "PERLBREW_PERL",
    "PLENV_VERSION",
    "RUSTUP_TOOLCHAIN",
    "RBENV_VERSION",
    "RUBY_VERSION",
];

fn client_request(generation: u64, cwd: &[u8]) -> Vec<u8> {
    client_request_with_env(generation, cwd, &[])
}

fn client_request_with_env(generation: u64, cwd: &[u8], env: &[(&str, &str)]) -> Vec<u8> {
    let mut fields: [&[u8]; 13] = [b""; 13];
    for (name, value) in env {
        let index = REQUEST_ENV_NAMES
            .iter()
            .position(|candidate| *candidate == *name)
            .unwrap();
        fields[index] = value.as_bytes();
    }
    let mut bytes = b"ZTREQ\0".to_vec();
    bytes.extend_from_slice(b"1\0");
    bytes.extend_from_slice(generation.to_string().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(cwd);
    bytes.push(0);
    for field in fields {
        bytes.extend_from_slice(field);
        bytes.push(0);
    }
    bytes
}

#[test]
fn client_daemon_round_trips_requests_and_exits_on_eof() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let mut child = sandbox
        .command()
        .args([
            "__client-daemon",
            "--theme",
            "0000",
            "--dev",
            "client-round-trip",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    stdin
        .write_all(&client_request(
            7,
            sandbox.home.to_str().unwrap().as_bytes(),
        ))
        .unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "ZTHEME1\t7\tdone\n");

    drop(stdin);
    let status = wait_for_exit(&mut child, PROCESS_TIMEOUT).unwrap();
    assert!(status.success());
}

#[test]
fn client_daemon_serves_git_requests_with_correct_generation() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("client-git-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));

    // Pre-spawn the server so it can be shut down deterministically at the end.
    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child());

    let generated = sandbox
        .command()
        .args(["init", "zsh", "--dev", &instance])
        .output()
        .unwrap();
    assert_success(&generated);
    let source = String::from_utf8(generated.stdout).unwrap();
    let hex = source
        .lines()
        .find_map(|line| line.strip_prefix("typeset -g __ZTHEME_ASYNC_THEME='"))
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap()
        .to_owned();

    let mut child = sandbox
        .command()
        .args(["__client-daemon", "--theme", &hex, "--dev", &instance])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    stdin
        .write_all(&client_request(
            23,
            sandbox.home.to_str().unwrap().as_bytes(),
        ))
        .unwrap();
    let mut records = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "client closed its output before done"
        );
        let fields: Vec<&str> = line.trim_end().split('\t').collect();
        assert_eq!(fields[0], "ZTHEME1");
        assert_eq!(fields[1], "23");
        records.push(line.clone());
        if fields[2] == "done" {
            break;
        }
    }
    assert!(records.len() >= 2, "expected records before done");

    drop(stdin);
    let status = wait_for_exit(&mut child, PROCESS_TIMEOUT).unwrap();
    assert!(status.success());

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn client_daemon_renders_async_segments_through_the_shell_integration() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("client-zsh-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    sandbox.write_theme(
        "asynctheme",
        "version = 1\n\
         [layout]\n\
         lines = [[\"directory\", \"git\"], [\"character\"]]\n\
         right = [\"status\"]\n\
         separator = \" | \"\n\
         blank_line_before = false\n",
    );

    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child());

    let script = format!(
        r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --dev {instance} --theme asynctheme)" || exit 80
add-zsh-hook -D preexec _ztheme_preexec 2>/dev/null
(( __ZTHEME_HAS_ASYNC )) || exit 81
[[ -n "$ZTHEME_CLIENT_PID" ]] || exit 82
# Write the request directly instead of _ztheme_start_worker: that function
# requires zle -F registration, which is only meaningful in an interactive
# shell, and the test drives the wire protocol itself.
typeset -i request_generation=5
local request_line="ZTREQ"$'\0'"1"$'\0'"$request_generation"$'\0'"$PWD"$'\0'
request_line+="${{PATH:-}}"$'\0'"${{HOME:-}}"$'\0'
request_line+="${{GIT_DIR:-}}"$'\0'"${{GIT_WORK_TREE:-}}"$'\0'
request_line+="${{GIT_CEILING_DIRECTORIES:-}}"$'\0'
request_line+="${{VIRTUAL_ENV:-}}"$'\0'"${{CONDA_PREFIX:-}}"$'\0'
request_line+="${{CONDA_DEFAULT_ENV:-}}"$'\0'
request_line+="${{PERLBREW_PERL:-}}"$'\0'"${{PLENV_VERSION:-}}"$'\0'
request_line+="${{RUSTUP_TOOLCHAIN:-}}"$'\0'"${{RBENV_VERSION:-}}"$'\0'
request_line+="${{RUBY_VERSION:-}}"$'\0'
if ! print -rn -- "$request_line" >&"$ZTHEME_REQ_FD"; then
    exit 83
fi
typeset -i found_done=0
typeset protocol generation kind segment fragment
while (( ! found_done )); do
    if ! IFS=$'\t' read -r -t 10 -u "$ZTHEME_RESP_FD" \
        protocol generation kind segment fragment
    then
        exit 84
    fi
    [[ "$protocol" == ZTHEME1 ]] || exit 85
    (( generation == request_generation )) || exit 86
    case "$kind" in
        segment) ;;
        error) ;;
        done) found_done=1 ;;
        *) exit 87 ;;
    esac
done
_ztheme_stop_client
"#,
        instance = instance
    );
    let output = sandbox.zsh(&script);
    assert_success(&output);

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn client_death_surfaces_eof_on_the_response_fifo() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("client-eof-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));

    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child());

    // The shell opens the response FIFO read-only, so when the client (the
    // only writer) dies, the shell's next read must see EOF immediately.
    // With the old O_RDWR shell-side open this would time out instead.
    let script = format!(
        r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --dev {instance})" || exit 80
add-zsh-hook -D preexec _ztheme_preexec 2>/dev/null
(( __ZTHEME_HAS_ASYNC )) || exit 81
[[ -n "$ZTHEME_CLIENT_PID" ]] || exit 82
# The client spawn must not permanently redirect the shell's stderr
# (regression check for the `exec ... 2>/dev/null` bug).
echo "stderr-alive" >&2 || exit 87
kill -9 "$ZTHEME_CLIENT_PID" 2>/dev/null || exit 83
zmodload zsh/datetime 2>/dev/null || exit 84
typeset -F start_time=EPOCHREALTIME
typeset line
if read -r -t 2 -u "$ZTHEME_RESP_FD" line 2>/dev/null; then
    exit 85  # got data, expected EOF
fi
typeset -F elapsed=$(( EPOCHREALTIME - start_time ))
(( elapsed < 1.5 )) || exit 86  # timed out instead of seeing EOF
_ztheme_stop_client
"#,
        instance = instance
    );
    let output = sandbox.zsh(&script);
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("stderr-alive"),
        "shell stderr was swallowed by the client spawn\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn client_daemon_applies_per_request_environment() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("client-env-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    sandbox.write_theme(
        "slowpython",
        "version = 1\n[layout]\nlines = [[\"python\"]]\nright = []\nseparator = \" | \"\nblank_line_before = false\n[segments.python]\nsymbol = \"py\"\nstyle = { foreground = \"accent\" }\nenvironment = { prefix = \"env:\" }\n",
    );

    // The version command must finish well inside the runtime command
    // timeout (250 ms). The first exec of a freshly compiled binary under a
    // stripped environment is slow (dyld warmup), so it is executed once
    // before the client starts.
    let bin = sandbox.home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_source = sandbox.home.join("fake_python.c");
    fs::write(
        &fake_source,
        "#include <stdio.h>\nint main(void) { printf(\"Python 3.12.0\\n\"); return 0; }\n",
    )
    .unwrap();
    let fake_python = bin.join("python");
    let compiled = Command::new("cc")
        .arg(&fake_source)
        .arg("-o")
        .arg(&fake_python)
        .status()
        .unwrap();
    assert!(compiled.success(), "failed to compile fake python");
    // Warm the dyld cache so the first request's exec is fast.
    assert!(Command::new(&fake_python).status().unwrap().success());
    // Marker so the python runtime is detected in the request cwd.
    fs::write(sandbox.home.join("pyproject.toml"), "[]\n").unwrap();

    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child());

    let generated = sandbox
        .command()
        .args(["init", "zsh", "--dev", &instance, "--theme", "slowpython"])
        .output()
        .unwrap();
    assert_success(&generated);
    let source = String::from_utf8(generated.stdout).unwrap();
    let hex = source
        .lines()
        .find_map(|line| line.strip_prefix("typeset -g __ZTHEME_ASYNC_THEME='"))
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap()
        .to_owned();

    let mut child = sandbox
        .command()
        .args(["__client-daemon", "--theme", &hex, "--dev", &instance])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let cwd = sandbox.home.to_str().unwrap().as_bytes().to_vec();
    let fake_path = bin.to_str().unwrap();

    fn read_until_done(
        stdout: &mut BufReader<std::process::ChildStdout>,
        generation: u64,
    ) -> Vec<String> {
        let mut fragments = Vec::new();
        loop {
            let mut line = String::new();
            assert_ne!(
                stdout.read_line(&mut line).unwrap(),
                0,
                "client closed its output before done"
            );
            let fields: Vec<&str> = line.trim_end().split('\t').collect();
            assert_eq!(fields[0], "ZTHEME1");
            assert_eq!(fields[1], generation.to_string());
            if fields[2] == "segment" {
                assert_eq!(fields[3], "python");
                fragments.push(fields[4].to_owned());
            }
            if fields[2] == "done" {
                return fragments;
            }
        }
    }

    // Request 1: VIRTUAL_ENV unset -> no environment label.
    stdin
        .write_all(&client_request_with_env(1, &cwd, &[("PATH", fake_path)]))
        .unwrap();
    let first = read_until_done(&mut stdout, 1);
    assert!(
        first.iter().all(|fragment| !fragment.contains("env:")),
        "unexpected label without VIRTUAL_ENV: {first:?}"
    );

    // Request 2: VIRTUAL_ENV set -> the label must reflect the new value.
    stdin
        .write_all(&client_request_with_env(
            2,
            &cwd,
            &[("PATH", fake_path), ("VIRTUAL_ENV", "/venv-b")],
        ))
        .unwrap();
    let second = read_until_done(&mut stdout, 2);
    assert!(
        second
            .iter()
            .any(|fragment| fragment.contains("env:venv-b")),
        "request environment was not applied: {second:?}"
    );

    // Request 3: back to the unset state; the previous value must not leak.
    stdin
        .write_all(&client_request_with_env(3, &cwd, &[("PATH", fake_path)]))
        .unwrap();
    let third = read_until_done(&mut stdout, 3);
    assert!(
        third
            .iter()
            .all(|fragment| !fragment.contains("env:venv-b")),
        "stale environment leaked into a later request: {third:?}"
    );

    drop(stdin);
    let status = wait_for_exit(&mut child, PROCESS_TIMEOUT).unwrap();
    assert!(status.success());

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn client_daemon_cancels_in_flight_work_without_emitting_stale_records() {
    let sandbox = Sandbox::new();
    let instance = format!("client-cancel-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    sandbox.write_theme(
        "gitonly",
        "version = 1\n[layout]\nlines = [[\"git\"]]\nright = []\nseparator = \" | \"\nblank_line_before = false\n",
    );

    // A gitstatusd that answers after a delay: the daemon spawns it with a
    // normal environment, so the delay is reliable (unlike a runtime command
    // in the client's stripped request environment).
    let delayed = sandbox.data.join("ztheme/gitstatus/v1.5/gitstatusd");
    fs::create_dir_all(delayed.parent().unwrap()).unwrap();
    fs::write(&delayed, "#!/bin/sh\n/bin/sleep 0.15\nexec cat\n").unwrap();
    fs::set_permissions(&delayed, fs::Permissions::from_mode(0o700)).unwrap();

    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child());

    let generated = sandbox
        .command()
        .args(["init", "zsh", "--dev", &instance, "--theme", "gitonly"])
        .output()
        .unwrap();
    assert_success(&generated);
    let source = String::from_utf8(generated.stdout).unwrap();
    let hex = source
        .lines()
        .find_map(|line| line.strip_prefix("typeset -g __ZTHEME_ASYNC_THEME='"))
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap()
        .to_owned();

    let mut child = sandbox
        .command()
        .args(["__client-daemon", "--theme", &hex, "--dev", &instance])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let cwd = sandbox.home.to_str().unwrap().as_bytes().to_vec();
    let fake_path = sandbox.data.to_str().unwrap();

    // Request A: its git query is still in flight (the fake gitstatusd
    // delays 150 ms) when B arrives and supersedes it, so A must contribute
    // no records at all.
    stdin
        .write_all(&client_request_with_env(1, &cwd, &[("PATH", fake_path)]))
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    stdin
        .write_all(&client_request_with_env(
            2,
            &cwd,
            &[("PATH", fake_path), ("VIRTUAL_ENV", "/venv-b")],
        ))
        .unwrap();

    let mut records = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "client closed its output before done"
        );
        let fields: Vec<&str> = line.trim_end().split('\t').collect();
        assert_eq!(fields[0], "ZTHEME1");
        assert_eq!(fields[1], "2", "superseded request leaked records: {line}");
        records.push(line.clone());
        if fields[2] == "done" {
            break;
        }
    }
    assert!(records.len() >= 2, "expected records before done");

    // Request C: the client still serves normally after the supersede.
    stdin
        .write_all(&client_request_with_env(3, &cwd, &[("PATH", fake_path)]))
        .unwrap();
    let mut c_records = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "client closed its output before done"
        );
        let fields: Vec<&str> = line.trim_end().split('\t').collect();
        assert_eq!(fields[0], "ZTHEME1");
        assert_eq!(fields[1], "3");
        c_records.push(line.clone());
        if fields[2] == "done" {
            break;
        }
    }
    assert!(c_records.len() >= 2, "expected records before done");

    drop(stdin);
    let status = wait_for_exit(&mut child, PROCESS_TIMEOUT).unwrap();
    assert!(status.success());

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}
