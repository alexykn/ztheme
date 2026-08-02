use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    CACHE_FORMAT_VERSION, CacheKey, Entry, MAX_ENTRIES, MAX_VALUE_BYTES, now_epoch_seconds,
    validate_value,
};

const MAGIC: [u8; 4] = *b"ZTHC";
const MAX_FILE_BYTES: u64 = 9 * 1024 * 1024;
const FUTURE_SKEW_SECONDS: u64 = 5 * 60;

pub(super) fn load(path: &Path) -> io::Result<HashMap<CacheKey, Entry>> {
    validate_private_file(path)?;
    let metadata = path.metadata()?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(invalid_data("cache file exceeds size limit"));
    }
    let capacity =
        usize::try_from(metadata.len()).map_err(|_| invalid_data("cache file is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    File::open(path)?
        .take(MAX_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return Err(invalid_data("cache file exceeds size limit"));
    }
    decode(&bytes)
}

pub(super) fn save(path: &Path, entries: &HashMap<CacheKey, Entry>) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(invalid_data("cache path has no parent"));
    };
    ensure_private_directory(parent)?;

    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = open_temporary(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        encode(&mut file, entries)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) fn clear_all(path: Option<&Path>) -> io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let Some(directory) = path.parent() else {
        return Ok(());
    };
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_dir() || metadata.mode() & 0o077 != 0 {
        return Err(invalid_data("cache directory permissions are unsafe"));
    }

    for name in [path.file_name(), temporary_path(path).file_name()]
        .into_iter()
        .flatten()
    {
        let candidate = directory.join(name);
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    sync_directory(directory)
}

fn decode(bytes: &[u8]) -> io::Result<HashMap<CacheKey, Entry>> {
    let mut decoder = Decoder::new(bytes);
    if decoder.take(MAGIC.len())? != MAGIC {
        return Err(invalid_data("cache file magic is invalid"));
    }
    if decoder.u16()? != CACHE_FORMAT_VERSION {
        return Err(invalid_data("cache file version is unsupported"));
    }
    let count = usize::try_from(decoder.u32()?)
        .map_err(|_| invalid_data("cache entry count is invalid"))?;
    if count > MAX_ENTRIES {
        return Err(invalid_data("cache entry count exceeds limit"));
    }

    let now = now_epoch_seconds();
    let latest_allowed = now.saturating_add(FUTURE_SKEW_SECONDS);
    let mut entries = HashMap::with_capacity(count);
    for _ in 0..count {
        let key = CacheKey::from_digest(decoder.array_32()?);
        let last_used_at = decoder.u64()?;
        let lru_order = decoder.u64()?;
        let value_length = usize::try_from(decoder.u32()?)
            .map_err(|_| invalid_data("cache value length is invalid"))?;
        if value_length > MAX_VALUE_BYTES {
            return Err(invalid_data("cache value exceeds size limit"));
        }
        if last_used_at > latest_allowed {
            return Err(invalid_data("cache timestamp is invalid"));
        }

        let value = decoder.take(value_length)?.to_vec();
        validate_value(&value)?;
        let entry = Entry {
            value: Arc::from(value),
            last_used_at,
            persisted_last_used_at: last_used_at,
            lru_order,
        };
        if entries.insert(key, entry).is_some() {
            return Err(invalid_data("cache file contains duplicate keys"));
        }
    }
    if !decoder.is_empty() {
        return Err(invalid_data("cache file contains trailing data"));
    }
    Ok(entries)
}

