mod disk;

use std::collections::HashMap;
use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, Notify};
use tokio::time::{Instant, sleep};

const CACHE_FILE_NAME: &str = "runtime-v2.bin";
pub(crate) const CACHE_FORMAT_VERSION: u16 = 2;
const MAX_ENTRIES: usize = 500;
pub(crate) const MAX_VALUE_BYTES: usize = 16 * 1024;
const LEASE_DURATION: Duration = Duration::from_millis(400);
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
const SAVE_RETRY: Duration = Duration::from_secs(30);
const LAST_USED_SAVE_INTERVAL: u64 = 5 * 60;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CacheKey([u8; 32]);

impl CacheKey {
    pub(crate) fn from_digest(value: [u8; 32]) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(crate) fn from_value(value: u64) -> Self {
        let mut bytes = [0; 32];
        bytes[24..].copy_from_slice(&value.to_be_bytes());
        Self(bytes)
    }

    pub(crate) const fn bytes(self) -> [u8; 32] {
        self.0
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    value: Arc<[u8]>,
    last_used_at: u64,
    persisted_last_used_at: u64,
    lru_order: u64,
}

impl Entry {
    fn new(value: Vec<u8>, now: u64, lru_order: u64) -> Self {
        Self {
            value: Arc::from(value),
            last_used_at: now,
            persisted_last_used_at: 0,
            lru_order,
        }
    }
}

struct Lease {
    token: u64,
    expires_at: Instant,
    notify: Arc<Notify>,
}

pub(crate) enum Acquire {
    Hit(Arc<[u8]>),
    Owner(u64),
}

pub(crate) struct RuntimeCache {
    pub(crate) state: Mutex<State>,
    pub(crate) disk_io: Mutex<()>,
    changed: Notify,
    path: Option<PathBuf>,
}

#[derive(Default)]
pub(crate) struct State {
    entries: HashMap<CacheKey, Entry>,
    in_flight: HashMap<CacheKey, Lease>,
    next_token: u64,
    revision: u64,
    saved_revision: u64,
    load_epoch: u64,
    lru_order: u64,
}

impl RuntimeCache {
    pub(crate) fn new() -> Self {
        Self::new_with_path(cache_path())
    }

    pub(crate) fn new_with_path(path: Option<PathBuf>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            disk_io: Mutex::new(()),
            changed: Notify::new(),
            path,
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
        let mut entries: Vec<_> = loaded.into_iter().collect();
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
    }

    #[cfg(test)]
    pub(crate) async fn get(&self, key: CacheKey) -> Option<Arc<[u8]>> {
        let now = now_epoch_seconds();
        let (value, persist_use) = {
            let mut state = self.state.lock().await;
            touch_entry(&mut state, key, now)
        }?;
        if persist_use {
            self.changed.notify_one();
        }
        Some(value)
    }

    #[cfg(test)]
    pub(crate) async fn put(&self, key: CacheKey, value: Vec<u8>) -> io::Result<()> {
        validate_value(&value)?;
        let now = now_epoch_seconds();
        let mut state = self.state.lock().await;
        insert_entry(&mut state, key, value, now);
        state.revision = state.revision.wrapping_add(1);
        self.changed.notify_one();
        Ok(())
    }

    pub(crate) async fn acquire(&self, key: CacheKey) -> Acquire {
        enum Decision {
            Hit(Arc<[u8]>, bool),
            Owner(u64),
            Wait {
                notify: Arc<Notify>,
                deadline: Instant,
            },
        }

        loop {
            let decision = {
                let mut state = self.state.lock().await;
                if let Some((value, persist_use)) =
                    touch_entry(&mut state, key, now_epoch_seconds())
                {
                    Decision::Hit(value, persist_use)
                } else {
                    let now = Instant::now();
                    if state
                        .in_flight
                        .get(&key)
                        .is_some_and(|lease| lease.expires_at <= now)
                        && let Some(lease) = state.in_flight.remove(&key)
                    {
                        lease.notify.notify_waiters();
                    }
                    if let Some(lease) = state.in_flight.get(&key) {
                        Decision::Wait {
                            notify: Arc::clone(&lease.notify),
                            deadline: lease.expires_at,
                        }
                    } else {
                        state.next_token = state.next_token.wrapping_add(1);
                        let token = state.next_token;
                        state.in_flight.insert(
                            key,
                            Lease {
                                token,
                                expires_at: now + LEASE_DURATION,
                                notify: Arc::new(Notify::new()),
                            },
                        );
                        Decision::Owner(token)
                    }
                }
            };

            match decision {
                Decision::Hit(value, persist_use) => {
                    if persist_use {
                        self.changed.notify_one();
                    }
                    return Acquire::Hit(value);
                }
                Decision::Owner(token) => return Acquire::Owner(token),
                Decision::Wait { notify, deadline } => {
                    let notified = notify.notified();
                    let _ = tokio::time::timeout_at(deadline, notified).await;
                }
            }
        }
    }

