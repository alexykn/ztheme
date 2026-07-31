use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::daemon;
use crate::prompt::snapshot;
use crate::theme::AsyncTheme;

const REQUEST_MAGIC: &[u8] = b"ZTREQ";
const REQUEST_VERSION: &[u8] = b"1";

/// Serves one shell's prompt requests for the shell's lifetime.
///
/// Requests arrive on stdin as NUL-delimited fields (see `read_request`);
/// rendered records go to stdout using the same `ZTHEME1` line protocol the
/// short-lived `__snapshot` helper uses. The shell owns the request pipe's
/// write end, so EOF on stdin means the shell is gone and the client exits.
pub async fn serve_client(instance: daemon::Instance, theme: Arc<AsyncTheme>) -> io::Result<()> {
    let (sender, mut receiver) = mpsc::channel(4);
    let reader = tokio::spawn(read_requests(sender));

    let mut current: Option<JoinHandle<io::Result<()>>> = None;
    loop {
        match receiver.recv().await {
            Some(request) => {
                // A new request supersedes any in-flight work, matching the
                // previous design where zsh killed the short-lived helper of
                // an older prompt. The superseded task is aborted and awaited
                // before the environment is touched: its JoinSet drops only
                // when the task is actually destroyed, and the request tasks
                // read environment values after awaits, so without the await
                // they could resume after the mutation below with the new
                // request's environment. This ordering (destroy the request
                // task and its JoinSet, then mutate the environment) is the
                // correctness invariant; the integration tests can only
                // observe its black-box consequences (no stale records, clean
                // per-request environment), so this comment carries the
                // stronger guarantee.
                if let Some(handle) = current.take() {
                    handle.abort();
                    if let Ok(Err(error)) = handle.await {
                        // The request's records could not be written, so the
                        // response pipe is gone; keep serving has no point.
                        return Err(error);
                    }
                }
                apply_request_env(&request.env);
                let instance = instance.clone();
                let theme = Arc::clone(&theme);
                current = Some(tokio::spawn(async move {
                    snapshot(request.generation, request.cwd, instance, &theme).await
                }));
            }
            None => {
                // EOF only arrives after the writer closes, so finish any
                // in-flight request first: its records still have a reader, or
                // its writes fail with EPIPE if the shell is really gone.
                if let Some(handle) = current.take() {
                    handle.abort();
                    let _ = handle.await;
                }
                break;
            }
        }
    }
    reader.abort();
    Ok(())
}

