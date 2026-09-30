use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Reject known special files before opening, then check the opened descriptor
/// rather than trusting the path. `O_NONBLOCK` prevents a replacement FIFO from
/// blocking open; `O_NOCTTY` avoids acquiring a replacement device as a terminal.
/// Symlinks to regular files remain supported. Regular files and metadata
/// operations can still stall (e.g. remote filesystems), so callers run off the
/// event loop with bounded worker ownership.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<File> {
    fn require_regular(metadata: &fs::Metadata) -> io::Result<()> {
        if metadata.is_file() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "path is not a regular file",
            ))
        }
    }
    require_regular(&fs::metadata(path)?)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    require_regular(&file.metadata()?)?;
    Ok(file)
}
