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
use crate::environment::PromptEnvironment;
use crate::prompt::snapshot;
use crate::theme::AsyncTheme;

const REQUEST_MAGIC: &[u8] = b"ZTREQ";
const REQUEST_VERSION: &[u8] = b"1";

/// How often the client verifies that the shell that spawned it is still its
/// parent. EOF on the request pipe is the primary lifetime signal; this
/// watchdog is an independent fallback for cases where EOF propagation could
/// be masked, such as descriptor leakage, transport changes, or an
/// unexpected wrapper process.
const PARENT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

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

    let mut current: Option<JoinHandle<io::Result<()>>> = None;
    let mut parent_check = interval_at(
        Instant::now() + PARENT_CHECK_INTERVAL,
        PARENT_CHECK_INTERVAL,
    );
    parent_check.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            request = receiver.recv() => {
                if let Some(request) = request {
                    // A new request supersedes any in-flight work: the records
                    // of an older generation would be ignored by the shell, and
                    // letting the work run on would waste time and read the new
                    // request's environment. The superseded task is aborted and
                    // awaited before the environment is touched: its JoinSet
                    // drops only when the task is actually destroyed, and the
                    // request tasks read environment values after awaits, so
                    // without the await they could resume after the mutation
                    // below with the new request's environment. This ordering
                    // (destroy the request task and its JoinSet, then mutate
                    // the environment) is the correctness invariant; the
                    // integration tests can only observe its black-box
                    // consequences (no stale records, clean per-request
                    // environment), so this comment carries the stronger
                    // guarantee.
                    if let Some(handle) = current.take() {
                        handle.abort();
                        if let Ok(Err(error)) = handle.await {
                            // The request's records could not be written, so
                            // the response pipe is gone; keep serving has no
                            // point.
                            return Err(error);
                        }
                    }
                    let instance = instance.clone();
                    let theme = Arc::clone(&theme);
                    let environment = Arc::new(request.environment);
                    current = Some(tokio::spawn(async move {
                        snapshot(
                            request.generation,
                            request.cwd,
                            instance,
                            environment,
                            &theme,
                        )
                        .await
                    }));
                } else {
                    // EOF only arrives after the writer closes, so finish any
                    // in-flight request first: its records still have a reader,
                    // or its writes fail with EPIPE if the shell is really gone.
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
                    // plain OS thread, so returning drops the runtime without
                    // waiting on its blocked stdin read.
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
fn spawn_request_reader(sender: mpsc::Sender<Request>) {
    std::thread::Builder::new()
        .name("ztheme-client-requests".into())
        .spawn(move || {
            let mut reader = std::io::BufReader::new(std::io::stdin());
            loop {
                match read_request(&mut reader) {
                    Ok(Some(request)) => {
                        if sender.blocking_send(request).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
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

/// Parses one NUL-delimited request. `Ok(None)` means a clean EOF before any
/// field; every field after the magic is required, so a request that is cut
/// short is rejected rather than silently accepted with missing values.
fn read_request<R>(reader: &mut R) -> io::Result<Option<Request>>
where
    R: std::io::BufRead,
{
    let magic = read_field(reader)?;
    let Some(magic) = magic else {
        return Ok(None);
    };
    if magic != REQUEST_MAGIC {
        return Err(invalid_data("client request magic is invalid"));
    }
    let version = read_field(reader)?.ok_or_else(truncated)?;
    if version != REQUEST_VERSION {
        return Err(invalid_data("client request version is unsupported"));
    }

    let generation = read_field(reader)?.ok_or_else(truncated)?;
    let generation = std::str::from_utf8(&generation)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_data("client request generation is invalid"))?;

    let cwd = read_field(reader)?.ok_or_else(truncated)?;
    let cwd = PathBuf::from(OsString::from_vec(cwd));
    if !cwd.is_absolute() {
        return Err(invalid_data("client request cwd is not absolute"));
    }

    let environment = PromptEnvironment {
        path: env_field(read_field(reader)?)?,
        home: env_field(read_field(reader)?)?,
        git_dir: env_field(read_field(reader)?)?,
        git_work_tree: env_field(read_field(reader)?)?,
        git_ceilings: env_field(read_field(reader)?)?,
        virtual_env: env_field(read_field(reader)?)?,
        conda_prefix: env_field(read_field(reader)?)?,
        conda_default_env: env_field(read_field(reader)?)?,
        perlbrew_perl: env_field(read_field(reader)?)?,
        plenv_version: env_field(read_field(reader)?)?,
        rustup_toolchain: env_field(read_field(reader)?)?,
        rbenv_version: env_field(read_field(reader)?)?,
        ruby_version: env_field(read_field(reader)?)?,
    };

    Ok(Some(Request {
        generation,
        cwd,
        environment,
    }))
}

fn read_field<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: std::io::BufRead,
{
    let mut field = Vec::with_capacity(64);
    if reader.read_until(0, &mut field)? == 0 {
        return Ok(None);
    }
    if field.pop() != Some(0) {
        return Err(invalid_data("client request field is not NUL-terminated"));
    }
    Ok(Some(field))
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

    use super::{REQUEST_MAGIC, REQUEST_VERSION, read_request};

    const ENV_FIELD_COUNT: usize = 13;

    fn request(cwd: &[u8], fields: &[&[u8]]) -> Vec<u8> {
        assert_eq!(fields.len(), ENV_FIELD_COUNT);
        let mut bytes = REQUEST_MAGIC.to_vec();
        bytes.push(0);
        bytes.extend_from_slice(REQUEST_VERSION);
        bytes.push(0);
        bytes.extend_from_slice(b"42");
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
    fn parses_a_complete_request_with_environment() {
        let fields: [&[u8]; ENV_FIELD_COUNT] = [
            b"/opt/bin:/usr/bin",
            b"/home/user",
            b"",
            b"/work/tree",
            b"",
            b"",
            b"",
            b"",
            b"",
            b"",
            b"",
            b"",
            b"",
        ];
        let bytes = request(b"/work/project", &fields);
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
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn non_utf8_cwd_and_environment_round_trip() {
        let mut git_dir = b"/repo-".to_vec();
        git_dir.push(0xff);
        let fields: [&[u8]; ENV_FIELD_COUNT] = [
            b"", b"", &git_dir, b"", b"", b"", b"", b"", b"", b"", b"", b"", b"",
        ];
        let mut cwd = b"/cwd-".to_vec();
        cwd.push(0xfe);
        let bytes = request(&cwd, &fields);
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
        let empty: [&[u8]; ENV_FIELD_COUNT] = [b""; ENV_FIELD_COUNT];
        let valid = request(b"/work", &empty);

        // bytes: ZTREQ\0 1\0 42\0 /work\0 then 13 empty fields
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
    }
}