fn encode(output: &mut impl Write, entries: &HashMap<CacheKey, Entry>) -> io::Result<()> {
    output.write_all(&MAGIC)?;
    output.write_all(&CACHE_FORMAT_VERSION.to_be_bytes())?;
    output.write_all(
        &u32::try_from(entries.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    )?;
    for (key, entry) in entries {
        output.write_all(&key.bytes())?;
        output.write_all(&entry.last_used_at.to_be_bytes())?;
        output.write_all(&entry.lru_order.to_be_bytes())?;
        output.write_all(
            &u32::try_from(entry.value.len())
                .map_err(|_| invalid_data("cache value length is invalid"))?
                .to_be_bytes(),
        )?;
        output.write_all(&entry.value)?;
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(invalid_data("cache directory is not a directory"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn validate_private_file(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(invalid_data("cache path has no parent"));
    };
    let directory = fs::symlink_metadata(parent)?;
    if !directory.file_type().is_dir() || directory.mode() & 0o077 != 0 {
        return Err(invalid_data("cache directory permissions are unsafe"));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.mode() & 0o077 != 0 {
        return Err(invalid_data("cache file permissions are unsafe"));
    }
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(name)
}

fn open_temporary(path: &Path) -> io::Result<File> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            OpenOptions::new().write(true).create_new(true).open(path)
        }
        Err(error) => Err(error),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| invalid_data("cache file length overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid_data("cache file is truncated"))?;
        self.position = end;
        Ok(value)
    }

    fn array_32(&mut self) -> io::Result<[u8; 32]> {
        self.take(32)?
            .try_into()
            .map_err(|_| invalid_data("cache file is truncated"))
    }

    fn u16(&mut self) -> io::Result<u16> {
        self.take(2)?
            .try_into()
            .map(u16::from_be_bytes)
            .map_err(|_| invalid_data("cache file is truncated"))
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.take(4)?
            .try_into()
            .map(u32::from_be_bytes)
            .map_err(|_| invalid_data("cache file is truncated"))
    }

    fn u64(&mut self) -> io::Result<u64> {
        self.take(8)?
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| invalid_data("cache file is truncated"))
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        CACHE_FORMAT_VERSION, CacheKey, Entry, FUTURE_SKEW_SECONDS, MAGIC, decode, encode, load,
        now_epoch_seconds, save,
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ztheme-cache-disk-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(value: &[u8], last_used_at: u64, order: u64) -> Entry {
        Entry {
            value: Arc::from(value),
            last_used_at,
            persisted_last_used_at: last_used_at,
            lru_order: order,
        }
    }

    fn encoded(entries: &HashMap<CacheKey, Entry>) -> Vec<u8> {
        let mut output = Vec::new();
        encode(&mut output, entries).unwrap();
        output
    }

    #[test]
    fn save_load_preserves_entries_lru_and_private_permissions() {
        let directory = TestDirectory::new();
        let path = directory.path().join("nested/cache/runtime-v2.bin");
        let entries = HashMap::from([
            (CacheKey::from_value(1), entry(b"first", 100, 4)),
            (CacheKey::from_value(2), entry(b"second", 200, 9)),
        ]);

        save(&path, &entries).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.len(), 2);
        for (key, expected) in &entries {
            let actual = &loaded[key];
            assert_eq!(actual.value.as_ref(), expected.value.as_ref());
            assert_eq!(actual.last_used_at, expected.last_used_at);
            assert_eq!(
                actual.persisted_last_used_at,
                expected.persisted_last_used_at
            );
            assert_eq!(actual.lru_order, expected.lru_order);
        }
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn old_entries_are_read_without_wall_clock_expiry() {
        let entries = HashMap::from([(CacheKey::from_value(1), entry(b"old", 1, 1))]);
        let loaded = decode(&encoded(&entries)).unwrap();
        assert_eq!(&*loaded[&CacheKey::from_value(1)].value, b"old");
    }

    #[test]
    fn malformed_cache_files_are_rejected() {
        let entries = HashMap::from([(CacheKey::from_value(1), entry(b"value", 1, 1))]);
        let valid = encoded(&entries);
        for length in 0..valid.len() {
            assert!(
                decode(&valid[..length]).is_err(),
                "accepted length {length}"
            );
        }
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let mut bad_magic = valid.clone();
        bad_magic[0] ^= 1;
        assert!(decode(&bad_magic).is_err());
        let mut bad_version = valid.clone();
        bad_version[MAGIC.len()..MAGIC.len() + 2]
            .copy_from_slice(&CACHE_FORMAT_VERSION.saturating_add(1).to_be_bytes());
        assert!(decode(&bad_version).is_err());
    }

    #[test]
    fn duplicate_cache_keys_are_rejected() {
        let entries = HashMap::from([(CacheKey::from_value(1), entry(b"value", 1, 1))]);
        let valid = encoded(&entries);
        let mut duplicate = valid.clone();
        duplicate[6..10].copy_from_slice(&2_u32.to_be_bytes());
        duplicate.extend_from_slice(&valid[10..]);
        assert!(decode(&duplicate).is_err());
    }

    #[test]
    fn future_timestamps_are_rejected() {
        let entries = HashMap::from([(CacheKey::from_value(1), entry(b"value", 1, 1))]);
        let mut future = encoded(&entries);
        let timestamp = now_epoch_seconds().saturating_add(FUTURE_SKEW_SECONDS + 1);
        future[42..50].copy_from_slice(&timestamp.to_be_bytes());
        assert!(decode(&future).is_err());
    }
}
