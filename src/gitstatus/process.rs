use std::io;
use std::os::unix::ffi::OsStrExt;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::{CONFLICTED, DELETED, Query, STAGED, Snapshot, UNSTAGED, UNTRACKED, managed_binary};

const FIELD_SEPARATOR: u8 = 31;
const MESSAGE_SEPARATOR: u8 = 30;
const VERSION_GLOB: &str = "v1.5.*";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub struct Client {
    process: Option<Process>,
    next_request_id: u64,
}

struct Process {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    pub fn start() -> io::Result<Self> {
        Ok(Self {
            process: Some(Process::start()?),
            next_request_id: 0,
        })
    }

    pub async fn query(&mut self, query: &Query) -> io::Result<Option<Snapshot>> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);

        match self.query_once(request_id, query).await {
            Ok(snapshot) => Ok(snapshot),
            Err(first_error) => {
                self.process = None;
                self.process = Some(Process::start().map_err(|restart_error| {
                    io::Error::other(format!(
                        "gitstatusd failed ({first_error}) and could not restart: {restart_error}"
                    ))
                })?);
                self.query_once(request_id, query).await.map_err(|error| {
                    io::Error::other(format!("gitstatusd failed after restart: {error}"))
                })
            }
        }
    }

    pub fn restart(&mut self) -> io::Result<()> {
        self.process = Some(Process::start()?);
        Ok(())
    }

    async fn query_once(&mut self, request_id: u64, query: &Query) -> io::Result<Option<Snapshot>> {
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| io::Error::other("gitstatusd is not running"))?;
        process.write_request(request_id, query).await?;
        let response = process.read_response().await?;
        parse_response(request_id, &response)
    }
}

impl Process {
    fn start() -> io::Result<Self> {
        let thread_count = std::thread::available_parallelism().map_or(1, usize::from);
        let mut command = Command::new(managed_binary());
        command
            .arg("-G")
            .arg(VERSION_GLOB)
            .arg("-v")
            .arg("ERROR")
            .arg("-r")
            .arg("3600")
            .arg("-s")
            .arg("-1")
            .arg("-u")
            .arg("-1")
            .arg("-c")
            .arg("1")
            .arg("-d")
            .arg("1")
            .arg("-z")
            .arg("0")
            .arg("-t")
            .arg(thread_count.to_string())
            .arg("-p")
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_CEILING_DIRECTORIES",
        ] {
            command.env_remove(variable);
        }
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("gitstatusd stdin is unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .map(BufReader::new)
            .ok_or_else(|| io::Error::other("gitstatusd stdout is unavailable"))?;
        Ok(Self {
            _child: child,
            stdin,
            stdout,
        })
    }

    async fn write_request(&mut self, request_id: u64, query: &Query) -> io::Result<()> {
        let path = query.path().as_os_str().as_bytes();
        if path.contains(&FIELD_SEPARATOR) || path.contains(&MESSAGE_SEPARATOR) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repository path contains a gitstatusd protocol separator",
            ));
        }

        self.stdin
            .write_all(request_id.to_string().as_bytes())
            .await?;
        self.stdin.write_u8(FIELD_SEPARATOR).await?;
        if query.is_git_dir() {
            self.stdin.write_u8(b':').await?;
        }
        self.stdin.write_all(path).await?;
        self.stdin.write_u8(MESSAGE_SEPARATOR).await?;
        self.stdin.flush().await
    }

    async fn read_response(&mut self) -> io::Result<Vec<u8>> {
        let mut response = Vec::with_capacity(256);
        let read = self
            .stdout
            .read_until(MESSAGE_SEPARATOR, &mut response)
            .await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "gitstatusd closed its output",
            ));
        }
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(invalid_data("gitstatusd response is too large"));
        }
        if response.pop() != Some(MESSAGE_SEPARATOR) {
            return Err(invalid_data("unterminated gitstatusd response"));
        }
        Ok(response)
    }
}

fn parse_response(request_id: u64, response: &[u8]) -> io::Result<Option<Snapshot>> {
    let mut fields = response.split(|byte| *byte == FIELD_SEPARATOR);
    let response_id = fields
        .next()
        .ok_or_else(|| invalid_data("missing gitstatusd response id"))?;
    if parse_u64(response_id, "request id")? != request_id {
        return Err(invalid_data(
            "gitstatusd response id does not match request",
        ));
    }
    match fields
        .next()
        .ok_or_else(|| invalid_data("missing gitstatusd repository flag"))?
    {
        b"0" => return Ok(None),
        b"1" => {}
        _ => return Err(invalid_data("invalid gitstatusd repository flag")),
    }

    let mut repository = [&[][..]; 19];
    for field in &mut repository {
        *field = fields
            .next()
            .ok_or_else(|| invalid_data("incomplete gitstatusd repository response"))?;
    }

    let staged = parse_u64(repository[8], "staged count")?;
    let unstaged = parse_u64(repository[9], "unstaged count")?;
    let unstaged_deleted = parse_u64(repository[16], "unstaged deleted count")?;
    let staged_deleted = parse_u64(repository[18], "staged deleted count")?;
    let mut changes = 0;
    if staged > staged_deleted {
        changes |= STAGED;
    }
    if unstaged > unstaged_deleted {
        changes |= UNSTAGED;
    }
    if nonzero(repository[10], "conflicted count")? {
        changes |= CONFLICTED;
    }
    if nonzero(repository[11], "untracked count")? {
        changes |= UNTRACKED;
    }
    if unstaged_deleted != 0 || staged_deleted != 0 {
        changes |= DELETED;
    }

    Ok(Some(Snapshot {
        worktree: path_from_bytes(repository[0]),
        oid: text(repository[1], "commit id")?,
        branch: text(repository[2], "branch")?,
        action: text(repository[6], "repository state")?,
        ahead: parse_u64(repository[12], "ahead count")?,
        behind: parse_u64(repository[13], "behind count")?,
        stashes: parse_u64(repository[14], "stash count")?,
        changes,
    }))
}

fn nonzero(value: &[u8], name: &'static str) -> io::Result<bool> {
    Ok(parse_u64(value, name)? != 0)
}

fn parse_u64(value: &[u8], name: &'static str) -> io::Result<u64> {
    let value =
        std::str::from_utf8(value).map_err(|_| invalid_data_with_field(name, "is not ASCII"))?;
    value
        .parse()
        .map_err(|_| invalid_data_with_field(name, "is not an unsigned integer"))
}

fn text(value: &[u8], name: &'static str) -> io::Result<String> {
    String::from_utf8(value.to_vec())
        .map_err(|_| invalid_data_with_field(name, "is not valid UTF-8"))
}

fn path_from_bytes(value: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt as _;

    std::ffi::OsString::from_vec(value.to_vec()).into()
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_data_with_field(field: &'static str, message: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("gitstatusd {field} {message}"),
    )
}
