use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::process::parent_id;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::daemon;
use crate::environment::{PromptEnvironment, REQUEST_FIELDS};
use crate::prompt::protocol::{REQUEST_MAGIC, REQUEST_VERSION};
use crate::prompt::snapshot;
use crate::theme::AsyncTheme;

/// How often the client verifies that the shell that spawned it is still its
/// parent. EOF on the request pipe is the primary lifetime signal; this
/// watchdog is an independent fallback for cases where EOF propagation could
/// be masked, such as descriptor leakage, transport changes, or an
/// unexpected wrapper process.
const PARENT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Includes arbitrary path/selector bytes, but never permits unbounded input
/// allocation. The frame budget includes every NUL terminator.
const MAX_REQUEST_FIELD_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BYTES: usize = 128 * 1024;

/// Serves one shell's prompt requests for the shell's lifetime.
///
/// Requests arrive on stdin as NUL-delimited fields (see `read_request`);
/// rendered records go to stdout using the same `ZTHEME1` line protocol the
/// shell integration consumes. The shell owns the request pipe's write end,
/// so EOF on stdin means the shell is gone and the client exits. The parent
/// watchdog is an independent fallback: when the shell dies, the client is
/// reparented, so comparing `parent_id` against `shell_pid` detects the death
/// even if EOF propagation is ever masked by descriptor leakage, transport
/// changes, or an unexpected wrapper process.
pub async fn serve_client(
    instance: daemon::Instance,
    shell_pid: u32,
    theme: Arc<AsyncTheme>,
) -> io::Result<()> {
    // A client spawned by anything other than the intended shell is a stale
    // or misconfigured process: exit without doing any work.
    if parent_id() != shell_pid {
        return Ok(());
    }

    let (sender, mut receiver) = mpsc::channel(4);
    spawn_request_reader(sender);

    let executor = super::planning::PlanningExecutor::new();
    let mut current: Option<JoinHandle<io::Result<()>>> = None;
    let mut parent_check = interval_at(
        Instant::now() + PARENT_CHECK_INTERVAL,
        PARENT_CHECK_INTERVAL,
    );
    parent_check.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            request = receiver.recv() => {
                if let Some(input) = request {
                    match input {
                        ClientInput::Request(request) => {
                            // A new request supersedes any in-flight work: the
                            // records of an older generation would be ignored by
                            // the shell, and letting the work run on would waste
                            // time. Await destruction before starting the next
                            // snapshot so no stale records can be emitted.
                            // Blocking planning workers may outlive cancellation;
                            // they retain immutable request-owned inputs and
                            // their executor permits until they really finish.
                            if let Some(handle) = current.take() {
                                handle.abort();
                                if let Ok(Err(error)) = handle.await {
                                    // The request's records could not be written,
                                    // so the response pipe is gone; keep serving
                                    // has no point.
                                    return Err(error);
                                }
                            }
                            let request = *request;
                            let instance = instance.clone();
                            let theme = Arc::clone(&theme);
                            let environment = Arc::new(request.environment);
                            let executor = executor.clone();
                            current = Some(tokio::spawn(async move {
                                snapshot(
                                    request.generation,
                                    request.cwd,
                                    instance,
                                    environment,
                                    &theme,
                                    executor,
                                )
                                .await
                            }));
                        }
                        ClientInput::ProtocolError { generation, message } => {
                            // The shell sent an unsupported request version:
                            // emit a generation-tagged record the shell can
                            // surface through `zle -M`, then exit. A stale shell
                            // integration otherwise degrades to a plain prompt
                            // with no explanation.
                            crate::prompt::protocol::write_error(
                                &mut io::stdout().lock(),
                                generation,
                                "snapshot",
                                message,
                            )?;
                            cancel_current(&mut current).await;
                            return Ok(());
                        }
                    }
                } else {
                    // EOF means the request writer is gone: stop async work.
                    // Client runtime shutdown also bounds the wait for any
                    // uncancelable filesystem planning worker.
                    cancel_current(&mut current).await;
                    break;
                }
            }
            _ = parent_check.tick() => {
                if parent_id() != shell_pid {
                    // The shell is gone even though EOF may not have arrived;
                    // stop the current request and exit rather than waiting on
                    // the primary signal. The shell integration respawns a
                    // fresh client on the next prompt. The request reader is a
                    // plain OS thread, so runtime shutdown does not wait on its
                    // blocked stdin read; planning workers have a bounded wait.
                    cancel_current(&mut current).await;
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Aborts and awaits the in-flight request task, destroying its `JoinSet`
/// and the request tasks it spawned.
async fn cancel_current(current: &mut Option<JoinHandle<io::Result<()>>>) {
    if let Some(handle) = current.take() {
        handle.abort();
        let _ = handle.await;
    }
}

/// Reads requests from stdin on a dedicated thread. A plain OS thread rather
/// than a Tokio task or blocking task is used so that dropping the runtime
/// never waits on the stdin read: when the parent watchdog exits the client
/// while the request pipe is still open, the runtime must shut down without
/// joining a read that can never return.
fn spawn_request_reader(sender: mpsc::Sender<ClientInput>) {
    std::thread::Builder::new()
        .name("ztheme-client-requests".into())
        .spawn(move || {
            let mut reader = std::io::BufReader::new(std::io::stdin());
            loop {
                match read_request(&mut reader) {
                    Ok(Some(request)) => {
                        if sender
                            .blocking_send(ClientInput::Request(Box::new(request)))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        if let Some(mismatch) = error
                            .get_ref()
                            .and_then(|cause| cause.downcast_ref::<RequestVersionMismatch>())
                        {
                            if sender
                                .blocking_send(ClientInput::ProtocolError {
                                    generation: mismatch.generation,
                                    message: SHELL_INTEGRATION_OUT_OF_DATE,
                                })
                                .is_err()
                            {
                                return;
                            }
                            return;
                        }
                        eprintln!("ztheme: client daemon request failed: {error}");
                        return;
                    }
                }
            }
        })
        .expect("spawning the request reader thread cannot fail");
}

struct Request {
    generation: u64,
    cwd: PathBuf,
    environment: PromptEnvironment,
}

/// What the request reader passes to the serving loop: a parseable request,
/// or a version mismatch that must be reported to the shell before exiting.
enum ClientInput {
    Request(Box<Request>),
    ProtocolError {
        generation: u64,
        message: &'static str,
    },
}

/// Shown once through the shell's `zle -M` before the client exits, so a
/// stale shell integration is a diagnosed one-liner instead of a silent
/// plain prompt.
const SHELL_INTEGRATION_OUT_OF_DATE: &str = "client request version is unsupported; \
    regenerate the shell integration with `ztheme init zsh` and restart the shell";

/// Payload attached to the version-mismatch error so the request reader can
/// surface a generation-tagged diagnostic.
#[derive(Debug)]
struct RequestVersionMismatch {
    generation: u64,
}

impl std::fmt::Display for RequestVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("client request version is unsupported")
    }
}

impl std::error::Error for RequestVersionMismatch {}

fn request_version_mismatch(generation: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        RequestVersionMismatch { generation },
    )
}

