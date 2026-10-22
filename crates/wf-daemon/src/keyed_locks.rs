//! Locks by key, so that work on one session or one workforce waits for its own kind only.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A lock per key, made when first wanted and dropped when nobody holds or waits for it.
pub(crate) struct KeyedLocks<K> {
    locks: Mutex<BTreeMap<K, Arc<tokio::sync::Mutex<()>>>>,
}

/// Holds the lock of one key until dropped. It owns its share of the locks, so it can move into
/// a spawned task, and it is made before the wait, so a cancelled wait cleans up too.
pub(crate) struct KeyedGuard<K: Ord + Copy> {
    owner: Arc<KeyedLocks<K>>,
    key: K,
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            locks: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<K: Ord + Copy> KeyedLocks<K> {
    /// Waits for the lock of `key`; the locks of other keys do not matter.
    pub(crate) async fn lock(self: &Arc<Self>, key: K) -> KeyedGuard<K> {
        let lock = Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(key)
                .or_default(),
        );
        let mut guard = KeyedGuard {
            owner: Arc::clone(self),
            key,
            lock: Some(Arc::clone(&lock)),
            held: None,
        };
        guard.held = Some(lock.lock_owned().await);
        guard
    }
}

impl<K: Ord + Copy> Drop for KeyedGuard<K> {
    fn drop(&mut self) {
        // Released first, so that the map holds the only reference when nobody waits.
        drop(self.held.take());
        drop(self.lock.take());
        let mut locks = self
            .owner
            .locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if locks
            .get(&self.key)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            locks.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn keys_do_not_wait_on_each_other_and_locks_are_dropped() {
        let locks = Arc::new(KeyedLocks::<u8>::default());
        let first = locks.lock(1).await;
        let second = locks.lock(2).await;
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(50), locks.lock(1));
        assert!(waiting.await.is_err());
        drop((first, second));
        assert!(locks.locks.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_waiter_leaves_no_lock_behind() {
        let locks = Arc::new(KeyedLocks::<u8>::default());
        let holder = locks.lock(1).await;
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(20), locks.lock(1));
        assert!(waiting.await.is_err());
        drop(holder);
        assert!(locks.locks.lock().unwrap().is_empty());
    }
}
