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
        read_bounded_response(&mut self.stdout).await
    }
}

/// Reads one gitstatusd response without ever growing past roughly
/// `MAX_RESPONSE_BYTES + 1` bytes. The previous `read_until` could buffer
/// without bound when a broken child never emitted the message separator,
/// leaving the daemon's 30-second safety timeout as the only cap. Reading in
/// bounded chunks keeps a runaway child from consuming unbounded memory while
/// still distinguishing the four outcomes: a clean EOF with no bytes, an
/// oversized response, a response within the limit that lacks the separator,
/// and a valid terminated response.
async fn read_bounded_response<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let limit = MAX_RESPONSE_BYTES + 1;
    let mut response = Vec::with_capacity(256);
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if response.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "gitstatusd closed its output",
                ));
            }
            return Err(invalid_data("unterminated gitstatusd response"));
        }

        if let Some(separator) = buffer.iter().position(|byte| *byte == MESSAGE_SEPARATOR)
            && response.len() + separator < MAX_RESPONSE_BYTES
        {
            response.extend_from_slice(&buffer[..separator]);
            reader.consume(separator + 1);
            return Ok(response);
        }

        let take = buffer.len().min(limit.saturating_sub(response.len()));
        response.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if response.len() >= limit {
            return Err(invalid_data("gitstatusd response is too large"));
        }
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

#[cfg(test)]
mod tests {
    use super::{MAX_RESPONSE_BYTES, parse_response, read_bounded_response};
    use crate::gitstatus::{CONFLICTED, DELETED, STAGED, UNSTAGED, UNTRACKED};

    fn response(request_id: u64, fields: &[String]) -> Vec<u8> {
        let mut values = vec![request_id.to_string(), "1".to_owned()];
        values.extend_from_slice(fields);
        values.join("\x1f").into_bytes()
    }

    fn fields() -> Vec<String> {
        [
            "/repo",
            "0123456789abcdef",
            "main",
            "",
            "",
            "",
            "",
            "",
            "0",
            "0",
            "0",
            "0",
            "0",
            "0",
            "0",
            "0",
            "0",
            "0",
            "0",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn response_reading_is_bounded_and_distinguishes_outcomes() {
        use std::io;
        use tokio::io::BufReader;

        let empty = read_bounded_response(&mut BufReader::new(&b""[..]))
            .await
            .unwrap_err();
        assert_eq!(empty.kind(), io::ErrorKind::UnexpectedEof);

        let unterminated = read_bounded_response(&mut BufReader::new(&b"abc"[..]))
            .await
            .unwrap_err();
        assert_eq!(unterminated.kind(), io::ErrorKind::InvalidData);
        assert_eq!(unterminated.to_string(), "unterminated gitstatusd response");

        let valid = read_bounded_response(&mut BufReader::new(&b"abc\x1e"[..]))
            .await
            .unwrap();
        assert_eq!(valid, b"abc");

        let mut at_limit = vec![b'x'; MAX_RESPONSE_BYTES - 1];
        at_limit.push(0x1e);
        let at_limit_content = read_bounded_response(&mut BufReader::new(&at_limit[..]))
            .await
            .unwrap();
        assert_eq!(at_limit_content.len(), MAX_RESPONSE_BYTES - 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_and_unterminated_responses_never_grow_unbounded() {
        use std::io;
        use tokio::io::BufReader;

        // Content exactly at the limit with the separator after it: the total
        // response exceeds the cap, so it must be rejected as too large.
        let mut oversized = vec![b'x'; MAX_RESPONSE_BYTES];
        oversized.push(0x1e);
        let error = read_bounded_response(&mut BufReader::new(&oversized[..]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "gitstatusd response is too large");

        // A response larger than the cap without a separator must also be
        // rejected, and the reader must stop consuming near the cap instead of
        // buffering the whole runaway stream.
        let mut runaway = vec![b'y'; MAX_RESPONSE_BYTES + 10_000];
        runaway.push(0x1e);
        let error = read_bounded_response(&mut BufReader::new(&runaway[..]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        // Exactly the cap of content without a separator is unterminated, not
        // oversized, matching the pre-fix boundary.
        let at_cap = vec![b'z'; MAX_RESPONSE_BYTES];
        let unterminated = read_bounded_response(&mut BufReader::new(&at_cap[..]))
            .await
            .unwrap_err();
        assert_eq!(unterminated.kind(), io::ErrorKind::InvalidData);
        assert_eq!(unterminated.to_string(), "unterminated gitstatusd response");
    }

    #[test]
    fn parses_non_repository_and_repository_metadata() {
        assert!(parse_response(9, b"9\x1f0").unwrap().is_none());

        let mut values = fields();
        values[6] = "merge".to_owned();
        values[12] = "3".to_owned();
        values[13] = "2".to_owned();
        values[14] = "4".to_owned();
        let snapshot = parse_response(9, &response(9, &values)).unwrap().unwrap();
        assert_eq!(snapshot.worktree.to_string_lossy(), "/repo");
        assert_eq!(snapshot.oid, "0123456789abcdef");
        assert_eq!(snapshot.branch, "main");
        assert_eq!(snapshot.action, "merge");
        assert_eq!(snapshot.ahead, 3);
        assert_eq!(snapshot.behind, 2);
        assert_eq!(snapshot.stashes, 4);
    }

    #[test]
    fn maps_change_counts_without_double_counting_deletions() {
        let cases = [
            (1, 0, 0, 0, STAGED),
            (0, 1, 0, 0, UNSTAGED),
            (1, 0, 0, 1, DELETED),
            (0, 1, 1, 0, DELETED),
            (2, 0, 0, 1, STAGED | DELETED),
            (0, 2, 1, 0, UNSTAGED | DELETED),
        ];

        for (staged, unstaged, unstaged_deleted, staged_deleted, expected) in cases {
            let mut values = fields();
            values[8] = staged.to_string();
            values[9] = unstaged.to_string();
            values[16] = unstaged_deleted.to_string();
            values[18] = staged_deleted.to_string();
            let snapshot = parse_response(1, &response(1, &values)).unwrap().unwrap();
            assert_eq!(snapshot.changes, expected);
        }
    }

    #[test]
    fn maps_conflicted_and_untracked_counts() {
        let mut values = fields();
        values[10] = "1".to_owned();
        values[11] = "2".to_owned();
        let snapshot = parse_response(1, &response(1, &values)).unwrap().unwrap();
        assert_eq!(snapshot.changes, CONFLICTED | UNTRACKED);
    }

    #[test]
    fn rejects_malformed_responses() {
        assert!(parse_response(2, b"1\x1f0").is_err());
        assert!(parse_response(1, b"1\x1f2").is_err());
        assert!(parse_response(1, b"1\x1f1\x1f/repo").is_err());

        let mut invalid_count = fields();
        invalid_count[8] = "-1".to_owned();
        assert!(parse_response(1, &response(1, &invalid_count)).is_err());

        let mut invalid_utf8 = response(1, &fields());
        let branch = invalid_utf8
            .windows(4)
            .position(|window| window == b"main")
            .unwrap();
        invalid_utf8[branch] = 0xff;
        assert!(parse_response(1, &invalid_utf8).is_err());
    }
}