/// Parses one NUL-delimited request. `Ok(None)` means a clean EOF before any
/// field; every field after the magic is required, so a request that is cut
/// short is rejected rather than silently accepted with missing values.
fn read_request<R>(reader: &mut R) -> io::Result<Option<Request>>
where
    R: std::io::BufRead,
{
    let mut remaining = MAX_REQUEST_BYTES;
    let magic = read_field(reader, &mut remaining)?;
    let Some(magic) = magic else {
        return Ok(None);
    };
    if magic != REQUEST_MAGIC {
        return Err(invalid_data("client request magic is invalid"));
    }
    let version = read_field(reader, &mut remaining)?.ok_or_else(truncated)?;

    let generation = read_field(reader, &mut remaining)?.ok_or_else(truncated)?;
    let generation = std::str::from_utf8(&generation)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_data("client request generation is invalid"))?;

    // The version is checked after the generation is read so a stale shell
    // integration can be diagnosed with a generation-tagged record instead of
    // failing without a trace.
    if version != REQUEST_VERSION.as_bytes() {
        return Err(request_version_mismatch(generation));
    }

    let cwd = read_field(reader, &mut remaining)?.ok_or_else(truncated)?;
    let cwd = PathBuf::from(OsString::from_vec(cwd));
    if !cwd.is_absolute() {
        return Err(invalid_data("client request cwd is not absolute"));
    }

    let mut environment = PromptEnvironment::default();
    for field in REQUEST_FIELDS {
        field.set(
            &mut environment,
            env_field(read_field(reader, &mut remaining)?)?,
        );
    }

    Ok(Some(Request {
        generation,
        cwd,
        environment,
    }))
}