    pub(crate) async fn put_owned(
        &self,
        key: CacheKey,
        token: u64,
        value: Vec<u8>,
    ) -> io::Result<bool> {
        validate_value(&value)?;
        let notify = {
            let mut state = self.state.lock().await;
            let Some(lease) = state.in_flight.get(&key) else {
                return Ok(false);
            };
            if lease.token != token {
                return Ok(false);
            }
            let notify = Arc::clone(&lease.notify);
            state.in_flight.remove(&key);
            insert_entry(&mut state, key, value, now_epoch_seconds());
            state.revision = state.revision.wrapping_add(1);
            notify
        };
        notify.notify_waiters();
        self.changed.notify_one();
        Ok(true)
    }

    pub(crate) async fn release_owned(&self, key: CacheKey, token: u64) -> bool {
        let notify = {
            let mut state = self.state.lock().await;
            let Some(lease) = state.in_flight.get(&key) else {
                return false;
            };
            if lease.token != token {
                return false;
            }
            let notify = Arc::clone(&lease.notify);
            state.in_flight.remove(&key);
            notify
        };
        notify.notify_waiters();
        true
    }

    pub(crate) async fn remove(&self, key: CacheKey) -> io::Result<()> {
        let mut state = self.state.lock().await;
        if state.entries.remove(&key).is_some() {
            state.revision = state.revision.wrapping_add(1);
            self.changed.notify_one();
        }
        Ok(())
    }