async fn read_requests(sender: mpsc::Sender<Request>) {
    let mut reader = BufReader::new(tokio::io::stdin());
    loop {
        match read_request(&mut reader).await {
            Ok(Some(request)) => {
                if sender.send(request).await.is_err() {
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
}

struct Request {
    generation: u64,
    cwd: PathBuf,
    env: RequestEnv,
}

#[derive(Default)]
struct RequestEnv {
    path: Option<OsString>,
    home: Option<OsString>,
    git_dir: Option<OsString>,
    git_work_tree: Option<OsString>,
    git_ceilings: Option<OsString>,
    virtual_env: Option<OsString>,
    conda_prefix: Option<OsString>,
    conda_default_env: Option<OsString>,
    perlbrew_perl: Option<OsString>,
    plenv_version: Option<OsString>,
    rustup_toolchain: Option<OsString>,
    rbenv_version: Option<OsString>,
    ruby_version: Option<OsString>,
}

/// Parses one NUL-delimited request. `Ok(None)` means a clean EOF before any
/// field; every field after the magic is required, so a request that is cut
/// short is rejected rather than silently accepted with missing values.
async fn read_request<R>(reader: &mut R) -> io::Result<Option<Request>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let magic = read_field(reader).await?;
    let Some(magic) = magic else {
        return Ok(None);
    };
    if magic != REQUEST_MAGIC {
        return Err(invalid_data("client request magic is invalid"));
    }
    let version = read_field(reader).await?.ok_or_else(truncated)?;
    if version != REQUEST_VERSION {
        return Err(invalid_data("client request version is unsupported"));
    }

    let generation = read_field(reader).await?.ok_or_else(truncated)?;
    let generation = std::str::from_utf8(&generation)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_data("client request generation is invalid"))?;

    let cwd = read_field(reader).await?.ok_or_else(truncated)?;
    let cwd = PathBuf::from(OsString::from_vec(cwd));
    if !cwd.is_absolute() {
        return Err(invalid_data("client request cwd is not absolute"));
    }

    let env = RequestEnv {
        path: env_field(read_field(reader).await?)?,
        home: env_field(read_field(reader).await?)?,
        git_dir: env_field(read_field(reader).await?)?,
        git_work_tree: env_field(read_field(reader).await?)?,
        git_ceilings: env_field(read_field(reader).await?)?,
        virtual_env: env_field(read_field(reader).await?)?,
        conda_prefix: env_field(read_field(reader).await?)?,
        conda_default_env: env_field(read_field(reader).await?)?,
        perlbrew_perl: env_field(read_field(reader).await?)?,
        plenv_version: env_field(read_field(reader).await?)?,
        rustup_toolchain: env_field(read_field(reader).await?)?,
        rbenv_version: env_field(read_field(reader).await?)?,
        ruby_version: env_field(read_field(reader).await?)?,
    };

    Ok(Some(Request {
        generation,
        cwd,
        env,
    }))
}

async fn read_field<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut field = Vec::with_capacity(64);
    if reader.read_until(0, &mut field).await? == 0 {
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

fn apply_request_env(env: &RequestEnv) {
    set_env("PATH", env.path.as_deref());
    set_env("HOME", env.home.as_deref());
    set_env("GIT_DIR", env.git_dir.as_deref());
    set_env("GIT_WORK_TREE", env.git_work_tree.as_deref());
    set_env("GIT_CEILING_DIRECTORIES", env.git_ceilings.as_deref());
    set_env("VIRTUAL_ENV", env.virtual_env.as_deref());
    set_env("CONDA_PREFIX", env.conda_prefix.as_deref());
    set_env("CONDA_DEFAULT_ENV", env.conda_default_env.as_deref());
    set_env("PERLBREW_PERL", env.perlbrew_perl.as_deref());
    set_env("PLENV_VERSION", env.plenv_version.as_deref());
    set_env("RUSTUP_TOOLCHAIN", env.rustup_toolchain.as_deref());
    set_env("RBENV_VERSION", env.rbenv_version.as_deref());
    set_env("RUBY_VERSION", env.ruby_version.as_deref());
}

fn set_env(name: &str, value: Option<&OsStr>) {
    // SAFETY: this client runs on a current-thread Tokio runtime. Before
    // mutating the process environment, the previous request task is aborted
    // and awaited, so no request task can read the environment concurrently
    // or afterward.
    match value {
        Some(value) => unsafe { env::set_var(name, value) },
        None => unsafe { env::remove_var(name) },
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use tokio::io::BufReader;

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

    #[tokio::test(flavor = "current_thread")]
    async fn parses_a_complete_request_with_environment() {
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
        let request = read_request(&mut reader).await.unwrap().unwrap();

        assert_eq!(request.generation, 42);
        assert_eq!(request.cwd, Path::new("/work/project"));
        assert_eq!(
            request.env.path.as_deref(),
            Some(OsStr::new("/opt/bin:/usr/bin"))
        );
        assert_eq!(request.env.home.as_deref(), Some(OsStr::new("/home/user")));
        assert_eq!(request.env.git_dir, None);
        assert_eq!(
            request.env.git_work_tree.as_deref(),
            Some(OsStr::new("/work/tree"))
        );
        assert!(read_request(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_utf8_cwd_and_environment_round_trip() {
        let mut git_dir = b"/repo-".to_vec();
        git_dir.push(0xff);
        let fields: [&[u8]; ENV_FIELD_COUNT] = [
            b"", b"", &git_dir, b"", b"", b"", b"", b"", b"", b"", b"", b"", b"",
        ];
        let mut cwd = b"/cwd-".to_vec();
        cwd.push(0xfe);
        let bytes = request(&cwd, &fields);
        let mut reader = BufReader::new(&bytes[..]);
        let request = read_request(&mut reader).await.unwrap().unwrap();

        assert_eq!(request.cwd.as_os_str(), OsStr::from_bytes(&cwd));
        assert_eq!(
            request.env.git_dir.as_deref(),
            Some(OsStr::from_bytes(&git_dir))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_requests_are_rejected() {
        let empty: [&[u8]; ENV_FIELD_COUNT] = [b""; ENV_FIELD_COUNT];
        let valid = request(b"/work", &empty);

        // bytes: ZTREQ\0 1\0 42\0 /work\0 then 13 empty fields
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        assert!(
            read_request(&mut BufReader::new(&bad_magic[..]))
                .await
                .is_err()
        );

        let mut bad_version = valid.clone();
        bad_version[6] = b'2';
        assert!(
            read_request(&mut BufReader::new(&bad_version[..]))
                .await
                .is_err()
        );

        let mut bad_generation = valid.clone();
        bad_generation[8] = b'x';
        assert!(
            read_request(&mut BufReader::new(&bad_generation[..]))
                .await
                .is_err()
        );

        let mut relative_cwd = valid.clone();
        relative_cwd[11] = b'.';
        assert!(
            read_request(&mut BufReader::new(&relative_cwd[..]))
                .await
                .is_err()
        );

        let truncated = &valid[..valid.len() - 3];
        assert!(read_request(&mut BufReader::new(truncated)).await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clean_eof_is_a_normal_stop_but_partial_requests_are_rejected() {
        assert!(
            read_request(&mut BufReader::new(&b""[..]))
                .await
                .unwrap()
                .is_none()
        );
        let partial = b"ZTREQ\0";
        assert!(
            read_request(&mut BufReader::new(&partial[..]))
                .await
                .is_err()
        );
    }
}
