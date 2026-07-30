mod disk;

use std::collections::HashMap;
use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, Notify};
use tokio::time::sleep;

use crate::utils::HashBuilder;

const CACHE_FILE_PREFIX: &str = "runtime-v1-";
const CACHE_FILE_SUFFIX: &str = ".bin";
const CACHE_FORMAT_VERSION: u16 = 1;
const CACHE_IDENTITY_VERSION: u64 = 2;
const MAX_ENTRIES: usize = 500;
pub(crate) const MAX_VALUE_BYTES: usize = 16 * 1024;
const SAFETY_EXPIRY: Duration = Duration::from_hours(24);
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
const SAVE_RETRY: Duration = Duration::from_secs(30);
const LAST_USED_SAVE_INTERVAL: u64 = 5 * 60;
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CacheKey(u64);

impl CacheKey {
    pub(crate) const fn from_value(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug)]
struct Entry {
    value: Arc<[u8]>,
    refreshed_at: u64,
    last_used_at: u64,
    persisted_last_used_at: u64,
    lru_order: u64,
}

impl Entry {
    fn new(value: Vec<u8>, now: u64, lru_order: u64) -> Self {
        Self {
            value: Arc::from(value),
            refreshed_at: now,
            last_used_at: now,
            persisted_last_used_at: 0,
            lru_order,
        }
    }

    fn is_fresh(&self, now: u64) -> bool {
        now.checked_sub(self.refreshed_at)
            .is_some_and(|age| age <= SAFETY_EXPIRY.as_secs())
    }
}

pub(crate) struct RuntimeCache {
    state: Mutex<State>,
    disk_io: Mutex<()>,
    changed: Notify,
    path: Option<PathBuf>,
}

#[derive(Default)]
struct State {
    entries: HashMap<CacheKey, Entry>,
    revision: u64,
    saved_revision: u64,
    load_epoch: u64,
    lru_order: u64,
}

impl RuntimeCache {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            disk_io: Mutex::new(()),
            changed: Notify::new(),
            path: cache_path(),
        }
    }

    pub(crate) async fn load(self: Arc<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let load_epoch = self.state.lock().await.load_epoch;
        let _disk = self.disk_io.lock().await;
        let loaded = tokio::task::spawn_blocking(move || disk::load(&path)).await;
        let loaded = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => return,
            Ok(Err(error)) => {
                eprintln!("ztheme: persistent cache load failed: {error}");
                return;
            }
            Err(error) => {
                eprintln!("ztheme: persistent cache task failed: {error}");
                return;
            }
        };

        let mut state = self.state.lock().await;
        if state.load_epoch != load_epoch {
            return;
        }
        let mut entries: Vec<_> = loaded.entries.into_iter().collect();
        entries.sort_unstable_by_key(|(_, entry)| (entry.lru_order, entry.last_used_at));
        let shift = u64::try_from(entries.len()).unwrap_or(u64::MAX);
        for entry in state.entries.values_mut() {
            entry.lru_order = entry.lru_order.saturating_add(shift);
        }
        state.lru_order = state.lru_order.saturating_add(shift);
        for (index, (key, mut entry)) in entries.into_iter().enumerate() {
            entry.lru_order = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            state.entries.entry(key).or_insert(entry);
        }
        trim_lru(&mut state.entries);
        if loaded.needs_rewrite {
            state.revision = state.revision.wrapping_add(1);
            self.changed.notify_one();
        }
    }

    pub(crate) async fn get(&self, key: CacheKey) -> Option<Arc<[u8]>> {
        let now = now_epoch_seconds();
        let mut state = self.state.lock().await;
        state.lru_order = state.lru_order.saturating_add(1);
        let lru_order = state.lru_order;
        let entry = state.entries.get_mut(&key)?;

        if !entry.is_fresh(now) {
            state.entries.remove(&key);
            state.revision = state.revision.wrapping_add(1);
            self.changed.notify_one();
            return None;
        }

        entry.last_used_at = now;
        entry.lru_order = lru_order;
        let should_persist_use =
            now.saturating_sub(entry.persisted_last_used_at) >= LAST_USED_SAVE_INTERVAL;
        let value = entry.value.clone();
        if should_persist_use {
            state.revision = state.revision.wrapping_add(1);
            self.changed.notify_one();
        }
        Some(value)
    }

    pub(crate) async fn put(&self, key: CacheKey, value: Vec<u8>) -> io::Result<()> {
        validate_value(&value)?;
        let now = now_epoch_seconds();
        let mut state = self.state.lock().await;
        state.lru_order = state.lru_order.saturating_add(1);
        let lru_order = state.lru_order;
        state.entries.insert(key, Entry::new(value, now, lru_order));
        trim_lru(&mut state.entries);
        state.revision = state.revision.wrapping_add(1);
        self.changed.notify_one();
        Ok(())
    }

    pub(crate) async fn clear(&self) -> io::Result<()> {
        {
            let mut state = self.state.lock().await;
            state.entries.clear();
            state.load_epoch = state.load_epoch.wrapping_add(1);
            state.revision = state.revision.wrapping_add(1);
        }

        let path = self.path.clone();
        let _disk = self.disk_io.lock().await;
        let result = tokio::task::spawn_blocking(move || disk::clear_all(path.as_deref()))
            .await
            .map_err(io::Error::other)?;
        if result.is_ok() {
            let mut state = self.state.lock().await;
            state.saved_revision = state.revision;
        } else {
            self.changed.notify_one();
        }
        result
    }

    pub(crate) async fn flush_loop(self: Arc<Self>) {
        loop {
            self.changed.notified().await;
            sleep(SAVE_DEBOUNCE).await;

            loop {
                match self.flush_latest().await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        eprintln!("ztheme: persistent cache save failed: {error}");
                        sleep(SAVE_RETRY).await;
                    }
                }
            }
        }
    }

    pub(crate) async fn flush_latest(&self) -> io::Result<bool> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let (revision, entries) = {
            let state = self.state.lock().await;
            if state.revision == state.saved_revision {
                return Ok(false);
            }
            (state.revision, state.entries.clone())
        };

        let _disk = self.disk_io.lock().await;
        if self.state.lock().await.revision != revision {
            return Ok(true);
        }
        tokio::task::spawn_blocking(move || disk::save(&path, &entries))
            .await
            .map_err(io::Error::other)??;

        let mut state = self.state.lock().await;
        state.saved_revision = revision;
        for entry in state.entries.values_mut() {
            entry.persisted_last_used_at = entry.last_used_at;
        }
        Ok(state.revision != revision)
    }
}