    pub(crate) async fn clear(&self) -> io::Result<()> {
        let (clear_revision, leases) = {
            let mut state = self.state.lock().await;
            let leases = state
                .in_flight
                .drain()
                .map(|(_, lease)| lease.notify)
                .collect::<Vec<_>>();
            state.entries.clear();
            state.load_epoch = state.load_epoch.wrapping_add(1);
            state.revision = state.revision.wrapping_add(1);
            (state.revision, leases)
        };
        for lease in leases {
            lease.notify_waiters();
        }

        let path = self.path.clone();
        let _disk = self.disk_io.lock().await;
        let result = tokio::task::spawn_blocking(move || disk::clear_all(path.as_deref()))
            .await
            .map_err(io::Error::other)?;
        if result.is_ok() {
            let mut state = self.state.lock().await;
            state.saved_revision = clear_revision;
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

fn touch_entry(state: &mut State, key: CacheKey, now: u64) -> Option<(Arc<[u8]>, bool)> {
    state.lru_order = state.lru_order.saturating_add(1);
    let lru_order = state.lru_order;
    let entry = state.entries.get_mut(&key)?;
    entry.last_used_at = now;
    entry.lru_order = lru_order;
    let persist_use = now.saturating_sub(entry.persisted_last_used_at) >= LAST_USED_SAVE_INTERVAL;
    if persist_use {
        state.revision = state.revision.wrapping_add(1);
    }
    Some((entry.value.clone(), persist_use))
}

fn insert_entry(state: &mut State, key: CacheKey, value: Vec<u8>, now: u64) {
    state.lru_order = state.lru_order.saturating_add(1);
    let lru_order = state.lru_order;
    state.entries.insert(key, Entry::new(value, now, lru_order));
    trim_lru(&mut state.entries);
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
    path_for_file_name(CACHE_FILE_NAME)
}

pub(crate) fn path_for_file_name(file_name: &str) -> Option<PathBuf> {
    cache_root().map(|root| root.join("ztheme").join(file_name))
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::{Acquire, CacheKey, Entry, MAX_VALUE_BYTES, RuntimeCache, State, disk, trim_lru};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ztheme-cache-test-{}-{sequence}",
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

    fn disk_backed_cache(path: PathBuf) -> Arc<RuntimeCache> {
        Arc::new(RuntimeCache {
            state: tokio::sync::Mutex::new(State::default()),
            disk_io: tokio::sync::Mutex::new(()),
            changed: Notify::new(),
            path: Some(path),
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_keeps_concurrent_puts_dirty_until_flushed() {
        let directory = TestDirectory::new();
        let path = directory.path().join("runtime-v2.bin");
        let cache = disk_backed_cache(path.clone());
        cache
            .put(CacheKey::from_value(1), b"first".to_vec())
            .await
            .unwrap();
        cache.flush_latest().await.unwrap();

        let disk_guard = cache.disk_io.lock().await;
        let clear_task = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.clear().await })
        };
        loop {
            if cache.state.lock().await.entries.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        cache
            .put(CacheKey::from_value(2), b"second".to_vec())
            .await
            .unwrap();
        drop(disk_guard);
        clear_task.await.unwrap().unwrap();
        {
            let state = cache.state.lock().await;
            assert_ne!(state.revision, state.saved_revision);
        }
        cache.flush_latest().await.unwrap();
        let loaded = disk::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(&*loaded[&CacheKey::from_value(2)].value, b"second");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clear_is_scoped_to_one_persistent_path() {
        let directory = TestDirectory::new();
        let first_path = directory.path().join("runtime-v2-dev-one.bin");
        let second_path = directory.path().join("runtime-v2-dev-two.bin");
        let first = disk_backed_cache(first_path.clone());
        let second = disk_backed_cache(second_path.clone());
        first
            .put(CacheKey::from_value(1), b"first".to_vec())
            .await
            .unwrap();
        second
            .put(CacheKey::from_value(2), b"second".to_vec())
            .await
            .unwrap();
        first.flush_latest().await.unwrap();
        second.flush_latest().await.unwrap();

        first.clear().await.unwrap();

        assert!(!first_path.exists());
        assert_eq!(
            &*disk::load(&second_path).unwrap()[&CacheKey::from_value(2)].value,
            b"second"
        );
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

    #[tokio::test(flavor = "current_thread")]
    async fn lru_recency_preserves_a_revisited_old_entry() {
        let cache = RuntimeCache::new();
        for value in 0..500 {
            cache
                .put(CacheKey::from_value(value), vec![value.to_le_bytes()[0]])
                .await
                .unwrap();
        }
        assert!(cache.get(CacheKey::from_value(0)).await.is_some());
        cache
            .put(CacheKey::from_value(500), vec![500_u16.to_le_bytes()[0]])
            .await
            .unwrap();

        assert_eq!(
            cache.get(CacheKey::from_value(0)).await.as_deref(),
            Some([0_u8].as_slice())
        );
        assert!(cache.get(CacheKey::from_value(1)).await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lru_recency_survives_persistence_and_restart() {
        let directory = TestDirectory::new();
        let path = directory.path().join("runtime-v2.bin");
        let cache = disk_backed_cache(path.clone());
        for value in 0..500 {
            cache
                .put(CacheKey::from_value(value), vec![value.to_le_bytes()[0]])
                .await
                .unwrap();
        }
        cache.flush_latest().await.unwrap();
        assert!(cache.get(CacheKey::from_value(0)).await.is_some());
        cache.flush_latest().await.unwrap();
        cache
            .put(CacheKey::from_value(500), vec![500_u16.to_le_bytes()[0]])
            .await
            .unwrap();
        cache.flush_latest().await.unwrap();

        let restored = disk_backed_cache(path);
        restored.clone().load().await;
        assert!(restored.get(CacheKey::from_value(0)).await.is_some());
        assert!(restored.get(CacheKey::from_value(1)).await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_acquires_have_one_owner() {
        let cache = Arc::new(RuntimeCache::new());
        let key = CacheKey::from_value(9);
        let owner = cache.acquire(key).await;
        let token = match owner {
            Acquire::Owner(token) => token,
            Acquire::Hit(_) => panic!("unexpected cache hit"),
        };
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            waiters.push(tokio::spawn(async move { cache.acquire(key).await }));
        }
        tokio::task::yield_now().await;
        assert!(
            cache
                .put_owned(key, token, b"value".to_vec())
                .await
                .unwrap()
        );
        for waiter in waiters {
            assert!(matches!(waiter.await.unwrap(), Acquire::Hit(_)));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_owner_cannot_overwrite_a_replacement() {
        let cache = Arc::new(RuntimeCache::new());
        let key = CacheKey::from_value(10);
        let first = match cache.acquire(key).await {
            Acquire::Owner(token) => token,
            Acquire::Hit(_) => panic!("unexpected cache hit"),
        };
        tokio::time::sleep(Duration::from_millis(425)).await;
        let second = match cache.acquire(key).await {
            Acquire::Owner(token) => token,
            Acquire::Hit(_) => panic!("unexpected cache hit"),
        };
        assert_ne!(first, second);
        assert!(
            !cache
                .put_owned(key, first, b"stale".to_vec())
                .await
                .unwrap()
        );
        assert!(
            cache
                .put_owned(key, second, b"fresh".to_vec())
                .await
                .unwrap()
        );
        assert_eq!(cache.get(key).await.as_deref(), Some(b"fresh".as_slice()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_owner_can_put_when_lease_was_not_reclaimed() {
        let cache = RuntimeCache::new();
        let key = CacheKey::from_value(11);
        let token = match cache.acquire(key).await {
            Acquire::Owner(token) => token,
            Acquire::Hit(_) => panic!("unexpected cache hit"),
        };
        tokio::time::sleep(Duration::from_millis(425)).await;

        assert!(
            cache
                .put_owned(key, token, b"value".to_vec())
                .await
                .unwrap()
        );
        assert_eq!(cache.get(key).await.as_deref(), Some(b"value".as_slice()));
    }
}
