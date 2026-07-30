use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::{CacheKey, MAX_VALUE_BYTES};
use crate::gitstatus::{Query, Snapshot};

pub const VERSION: u16 = 1;
pub const GET: u8 = 1;
pub const PUT: u8 = 2;
pub const CLEAR: u8 = 4;
pub const GIT: u8 = 5;
pub const MISS: u8 = 0;
pub const HIT: u8 = 1;
pub const OK: u8 = 2;

const MAGIC: [u8; 2] = *b"ZT";
const DAEMON_OUTDATED: u8 = 0xfe;
const CLIENT_OUTDATED: u8 = 0xff;
const EXCHANGE_TIMEOUT: Duration = Duration::from_millis(25);
const GIT_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_GIT_TEXT_BYTES: usize = 16 * 1024;
const MAX_ERROR_BYTES: usize = 1024;

const GIT_NONE: u8 = 0;
const GIT_SNAPSHOT: u8 = 1;
const GIT_ERROR: u8 = 2;
const QUERY_DIRECTORY: u8 = 0;
const QUERY_GIT_DIR: u8 = 1;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    DaemonOutdated,
    ClientOutdated,
}

pub enum RequestHeader {
    Operation(u8),
    DaemonOutdated,
    ClientOutdated,
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub async fn get(socket: &Path, key: CacheKey) -> Result<Option<Vec<u8>>> {
    exchange(socket, async |stream| {
        write_request(stream, GET, Some(key)).await?;
        match read_response(stream).await? {
            MISS => Ok(None),
            HIT => {
                let length = usize::try_from(stream.read_u32().await?)
                    .map_err(|_| invalid_data("invalid cache value length"))?;
                if length > MAX_VALUE_BYTES {
                    return Err(invalid_data("cache value exceeds size limit").into());
                }
                let mut bytes = vec![0; length];
                stream.read_exact(&mut bytes).await?;
                super::validate_value(&bytes)?;
                Ok(Some(bytes))
            }
            _ => Err(invalid_data("invalid cache response").into()),
        }
    })
    .await
}

pub async fn put(socket: &Path, key: CacheKey, value: &[u8]) -> Result<()> {
    exchange(socket, async |stream| {
        write_request(stream, PUT, Some(key)).await?;
        stream.write_u32(u32_len(value)?).await?;
        stream.write_all(value).await?;
        expect_ok(stream).await
    })
    .await
}

pub async fn clear(socket: &Path) -> Result<()> {
    exchange(socket, async |stream| {
        write_request(stream, CLEAR, None).await?;
        expect_ok(stream).await
    })
    .await
}

pub async fn git(socket: &Path, query: &Query) -> Result<Option<Snapshot>> {
    exchange_with_timeout(socket, GIT_TIMEOUT, async |stream| {
        write_request(stream, GIT, None).await?;
        write_query(stream, query).await?;
        match read_response(stream).await? {
            GIT_NONE => Ok(None),
            GIT_SNAPSHOT => Ok(Some(read_snapshot(stream).await?)),
            GIT_ERROR => {
                let message = read_text(stream, MAX_ERROR_BYTES, "Git error").await?;
                Err(io::Error::other(message).into())
            }
            _ => Err(invalid_data("invalid Git response").into()),
        }
    })
    .await
}

pub async fn read_header(stream: &mut UnixStream) -> io::Result<RequestHeader> {
    let mut magic = [0; MAGIC.len()];
    stream.read_exact(&mut magic).await?;
    if magic != MAGIC {
        return Err(invalid_data("invalid cache protocol"));
    }
    match stream.read_u16().await?.cmp(&VERSION) {
        std::cmp::Ordering::Less => Ok(RequestHeader::ClientOutdated),
        std::cmp::Ordering::Equal => stream.read_u8().await.map(RequestHeader::Operation),
        std::cmp::Ordering::Greater => Ok(RequestHeader::DaemonOutdated),
    }
}

pub async fn read_query(stream: &mut UnixStream) -> io::Result<Query> {
    let kind = stream.read_u8().await?;
    let path = read_bytes(stream, MAX_PATH_BYTES, "Git query path").await?;
    let path = std::path::PathBuf::from(OsString::from_vec(path));
    match kind {
        QUERY_DIRECTORY => Ok(Query::Directory(path)),
        QUERY_GIT_DIR => Ok(Query::GitDir(path)),
        _ => Err(invalid_data("invalid Git query kind")),
    }
}

pub async fn read_key(stream: &mut UnixStream) -> io::Result<CacheKey> {
    Ok(CacheKey::from_value(stream.read_u64().await?))
}

pub async fn read_value(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let length = usize::try_from(stream.read_u32().await?)
        .map_err(|_| invalid_data("invalid cache value length"))?;
    if length > MAX_VALUE_BYTES {
        return Err(invalid_data("cache value exceeds size limit"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    super::validate_value(&bytes)?;
    Ok(bytes)
}

pub async fn write_miss(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_u8(MISS).await
}

pub async fn write_hit(stream: &mut UnixStream, value: &[u8]) -> io::Result<()> {
    stream.write_u8(HIT).await?;
    stream.write_u32(u32_len(value)?).await?;
    stream.write_all(value).await
}

pub async fn write_ok(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_u8(OK).await
}

pub async fn write_daemon_outdated(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_u8(DAEMON_OUTDATED).await
}

pub async fn write_client_outdated(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_u8(CLIENT_OUTDATED).await
}

pub async fn write_git_result(
    stream: &mut UnixStream,
    result: io::Result<Option<Snapshot>>,
) -> io::Result<()> {
    match result {
        Ok(None) => stream.write_u8(GIT_NONE).await,
        Ok(Some(snapshot)) => {
            stream.write_u8(GIT_SNAPSHOT).await?;
            write_snapshot(stream, &snapshot).await
        }
        Err(error) => {
            stream.write_u8(GIT_ERROR).await?;
            write_text(stream, &error.to_string(), MAX_ERROR_BYTES).await
        }
    }
}

async fn exchange<T, F>(socket: &Path, operation: F) -> Result<T>
where
    F: AsyncFnOnce(&mut UnixStream) -> Result<T>,
{
    exchange_with_timeout(socket, EXCHANGE_TIMEOUT, operation).await
}

async fn exchange_with_timeout<T, F>(socket: &Path, duration: Duration, operation: F) -> Result<T>
where
    F: AsyncFnOnce(&mut UnixStream) -> Result<T>,
{
    timeout(duration, async {
        let mut stream = UnixStream::connect(socket).await?;
        operation(&mut stream).await
    })
    .await
    .map_err(|_| {
        Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "daemon exchange timed out",
        ))
    })?
}

async fn write_request(
    stream: &mut UnixStream,
    operation: u8,
    key: Option<CacheKey>,
) -> io::Result<()> {
    stream.write_all(&MAGIC).await?;
    stream.write_u16(VERSION).await?;
    stream.write_u8(operation).await?;
    if let Some(key) = key {
        stream.write_u64(key.value()).await?;
    }
    Ok(())
}

async fn expect_ok(stream: &mut UnixStream) -> Result<()> {
    if read_response(stream).await? == OK {
        return Ok(());
    }
    Err(invalid_data("cache operation failed").into())
}

async fn read_response(stream: &mut UnixStream) -> Result<u8> {
    match stream.read_u8().await? {
        DAEMON_OUTDATED => Err(Error::DaemonOutdated),
        CLIENT_OUTDATED => Err(Error::ClientOutdated),
        response => Ok(response),
    }
}

async fn write_query(stream: &mut UnixStream, query: &Query) -> io::Result<()> {
    stream
        .write_u8(if query.is_git_dir() {
            QUERY_GIT_DIR
        } else {
            QUERY_DIRECTORY
        })
        .await?;
    write_bytes(
        stream,
        query.path().as_os_str().as_bytes(),
        MAX_PATH_BYTES,
        "Git query path",
    )
    .await
}

async fn write_snapshot(stream: &mut UnixStream, snapshot: &Snapshot) -> io::Result<()> {
    write_bytes(
        stream,
        snapshot.worktree.as_os_str().as_bytes(),
        MAX_PATH_BYTES,
        "Git worktree",
    )
    .await?;
    write_text(stream, &snapshot.oid, MAX_GIT_TEXT_BYTES).await?;
    write_text(stream, &snapshot.branch, MAX_GIT_TEXT_BYTES).await?;
    write_text(stream, &snapshot.action, MAX_GIT_TEXT_BYTES).await?;
    stream.write_u64(snapshot.ahead).await?;
    stream.write_u64(snapshot.behind).await?;
    stream.write_u64(snapshot.stashes).await?;
    stream.write_u8(snapshot.changes).await
}

async fn read_snapshot(stream: &mut UnixStream) -> io::Result<Snapshot> {
    let worktree = read_bytes(stream, MAX_PATH_BYTES, "Git worktree").await?;
    Ok(Snapshot {
        worktree: OsString::from_vec(worktree).into(),
        oid: read_text(stream, MAX_GIT_TEXT_BYTES, "Git commit id").await?,
        branch: read_text(stream, MAX_GIT_TEXT_BYTES, "Git branch").await?,
        action: read_text(stream, MAX_GIT_TEXT_BYTES, "Git action").await?,
        ahead: stream.read_u64().await?,
        behind: stream.read_u64().await?,
        stashes: stream.read_u64().await?,
        changes: stream.read_u8().await?,
    })
}

async fn write_text(stream: &mut UnixStream, value: &str, maximum: usize) -> io::Result<()> {
    write_bytes(stream, value.as_bytes(), maximum, "text field").await
}

async fn write_bytes(
    stream: &mut UnixStream,
    value: &[u8],
    maximum: usize,
    name: &'static str,
) -> io::Result<()> {
    if value.len() > maximum {
        return Err(invalid_data(name));
    }
    stream.write_u32(u32_len(value)?).await?;
    stream.write_all(value).await
}

async fn read_text(
    stream: &mut UnixStream,
    maximum: usize,
    name: &'static str,
) -> io::Result<String> {
    let bytes = read_bytes(stream, maximum, name).await?;
    String::from_utf8(bytes).map_err(|_| invalid_data(name))
}

async fn read_bytes(
    stream: &mut UnixStream,
    maximum: usize,
    name: &'static str,
) -> io::Result<Vec<u8>> {
    let length = usize::try_from(stream.read_u32().await?).map_err(|_| invalid_data(name))?;
    if length > maximum {
        return Err(invalid_data(name));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

fn u32_len(value: &[u8]) -> io::Result<u32> {
    u32::try_from(value.len()).map_err(|_| invalid_data("cache value length is invalid"))
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