fn trim_lru(entries: &mut HashMap<CacheKey, Entry>) {
    while entries.len() > MAX_ENTRIES {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.lru_order)
            .map(|(key, _)| *key)
        else {
            return;
        };
        entries.remove(&oldest);
    }
}

fn cache_path() -> Option<PathBuf> {
    cache_root().map(|root| {
        root.join("ztheme").join(format!(
            "{CACHE_FILE_PREFIX}{}{CACHE_FILE_SUFFIX}",
            cache_identity()
        ))
    })
}

fn cache_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_CACHE_HOME") {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            return Some(root);
        }
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".cache"))
}

fn cache_identity() -> String {
    let mut hash = HashBuilder::new(b"ztheme-runtime-cache-identity-v2");
    hash.add_u64(b"cache-identity-version", CACHE_IDENTITY_VERSION);
    hash.add_u64(b"cache-format-version", u64::from(CACHE_FORMAT_VERSION));
    hash.add_bytes(b"package-version", env!("CARGO_PKG_VERSION").as_bytes());
    if let Ok(executable) = env::current_exe() {
        hash.add_path(b"executable", &executable);
    }
    format!("{:016x}", hash.finish())
}

pub(crate) fn validate_value(value: &[u8]) -> io::Result<()> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache value exceeds size limit",
        ));
    }
    Ok(())
}

fn now_epoch_seconds() -> u64 {
    epoch_duration(SystemTime::now()).map_or(0, |duration| duration.as_secs())
}

fn epoch_duration(time: SystemTime) -> Option<Duration> {
    time.duration_since(UNIX_EPOCH).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{CacheKey, Entry, MAX_VALUE_BYTES, RuntimeCache, SAFETY_EXPIRY, trim_lru};

    #[test]
    fn entry_freshness_has_an_exact_safety_boundary() {
        let entry = Entry::new(Vec::new(), 100, 1);
        assert!(entry.is_fresh(100 + SAFETY_EXPIRY.as_secs()));
        assert!(!entry.is_fresh(101 + SAFETY_EXPIRY.as_secs()));
        assert!(!entry.is_fresh(99));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_cache_inserts_retrieves_and_rejects_oversized_values() {
        let cache = RuntimeCache::new();
        let key = CacheKey::from_value(7);

        cache.put(key, b"value".to_vec()).await.unwrap();
        assert_eq!(cache.get(key).await.as_deref(), Some(b"value".as_slice()));
        assert!(cache.put(key, vec![0; MAX_VALUE_BYTES + 1]).await.is_err());
    }

    #[test]
    fn lru_trimming_keeps_the_newest_entries() {
        let mut entries = HashMap::new();
        for order in 0..=500 {
            entries.insert(
                CacheKey::from_value(order),
                Entry::new(Vec::new(), 1, order),
            );
        }

        trim_lru(&mut entries);
        assert_eq!(entries.len(), 500);
        assert!(!entries.contains_key(&CacheKey::from_value(0)));
        assert!(entries.contains_key(&CacheKey::from_value(500)));
    }
}