fn read_field<R>(reader: &mut R, remaining: &mut usize) -> io::Result<Option<Vec<u8>>>
where
    R: std::io::BufRead,
{
    let mut field = Vec::with_capacity(64);
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return if field.is_empty() {
                Ok(None)
            } else {
                Err(invalid_data("client request field is not NUL-terminated"))
            };
        }
        let terminator = buffer.iter().position(|&byte| byte == 0);
        let length = terminator.unwrap_or(buffer.len());
        let consumed = length + usize::from(terminator.is_some());
        if consumed > *remaining {
            return Err(invalid_data("client request exceeds frame size limit"));
        }
        if length > MAX_REQUEST_FIELD_BYTES - field.len() {
            return Err(invalid_data("client request field exceeds size limit"));
        }
        field.extend_from_slice(&buffer[..length]);
        reader.consume(consumed);
        *remaining -= consumed;
        if terminator.is_some() {
            return Ok(Some(field));
        }
    }
}
/// An environment field is always present on the wire; an empty value means
/// the variable is unset in the shell.
fn env_field(field: Option<Vec<u8>>) -> io::Result<Option<OsString>> {
    match field {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => Ok(Some(OsString::from_vec(value))),
        None => Err(truncated()),
    }
}

fn truncated() -> io::Error {
    invalid_data("client request is truncated")
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use std::io::BufReader;

    use super::{
        MAX_REQUEST_BYTES, MAX_REQUEST_FIELD_BYTES, REQUEST_MAGIC, REQUEST_VERSION,
        RequestVersionMismatch, read_field, read_request,
    };
    use crate::environment::REQUEST_FIELDS;

    const ENV_FIELD_COUNT: usize = REQUEST_FIELDS.len();

    fn request(cwd: &[u8], values: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = REQUEST_MAGIC.to_vec();
        bytes.push(0);
        bytes.extend_from_slice(REQUEST_VERSION.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(b"42");
        bytes.push(0);
        bytes.extend_from_slice(cwd);
        bytes.push(0);
        // The wire layout comes from the shared definition, so a field added
        // to REQUEST_FIELDS appears here without a manual step.
        for field in REQUEST_FIELDS {
            let value = match values.iter().find(|&&(name, _)| name == field.name) {
                Some(&(_, value)) => value,
                None => &[],
            };
            bytes.extend_from_slice(value);
            bytes.push(0);
        }
        bytes
    }

    /// Literal version-3 bytes are intentionally independent of the descriptor
    /// list. Assert storage and child routing as well as the generated layout.
    #[test]
    fn version_three_wire_order_storage_and_child_policy_are_pinned() {
        let wire = b"ZTREQ\x003\x0042\x00/work\x00PATH\x00HOME\x00GIT_DIR\x00GIT_WORK_TREE\x00\
            GIT_CEILING_DIRECTORIES\x00VIRTUAL_ENV\x00CONDA_PREFIX\x00CONDA_DEFAULT_ENV\x00\
            PERLBREW_PERL\x00PLENV_VERSION\x00PYENV_VERSION\x00PYENV_DIR\x00RUSTUP_TOOLCHAIN\x00\
            RUSTUP_HOME\x00RBENV_DIR\x00RBENV_VERSION\x00NODENV_VERSION\x00NODENV_DIR\x00PLENV_DIR\x00\
            RUBY_VERSION\x00JAVA_HOME\x00GOTOOLCHAIN\x00DOTNET_ROOT\x00JULIAUP_CHANNEL\x00\
            JULIAUP_DEPOT_PATH\x00JULIA_PROJECT\x00JULIA_LOAD_PATH\x00JULIA_DEPOT_PATH\x00R_ARCH\x00";
        let request = read_request(&mut BufReader::with_capacity(3, &wire[..]))
            .unwrap()
            .unwrap();
        let environment = request.environment;
        let storage = [
            environment.path.as_deref(),
            environment.home.as_deref(),
            environment.git_dir.as_deref(),
            environment.git_work_tree.as_deref(),
            environment.git_ceilings.as_deref(),
            environment.virtual_env.as_deref(),
            environment.conda_prefix.as_deref(),
            environment.conda_default_env.as_deref(),
            environment.perlbrew_perl.as_deref(),
            environment.plenv_version.as_deref(),
            environment.pyenv_version.as_deref(),
            environment.pyenv_dir.as_deref(),
            environment.rustup_toolchain.as_deref(),
            environment.rustup_home.as_deref(),
            environment.rbenv_dir.as_deref(),
            environment.rbenv_version.as_deref(),
            environment.nodenv_version.as_deref(),
            environment.nodenv_dir.as_deref(),
            environment.plenv_dir.as_deref(),
            environment.ruby_version.as_deref(),
            environment.java_home.as_deref(),
            environment.gotoolchain.as_deref(),
            environment.dotnet_root.as_deref(),
            environment.juliaup_channel.as_deref(),
            environment.juliaup_depot_path.as_deref(),
            environment.julia_project.as_deref(),
            environment.julia_load_path.as_deref(),
            environment.julia_depot_path.as_deref(),
            environment.r_arch.as_deref(),
        ];
        let mut command = tokio::process::Command::new("true");
        environment.apply_to_command(&mut command);
        let envs: std::collections::HashMap<_, _> = command.as_std().get_envs().collect();
        let expected_names = [
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
            "PYENV_VERSION",
            "PYENV_DIR",
            "RUSTUP_TOOLCHAIN",
            "RUSTUP_HOME",
            "RBENV_DIR",
            "RBENV_VERSION",
            "NODENV_VERSION",
            "NODENV_DIR",
            "PLENV_DIR",
            "RUBY_VERSION",
            "JAVA_HOME",
            "GOTOOLCHAIN",
            "DOTNET_ROOT",
            "JULIAUP_CHANNEL",
            "JULIAUP_DEPOT_PATH",
            "JULIA_PROJECT",
            "JULIA_LOAD_PATH",
            "JULIA_DEPOT_PATH",
            "R_ARCH",
        ];
        assert_eq!(storage, expected_names.map(|name| Some(OsStr::new(name))));
        assert_eq!(
            REQUEST_FIELDS
                .iter()
                .map(|field| field.name)
                .collect::<Vec<_>>(),
            expected_names
        );
        let expected_shell = expected_names
            .iter()
            .map(|name| format!("    request_line+=\"${{{name}:-}}\"$'\\0'"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            crate::prompt::protocol::request_field_lines(),
            expected_shell
        );
        for name in expected_names {
            if ["GIT_DIR", "GIT_WORK_TREE", "GIT_CEILING_DIRECTORIES"].contains(&name) {
                assert!(!envs.contains_key(OsStr::new(name)));
            } else {
                assert_eq!(envs[OsStr::new(name)], Some(OsStr::new(name)));
            }
        }
    }

    #[test]
    fn request_field_limit_accepts_exact_size_and_rejects_excess_without_a_nul() {
        let value = vec![b'x'; MAX_REQUEST_FIELD_BYTES];
        let exact = request(b"/work", &[("PATH", &value)]);
        assert!(
            read_request(&mut BufReader::with_capacity(7, &exact[..]))
                .unwrap()
                .is_some()
        );
        let oversized = vec![b'x'; MAX_REQUEST_FIELD_BYTES + 1];
        for suffix in [&b""[..], &b"\0"[..]] {
            let mut bytes = oversized.clone();
            bytes.extend_from_slice(suffix);
            let mut reader = BufReader::with_capacity(7, &bytes[..]);
            let mut remaining = MAX_REQUEST_BYTES;
            let error = read_field(&mut reader, &mut remaining).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "client request field exceeds size limit");
        }
    }

    #[test]
    fn request_frame_budget_includes_terminators_and_resets_for_each_request() {
        let mut exact = request(b"/work", &[]);
        // Fill eight fields without exceeding any individual field limit.
        let header_len = b"ZTREQ\x003\x0042\x00/work\x00".len();
        let padding = MAX_REQUEST_BYTES - exact.len();
        let mut remaining = padding;
        for index in (0..8).rev() {
            let length = remaining.min(MAX_REQUEST_FIELD_BYTES);
            exact.splice(
                header_len + index..header_len + index,
                std::iter::repeat_n(b'x', length),
            );
            remaining -= length;
        }
        assert_eq!(remaining, 0);
        assert_eq!(exact.len(), MAX_REQUEST_BYTES);
        let mut two_frames = exact.clone();
        two_frames.extend_from_slice(&exact);
        let mut reader = BufReader::with_capacity(7, &two_frames[..]);
        assert!(read_request(&mut reader).unwrap().is_some());
        assert!(read_request(&mut reader).unwrap().is_some());
        assert!(read_request(&mut reader).unwrap().is_none());
        exact.insert(header_len + 1, b'x');
        let error = read_request(&mut BufReader::with_capacity(7, &exact[..]))
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "client request exceeds frame size limit");
    }

    #[test]
    fn parses_a_complete_request_with_environment() {
        let bytes = request(
            b"/work/project",
            &[
                ("PATH", b"/opt/bin:/usr/bin"),
                ("HOME", b"/home/user"),
                ("GIT_WORK_TREE", b"/work/tree"),
            ],
        );
        let mut reader = BufReader::new(&bytes[..]);
        let request = read_request(&mut reader).unwrap().unwrap();

        assert_eq!(request.generation, 42);
        assert_eq!(request.cwd, Path::new("/work/project"));
        assert_eq!(
            request.environment.path.as_deref(),
            Some(OsStr::new("/opt/bin:/usr/bin"))
        );
        assert_eq!(
            request.environment.home.as_deref(),
            Some(OsStr::new("/home/user"))
        );
        assert_eq!(request.environment.git_dir, None);
        assert_eq!(
            request.environment.git_work_tree.as_deref(),
            Some(OsStr::new("/work/tree"))
        );
        assert_eq!(request.environment.juliaup_channel, None);
        assert_eq!(request.environment.juliaup_depot_path, None);
        assert_eq!(request.environment.julia_project, None);
        assert_eq!(request.environment.julia_load_path, None);
        assert_eq!(request.environment.julia_depot_path, None);
        assert_eq!(request.environment.r_arch, None);
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn new_selector_fields_round_trip_and_empty_values_are_none() {
        let bytes = request(
            b"/work",
            &[
                ("JULIAUP_CHANNEL", b"release"),
                ("JULIAUP_DEPOT_PATH", b"/depot-a"),
                ("JULIA_PROJECT", b"@project"),
                ("JULIA_LOAD_PATH", b":"),
                ("JULIA_DEPOT_PATH", b"/depot-b"),
                ("R_ARCH", b"/x86_64"),
            ],
        );
        let mut reader = BufReader::new(&bytes[..]);
        let request = read_request(&mut reader).unwrap().unwrap();

        assert_eq!(
            request.environment.juliaup_channel.as_deref(),
            Some(OsStr::new("release"))
        );
        assert_eq!(
            request.environment.juliaup_depot_path.as_deref(),
            Some(OsStr::new("/depot-a"))
        );
        assert_eq!(
            request.environment.julia_project.as_deref(),
            Some(OsStr::new("@project"))
        );
        assert_eq!(
            request.environment.julia_load_path.as_deref(),
            Some(OsStr::new(":"))
        );
        assert_eq!(
            request.environment.julia_depot_path.as_deref(),
            Some(OsStr::new("/depot-b"))
        );
        assert_eq!(
            request.environment.r_arch.as_deref(),
            Some(OsStr::new("/x86_64"))
        );
    }

    #[test]
    fn non_utf8_selector_values_round_trip_as_os_string() {
        let mut channel = b"julia-".to_vec();
        channel.push(0xff);
        let mut arch = b"R_".to_vec();
        arch.push(0xfe);
        let bytes = request(
            b"/work",
            &[
                ("JULIAUP_CHANNEL", channel.as_slice()),
                ("R_ARCH", arch.as_slice()),
            ],
        );
        let mut reader = BufReader::new(&bytes[..]);
        let request = read_request(&mut reader).unwrap().unwrap();

        assert_eq!(
            request.environment.juliaup_channel.as_deref(),
            Some(OsStr::from_bytes(&channel))
        );
        assert_eq!(
            request.environment.r_arch.as_deref(),
            Some(OsStr::from_bytes(&arch))
        );
    }

    #[test]
    fn non_utf8_cwd_and_environment_round_trip() {
        let mut git_dir = b"/repo-".to_vec();
        git_dir.push(0xff);
        let mut cwd = b"/cwd-".to_vec();
        cwd.push(0xfe);
        let bytes = request(&cwd, &[("GIT_DIR", git_dir.as_slice())]);
        let mut reader = BufReader::new(&bytes[..]);
        let request = read_request(&mut reader).unwrap().unwrap();

        assert_eq!(request.cwd.as_os_str(), OsStr::from_bytes(&cwd));
        assert_eq!(
            request.environment.git_dir.as_deref(),
            Some(OsStr::from_bytes(&git_dir))
        );
    }

    #[test]
    fn malformed_requests_are_rejected() {
        let valid = request(b"/work", &[]);

        // bytes: ZTREQ\0 3\0 42\0 /work\0 then 29 empty fields
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        assert!(read_request(&mut BufReader::new(&bad_magic[..])).is_err());

        let mut bad_version = valid.clone();
        bad_version[6] = b'2';
        assert!(read_request(&mut BufReader::new(&bad_version[..])).is_err());

        let mut bad_generation = valid.clone();
        bad_generation[8] = b'x';
        assert!(read_request(&mut BufReader::new(&bad_generation[..])).is_err());

        let mut relative_cwd = valid.clone();
        relative_cwd[11] = b'.';
        assert!(read_request(&mut BufReader::new(&relative_cwd[..])).is_err());

        let truncated = &valid[..valid.len() - 3];
        assert!(read_request(&mut BufReader::new(truncated)).is_err());

        // An older request version: the mismatch must carry the generation so
        // the reader can emit a diagnostic the shell can surface.
        let mut stale = REQUEST_MAGIC.to_vec();
        stale.push(0);
        stale.extend_from_slice(b"2");
        stale.push(0);
        stale.extend_from_slice(b"42");
        stale.push(0);
        stale.extend_from_slice(b"/work");
        stale.push(0);
        stale.resize(stale.len() + ENV_FIELD_COUNT, 0);

        let Err(error) = read_request(&mut BufReader::new(&stale[..])) else {
            panic!("an old request version must be rejected");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let mismatch = error
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<RequestVersionMismatch>())
            .expect("version mismatch must carry a typed payload");
        assert_eq!(mismatch.generation, 42);
    }

    #[test]
    fn clean_eof_is_a_normal_stop_but_partial_requests_are_rejected() {
        assert!(
            read_request(&mut BufReader::new(&b""[..]))
                .unwrap()
                .is_none()
        );
        let partial = b"ZTREQ\0";
        assert!(read_request(&mut BufReader::new(&partial[..])).is_err());
        for partial in [&b"ZTREQ"[..], &b"ZTREQ\x003"[..], &b"ZTREQ\x003\x0042"[..]] {
            let error = read_request(&mut BufReader::with_capacity(2, partial))
                .err()
                .unwrap();
            assert_eq!(
                error.to_string(),
                "client request field is not NUL-terminated"
            );
        }
    }
}
