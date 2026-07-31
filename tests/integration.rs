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
    runtime: PathBuf,
}

struct ChildGuard(Option<Child>);

impl Drop for Sandbox {
    fn drop(&mut self) {
        // ChildGuards have already terminated the daemons by the time the
        // sandbox drops, so the private runtime directory (sockets, lock
        // files) can be removed without racing a live process.
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Sandbox {
    fn new() -> Self {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        // A short /tmp path keeps the daemon socket well below the Unix
        // socket pathname limit (SUN_LEN).
        let root =
            PathBuf::from("/tmp").join(format!("ztheme-test-{}-{sequence}", std::process::id()));
        let sandbox = Self {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
            runtime: root.join("runtime"),
            root,
        };
        for directory in [
            &sandbox.home,
            &sandbox.config,
            &sandbox.data,
            &sandbox.cache,
            &sandbox.runtime,
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::set_permissions(&sandbox.runtime, fs::Permissions::from_mode(0o700)).unwrap();
        sandbox
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ztheme"));
        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("ZTHEME_RUNTIME_DIR", &self.runtime)
            .env("NO_COLOR", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("VIRTUAL_ENV")
            .env_remove("CONDA_PREFIX");
        command
    }

    fn zsh(&self, script: &str) -> Output {
        self.zsh_command().args(["-dfc", script]).output().unwrap()
    }

    fn zsh_command(&self) -> Command {
        let mut command = Command::new("zsh");
        command
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("ZTHEME_RUNTIME_DIR", &self.runtime)
            .env("NO_COLOR", "1")
            .env("TERM", "xterm-256color")
            .env("ZTHEME_TEST_BIN", env!("CARGO_BIN_EXE_ztheme"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("VIRTUAL_ENV")
            .env_remove("CONDA_PREFIX");
        command
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

/// Compiles a tiny C helper into the sandbox's bin directory.
fn compile_c(sandbox: &Sandbox, name: &str, source: &str) -> PathBuf {
    let source_path = sandbox.home.join(format!("{name}.c"));
    fs::write(&source_path, source).unwrap();
    let out = sandbox.home.join("bin").join(name);
    fs::create_dir_all(out.parent().unwrap()).unwrap();
    let compiled = Command::new("cc")
        .arg(&source_path)
        .arg("-o")
        .arg(&out)
        .status()
        .unwrap();
    assert!(compiled.success(), "failed to compile {name}");
    out
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success()
}

/// Polls until the process with `pid` no longer exists (kill -0 fails).
fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Spawns a non-interactive zsh that sources the generated integration, runs
/// the given preamble, records the shell and client PIDs in a marker file,
/// and then stays alive reading commands from its stdin. Returns the shell
/// child and the client PID.
fn spawn_live_shell(sandbox: &Sandbox, instance: &str, preamble: &str) -> (Child, u32) {
    let marker = sandbox.home.join(format!("zt-marker-{instance}"));
    let _ = fs::remove_file(&marker);
    let mut command = sandbox.zsh_command();
    command
        .env("ZTHEME_TEST_MARKER", &marker)
        .args(["-df"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let script = format!(
        r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --dev {instance})" || exit 80
(( __ZTHEME_HAS_ASYNC )) || exit 81
[[ -n "$ZTHEME_CLIENT_PID" ]] || exit 82
{preamble}
print -r -- "CLIENT_PID=$ZTHEME_CLIENT_PID" >> "$ZTHEME_TEST_MARKER"
print -r -- "SHELL_PID=$$" >> "$ZTHEME_TEST_MARKER"
print -r -- "READY" >> "$ZTHEME_TEST_MARKER"
while true; do read -r __zt_line || break; done
"#,
    );
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();

    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let mut client_pid = 0;
    let mut ready = false;
    loop {
        if let Ok(contents) = fs::read_to_string(&marker) {
            for line in contents.lines() {
                if let Some(pid) = line.strip_prefix("CLIENT_PID=") {
                    client_pid = pid.parse().unwrap_or(0);
                }
                if line == "READY" {
                    ready = true;
                }
            }
            if ready && client_pid > 0 {
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready && client_pid > 0, "live shell never became ready");
    (child, client_pid)
}

fn wait_for_socket(child: &mut Child, directory: &Path) -> PathBuf {
    let pid = child.id();
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon {pid} exited before creating its socket: {status}");
        }
        if let Ok(entries) = fs::read_dir(directory) {
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
    let socket = wait_for_socket(first.child(), &sandbox.runtime);

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
    let replacement_socket = wait_for_socket(replacement.child(), &sandbox.runtime);
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

/// Sends a request and reads records until `done`, asserting each record
/// carries the request's generation.
fn send_and_read_until_done(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut BufReader<std::process::ChildStdout>,
    generation: u64,
    cwd: &[u8],
    env: &[(&str, &str)],
) -> Vec<String> {
    stdin
        .write_all(&client_request_with_env(generation, cwd, env))
        .unwrap();
    read_until_done(stdout, generation)
}

/// Reads records until `done`, asserting each record carries the generation.
fn read_until_done(
    stdout: &mut BufReader<std::process::ChildStdout>,
    generation: u64,
) -> Vec<String> {
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
        assert_eq!(fields[1], generation.to_string());
        records.push(line.clone());
        if fields[2] == "done" {
            return records;
        }
    }
}

/// Sends a request and returns the rendered python segment fragment.
fn send_python_request(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut BufReader<std::process::ChildStdout>,
    generation: u64,
    cwd: &[u8],
    env: &[(&str, &str)],
) -> String {
    let records = send_and_read_until_done(stdin, stdout, generation, cwd, env);
    records
        .iter()
        .find_map(|record| {
            let fields: Vec<&str> = record.trim_end().split('\t').collect();
            (fields[2] == "segment" && fields[3] == "python").then(|| fields[4].to_owned())
        })
        .unwrap_or_default()
}

/// Writes a python-only theme whose environment label carries a `env:` prefix.
fn write_python_env_theme(sandbox: &Sandbox, name: &str) {
    sandbox.write_theme(
        name,
        "version = 1\n[layout]\nlines = [[\"python\"]]\nright = []\nseparator = \" | \"\nblank_line_before = false\n[segments.python]\nsymbol = \"py\"\nstyle = { foreground = \"accent\" }\nenvironment = { prefix = \"env:\" }\n",
    );
}

/// Writes a git-only theme.
fn write_git_theme(sandbox: &Sandbox, name: &str) {
    sandbox.write_theme(
        name,
        "version = 1\n[layout]\nlines = [[\"git\"]]\nright = []\nseparator = \" | \"\nblank_line_before = false\n",
    );
}

const PLAIN_PYTHON: &str =
    "#include <stdio.h>\nint main(void) { printf(\"Python 3.12.0\\n\"); return 0; }\n";

const ENV_ECHO_PYTHON: &str = "#include <stdio.h>\n#include <stdlib.h>\nint main(void) { const char* v = getenv(\"VIRTUAL_ENV\"); const char* r = getenv(\"RUSTUP_TOOLCHAIN\"); printf(\"Python %s-%s-3.12.0\\n\", v ? v : \"unset\", r ? r : \"unset\"); return 0; }\n";

/// Compiles the fake python into the sandbox, warms the dyld cache, and adds
/// the pyproject marker so the python runtime is detected. Returns the
/// request PATH value that resolves `python` to the fake.
fn install_fake_python(sandbox: &Sandbox, source: &str) -> String {
    let bin = sandbox.home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_python = bin.join("python");
    let source_path = sandbox.home.join("fake_python.c");
    fs::write(&source_path, source).unwrap();
    let compiled = Command::new("cc")
        .arg(&source_path)
        .arg("-o")
        .arg(&fake_python)
        .status()
        .unwrap();
    assert!(compiled.success(), "failed to compile fake python");
    // Warm the dyld cache so the first request's exec is fast.
    assert!(Command::new(&fake_python).status().unwrap().success());
    fs::write(sandbox.home.join("pyproject.toml"), "[]\n").unwrap();
    bin.to_str().unwrap().to_owned()
}

/// Replaces gitstatusd with one that answers after 150 ms.
fn install_delayed_gitstatusd(sandbox: &Sandbox) {
    let delayed = sandbox.data.join("ztheme/gitstatus/v1.5/gitstatusd");
    fs::create_dir_all(delayed.parent().unwrap()).unwrap();
    fs::write(&delayed, "#!/bin/sh\n/bin/sleep 0.15\nexec cat\n").unwrap();
    fs::set_permissions(&delayed, fs::Permissions::from_mode(0o700)).unwrap();
}

/// Spawns the shared server for an instance and waits for its socket.
fn spawn_server(sandbox: &Sandbox, instance: &str) -> (ChildGuard, PathBuf) {
    let server = sandbox
        .command()
        .args(["__daemon", "--dev", instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child(), &sandbox.runtime);
    (server, socket)
}

/// Runs `init zsh` for an instance and theme and extracts the compiled theme.
fn theme_hex(sandbox: &Sandbox, instance: &str, theme: &str) -> String {
    let generated = sandbox
        .command()
        .args(["init", "zsh", "--dev", instance, "--theme", theme])
        .output()
        .unwrap();
    assert_success(&generated);
    let source = String::from_utf8(generated.stdout).unwrap();
    source
        .lines()
        .find_map(|line| line.strip_prefix("typeset -g __ZTHEME_ASYNC_THEME='"))
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap()
        .to_owned()
}

/// Spawns a client daemon with the test process as its parent and optional
/// extra startup environment.
fn spawn_client_daemon(
    sandbox: &Sandbox,
    instance: &str,
    hex: &str,
    extra_env: &[(&str, &str)],
) -> (
    Child,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
) {
    let mut command = sandbox.command();
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let mut child = command
        .args([
            "__client-daemon",
            "--shell-pid",
            &std::process::id().to_string(),
            "--theme",
            hex,
            "--dev",
            instance,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

/// Shuts the shared server down and reaps it.
fn shutdown_server(server: ChildGuard, socket: &Path) {
    shutdown_outdated_daemon(socket);
    let mut server = server;
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn client_daemon_round_trips_requests_and_exits_on_eof() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let mut child = sandbox
        .command()
        .args([
            "__client-daemon",
            "--shell-pid",
            &std::process::id().to_string(),
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
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

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
        .args([
            "__client-daemon",
            "--shell-pid",
            &std::process::id().to_string(),
            "--theme",
            &hex,
            "--dev",
            &instance,
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
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

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
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

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
    write_python_env_theme(&sandbox, "envpython");
    let fake_path = install_fake_python(&sandbox, PLAIN_PYTHON);
    let (server, socket) = spawn_server(&sandbox, &instance);
    let hex = theme_hex(&sandbox, &instance, "envpython");
    let (mut child, mut stdin, mut stdout) = spawn_client_daemon(&sandbox, &instance, &hex, &[]);
    let cwd = sandbox.home.to_str().unwrap().as_bytes().to_vec();

    // Request 1: VIRTUAL_ENV unset -> no environment label.
    let first = send_python_request(&mut stdin, &mut stdout, 1, &cwd, &[("PATH", &fake_path)]);
    assert!(
        !first.contains("env:"),
        "unexpected label without VIRTUAL_ENV: {first}"
    );

    // Request 2: VIRTUAL_ENV set -> the label must reflect the new value.
    let second = send_python_request(
        &mut stdin,
        &mut stdout,
        2,
        &cwd,
        &[("PATH", &fake_path), ("VIRTUAL_ENV", "/venv-b")],
    );
    assert!(
        second.contains("env:venv-b"),
        "request environment was not applied: {second}"
    );

    // Request 3: back to the unset state; the previous value must not leak.
    let third = send_python_request(&mut stdin, &mut stdout, 3, &cwd, &[("PATH", &fake_path)]);
    assert!(
        !third.contains("env:venv-b"),
        "stale environment leaked into a later request: {third}"
    );

    drop(stdin);
    assert!(
        wait_for_exit(&mut child, PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    shutdown_server(server, &socket);
}

#[test]
fn client_daemon_cancels_in_flight_work_without_emitting_stale_records() {
    let sandbox = Sandbox::new();
    let instance = format!("client-cancel-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    write_git_theme(&sandbox, "gitonly");
    install_delayed_gitstatusd(&sandbox);
    let (server, socket) = spawn_server(&sandbox, &instance);
    let hex = theme_hex(&sandbox, &instance, "gitonly");
    let (mut child, mut stdin, mut stdout) = spawn_client_daemon(&sandbox, &instance, &hex, &[]);
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
    let records = read_until_done(&mut stdout, 2);
    assert!(records.len() >= 2, "expected records before done");

    // Request C: the client still serves normally after the supersede.
    let c_records =
        send_and_read_until_done(&mut stdin, &mut stdout, 3, &cwd, &[("PATH", fake_path)]);
    assert!(c_records.len() >= 2, "expected records before done");

    drop(stdin);
    assert!(
        wait_for_exit(&mut child, PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    shutdown_server(server, &socket);
}

fn stale_fifo_count() -> usize {
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let prefix = format!("ztheme-{}-", user_id());
    fs::read_dir(base).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(&prefix)
                    && matches!(
                        Path::new(&name).extension().and_then(OsStr::to_str),
                        Some("req" | "resp")
                    )
            })
            .count()
    })
}

/// Polls until the shared $TMPDIR holds no ztheme FIFO entries beyond the
/// baseline. Tests run in parallel, and a concurrent shell transiently
/// creates entries in the tiny window between `mkfifo` and the immediate
/// unlink, so the assertion must tolerate that and only require the entries
/// to disappear shortly after.
fn wait_for_no_stale_fifos(baseline: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if stale_fifo_count() <= baseline {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "stale FIFO entries remain after the grace period"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn count_clients(instance: &str) -> usize {
    let output = Command::new("pgrep")
        .args(["-f", &format!("client-daemon.*{instance}")])
        .output()
        .unwrap();
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout).lines().count()
    } else {
        0
    }
}

#[test]
fn shell_descriptors_are_closed_in_external_commands() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    compile_c(
        &sandbox,
        "fdprobe",
        "#include <stdio.h>\n#include <stdlib.h>\n#include <fcntl.h>\n#include <unistd.h>\n\
         int main(int argc, char **argv) { for (int i = 1; i < argc; i++) { int fd = atoi(argv[i]); \
         int flags = fcntl(fd, F_GETFD); printf(\"%s=%s \", argv[i], flags == -1 ? \"EBADF\" : \"OPEN\"); } \
         printf(\"\\n\"); return 0; }\n",
    );
    let instance = format!("fdprobe-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

    let script = format!(
        r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --dev {instance})" || exit 80
(( __ZTHEME_HAS_ASYNC )) || exit 81
"$HOME/bin/fdprobe" "$ZTHEME_REQ_FD" "$ZTHEME_RESP_FD" > "$HOME/fdprobe.out"
"#,
    );
    let output = sandbox.zsh(&script);
    assert_success(&output);
    let probe = fs::read_to_string(sandbox.home.join("fdprobe.out")).unwrap();
    let fields: Vec<&str> = probe.split_whitespace().collect();
    assert_eq!(
        fields.len(),
        2,
        "expected both prompt descriptors probed: {probe}"
    );
    assert!(
        fields.iter().all(|field| field.ends_with("=EBADF")),
        "prompt descriptors leaked into an external command: {probe}"
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
fn client_exits_on_normal_shell_exit() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("life-normal-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let (mut shell, client_pid) = spawn_live_shell(&sandbox, &instance, "");
    // Closing the shell's stdin makes its read loop end; the shell exits
    // normally and its request writer closes, so the client must terminate
    // through stdin EOF. The parent watchdog fires no earlier than one second
    // after startup, so a sub-second exit proves EOF is the mechanism.
    drop(shell.stdin.take());
    assert!(
        wait_for_pid_exit(client_pid, Duration::from_millis(900)),
        "client did not exit through EOF on normal shell exit"
    );
    let status = wait_for_exit(&mut shell, PROCESS_TIMEOUT).unwrap();
    assert!(status.success());
    wait_for_no_stale_fifos(0);
}

#[test]
fn client_exits_when_shell_is_killed() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("life-kill-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let (mut shell, client_pid) = spawn_live_shell(&sandbox, &instance, "");
    shell.kill().unwrap();
    shell.wait().unwrap();
    assert!(
        wait_for_pid_exit(client_pid, Duration::from_secs(2)),
        "client survived a SIGKILLed shell"
    );
    wait_for_no_stale_fifos(0);
}

#[test]
fn external_child_does_not_keep_client_alive() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("life-child-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

    let (mut shell, client_pid) = spawn_live_shell(
        &sandbox,
        &instance,
        "/bin/sleep 30 &\n\
         print -r -- \"CHILD_PID=$!\" >> \"$ZTHEME_TEST_MARKER\"\n",
    );
    let marker = fs::read_to_string(sandbox.home.join(format!("zt-marker-{instance}"))).unwrap();
    let child_pid: u32 = marker
        .lines()
        .find_map(|line| line.strip_prefix("CHILD_PID="))
        .unwrap()
        .parse()
        .unwrap();
    shell.kill().unwrap();
    shell.wait().unwrap();
    assert!(
        process_alive(child_pid),
        "external child should survive the shell"
    );
    assert!(
        wait_for_pid_exit(client_pid, Duration::from_secs(2)),
        "client survived with a live external child holding shell descriptors"
    );
    let _ = Command::new("kill")
        .args(["-9", &child_pid.to_string()])
        .status();
    assert!(wait_for_pid_exit(child_pid, Duration::from_secs(2)));

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn long_lived_subshell_does_not_keep_client_alive() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("life-sub-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

    let (mut shell, client_pid) = spawn_live_shell(
        &sandbox,
        &instance,
        "( while true; do /bin/sleep 30; done ) &\n\
         print -r -- \"SUB_PID=$!\" >> \"$ZTHEME_TEST_MARKER\"\n",
    );
    let marker = fs::read_to_string(sandbox.home.join(format!("zt-marker-{instance}"))).unwrap();
    let sub_pid: u32 = marker
        .lines()
        .find_map(|line| line.strip_prefix("SUB_PID="))
        .unwrap()
        .parse()
        .unwrap();
    shell.kill().unwrap();
    shell.wait().unwrap();
    // On zsh >= 5.9 the close-on-exec descriptors do not survive the subshell
    // fork either, so EOF terminates the client; the parent watchdog remains
    // the fallback if a future change reintroduces writer inheritance.
    assert!(process_alive(sub_pid), "subshell should survive the shell");
    assert!(
        wait_for_pid_exit(client_pid, Duration::from_secs(3)),
        "client survived with a live subshell"
    );
    let _ = Command::new("kill")
        .args(["-9", &sub_pid.to_string()])
        .status();
    assert!(wait_for_pid_exit(sub_pid, Duration::from_secs(2)));

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn client_with_wrong_parent_pid_exits_immediately() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("wrong-parent-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let mut child = sandbox
        .command()
        .args([
            "__client-daemon",
            "--shell-pid",
            "4294967295",
            "--theme",
            "0000",
            "--dev",
            &instance,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = wait_for_exit(&mut child, Duration::from_secs(2)).unwrap();
    assert!(status.success());
}

#[test]
fn parent_watchdog_terminates_client_when_eof_is_masked() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("watchdog-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    // The client's stdin is the wrapper's stdin, a pipe the test keeps open,
    // so request EOF can never arrive. A wrapper zsh spawns the client with
    // itself as the declared parent and then kills itself; the client is
    // reparented, and the parent watchdog must terminate it.
    let wrapper_script = format!(
        "\"$ZTHEME_TEST_BIN\" __client-daemon --shell-pid $$ --theme 0000 --dev {instance} \
         2>/dev/null &\n\
         print -r -- \"$!\" > \"$HOME/zt-client-pid\"\n\
         kill -9 $$\n"
    );
    let mut wrapper = sandbox
        .zsh_command()
        .args(["-dfc", &wrapper_script])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The wrapper kills itself with SIGKILL, so the exit status is a signal
    // death rather than a clean zero; only prompt termination is asserted.
    assert!(wait_for_exit(&mut wrapper, PROCESS_TIMEOUT).is_some());
    let client_pid: u32 = fs::read_to_string(sandbox.home.join("zt-client-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_for_pid_exit(client_pid, Duration::from_secs(3)),
        "parent watchdog did not terminate the client"
    );
    // Dropping the wrapper closes the stdin pipe that masked EOF.
    drop(wrapper);
}

#[test]
fn many_shells_do_not_accumulate_clients() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("many-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

    let mut shells = Vec::new();
    let mut clients = Vec::new();
    for _ in 0..20 {
        let (shell, client_pid) = spawn_live_shell(&sandbox, &instance, "");
        shells.push(shell);
        clients.push(client_pid);
    }
    assert_eq!(
        count_clients(&instance),
        20,
        "expected exactly one client per shell"
    );

    for (index, mut shell) in shells.into_iter().enumerate() {
        if index % 2 == 0 {
            // Normal exit: close stdin, the shell's read loop ends.
            drop(shell.stdin.take());
        } else {
            let _ = shell.kill();
        }
        let _ = wait_for_exit(&mut shell, PROCESS_TIMEOUT);
    }
    for client_pid in &clients {
        assert!(
            wait_for_pid_exit(*client_pid, Duration::from_secs(3)),
            "client {client_pid} accumulated after its shell exited"
        );
    }
    assert_eq!(count_clients(&instance), 0, "clients accumulated");
    wait_for_no_stale_fifos(0);

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn client_exits_after_shell_killed_during_active_request() {
    let sandbox = Sandbox::new();
    let instance = format!("life-active-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
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
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

    // A request whose git query is still in flight when the shell dies.
    let preamble = "print -rn -- \"ZTREQ\"$'\\0'\"1\"$'\\0'\"1\"$'\\0'\"$PWD\"$'\\0' \\\n\
         \"${PATH:-}\"$'\\0'\"${HOME:-}\"$'\\0'\"${GIT_DIR:-}\"$'\\0'\"${GIT_WORK_TREE:-}\"$'\\0' \\\n\
         \"${GIT_CEILING_DIRECTORIES:-}\"$'\\0'\"${VIRTUAL_ENV:-}\"$'\\0'\"${CONDA_PREFIX:-}\"$'\\0' \\\n\
         \"${CONDA_DEFAULT_ENV:-}\"$'\\0'\"${PERLBREW_PERL:-}\"$'\\0'\"${PLENV_VERSION:-}\"$'\\0' \\\n\
         \"${RUSTUP_TOOLCHAIN:-}\"$'\\0'\"${RBENV_VERSION:-}\"$'\\0'\"${RUBY_VERSION:-}\"$'\\0' \\\n\
         >&\"$ZTHEME_REQ_FD\"\n";
    let (mut shell, client_pid) = spawn_live_shell(&sandbox, &instance, preamble);
    thread::sleep(Duration::from_millis(100));
    shell.kill().unwrap();
    shell.wait().unwrap();
    assert!(
        wait_for_pid_exit(client_pid, Duration::from_secs(3)),
        "client did not exit after the shell died during a request"
    );
    wait_for_no_stale_fifos(0);

    shutdown_outdated_daemon(&socket);
    assert!(
        wait_for_exit(server.child(), PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    server.wait().unwrap();
}

#[test]
fn stop_client_is_idempotent() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("stop-idem-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let server = sandbox
        .command()
        .args(["__daemon", "--dev", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = ChildGuard::new(server);
    let socket = wait_for_socket(server.child(), &sandbox.runtime);

    let script = format!(
        r#"
eval "$("$ZTHEME_TEST_BIN" init zsh --dev {instance})" || exit 80
(( __ZTHEME_HAS_ASYNC )) || exit 81
_ztheme_stop_client
_ztheme_stop_client
[[ -z "$ZTHEME_CLIENT_PID" ]] || exit 83
(( ZTHEME_REQ_FD < 0 )) || exit 84
(( ZTHEME_RESP_FD < 0 )) || exit 85
print -u1 -- "OK"
"#,
    );
    let output = sandbox.zsh(&script);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("OK"));
    assert!(
        String::from_utf8_lossy(&output.stderr).is_empty(),
        "double stop produced stderr noise: {}",
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
fn runtime_child_receives_the_request_environment() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!(
        "client-child-env-{}",
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    write_python_env_theme(&sandbox, "envpython");
    let fake_path = install_fake_python(&sandbox, ENV_ECHO_PYTHON);
    let (server, socket) = spawn_server(&sandbox, &instance);
    let hex = theme_hex(&sandbox, &instance, "envpython");

    // Start the client with a VIRTUAL_ENV of its own: a request that leaves
    // the field empty must still remove it from the child command, proving
    // explicit env_remove rather than accidental inheritance.
    let (mut child, mut stdin, mut stdout) = spawn_client_daemon(
        &sandbox,
        &instance,
        &hex,
        &[
            ("VIRTUAL_ENV", "/startup-venv"),
            ("RUSTUP_TOOLCHAIN", "startup-toolchain"),
        ],
    );
    let cwd = sandbox.home.to_str().unwrap().as_bytes().to_vec();

    // Request 1: the child must see exactly the request values.
    let first = send_python_request(
        &mut stdin,
        &mut stdout,
        1,
        &cwd,
        &[
            ("PATH", &fake_path),
            ("VIRTUAL_ENV", "/venv-a"),
            ("RUSTUP_TOOLCHAIN", "nightly"),
        ],
    );
    assert!(
        first.contains("/venv-a-nightly-3.12.0"),
        "child did not receive the request environment: {first}"
    );

    // Request 2: empty fields must remove the inherited startup values.
    let second = send_python_request(&mut stdin, &mut stdout, 2, &cwd, &[("PATH", &fake_path)]);
    assert!(
        second.contains("unset-unset-3.12.0"),
        "child inherited stale startup environment: {second}"
    );

    drop(stdin);
    assert!(
        wait_for_exit(&mut child, PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    shutdown_server(server, &socket);
}

#[test]
fn two_clients_share_one_server_without_environment_contamination() {
    let sandbox = Sandbox::new();
    sandbox.install_fake_gitstatus();
    let instance = format!("client-two-{}", SEQUENCE.fetch_add(1, Ordering::Relaxed));
    write_python_env_theme(&sandbox, "envpython");
    let fake_path = install_fake_python(&sandbox, PLAIN_PYTHON);
    let (server, socket) = spawn_server(&sandbox, &instance);
    let hex = theme_hex(&sandbox, &instance, "envpython");
    let (mut client_a, mut stdin_a, mut stdout_a) =
        spawn_client_daemon(&sandbox, &instance, &hex, &[]);
    let (mut client_b, mut stdin_b, mut stdout_b) =
        spawn_client_daemon(&sandbox, &instance, &hex, &[]);
    let cwd = sandbox.home.to_str().unwrap().as_bytes().to_vec();

    // Both clients stay live against the one shared server. The requests are
    // sequential (each answered before the next is sent, so nothing
    // supersedes): client A renders its own value, client B renders its own,
    // and client A's later request renders its own again rather than
    // retaining A's earlier value or absorbing B's. The server is started
    // before any request, so this does not exercise daemon startup; the
    // absence of process-global mutation is what structurally rules out the
    // daemon inheriting a request's transient environment.
    let a1 = send_python_request(
        &mut stdin_a,
        &mut stdout_a,
        1,
        &cwd,
        &[("PATH", &fake_path), ("VIRTUAL_ENV", "/venv-a")],
    );
    let b1 = send_python_request(
        &mut stdin_b,
        &mut stdout_b,
        2,
        &cwd,
        &[("PATH", &fake_path), ("VIRTUAL_ENV", "/venv-b")],
    );
    let a2 = send_python_request(
        &mut stdin_a,
        &mut stdout_a,
        3,
        &cwd,
        &[("PATH", &fake_path), ("VIRTUAL_ENV", "/venv-c")],
    );

    assert!(
        a1.contains("env:venv-a"),
        "client A did not render its own environment: {a1}"
    );
    assert!(
        b1.contains("env:venv-b"),
        "client B did not render its own environment: {b1}"
    );
    assert!(
        a2.contains("env:venv-c"),
        "client A's later request was contaminated: {a2}"
    );

    drop(stdin_a);
    drop(stdin_b);
    assert!(
        wait_for_exit(&mut client_a, PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    assert!(
        wait_for_exit(&mut client_b, PROCESS_TIMEOUT)
            .unwrap()
            .success()
    );
    shutdown_server(server, &socket);
}
