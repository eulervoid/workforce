use std::collections::BTreeSet;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use redb::{
    Database, DatabaseError, MultimapTableDefinition, ReadableDatabase, ReadableTable,
    ReadableTableMetadata, TableDefinition,
};
use wf_core::event::Event;
use wf_core::id::{EventId, HashId};
use wf_core::identity::DeviceKey;

use crate::store::{Commit, EventStore, PendingChange, SnapshotChange, StoreError, StoredSnapshot};

const EVENTS: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("events");
/// `(author, seq)` to the ids of the events there: more than one when a feed has equivocated.
const FEEDS: MultimapTableDefinition<([u8; 32], u64), [u8; 32]> =
    MultimapTableDefinition::new("feeds");
/// Position to event id, in the order events were applied.
const LOG: TableDefinition<u64, [u8; 32]> = TableDefinition::new("log");
const PENDING: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("pending");
/// Snapshots keyed by the hash of the heads they cover.
const SNAPSHOTS: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("snapshots");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const LATEST_SNAPSHOT: &str = "latest_snapshot";

/// Another `wf` process holds the single-writer lock: retry briefly before giving up.
const LOCK_ATTEMPTS: u32 = 12;
const LOCK_FIRST_WAIT: Duration = Duration::from_millis(10);

pub struct RedbStore {
    database: Database,
}

impl RedbStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let mut wait = LOCK_FIRST_WAIT;
        for attempt in 1..=LOCK_ATTEMPTS {
            match Database::create(path.as_ref()) {
                Ok(database) => {
                    let store = Self { database };
                    store.create_tables()?;
                    return Ok(store);
                }
                Err(DatabaseError::DatabaseAlreadyOpen) if attempt < LOCK_ATTEMPTS => {
                    sleep(wait);
                    wait = (wait * 2).min(Duration::from_millis(400));
                }
                Err(DatabaseError::DatabaseAlreadyOpen) => return Err(StoreError::Locked),
                Err(error) => return Err(error.into()),
            }
        }
        Err(StoreError::Locked)
    }

    /// A store that lives and dies with the process, for tests and simulations.
    pub fn in_memory() -> Result<Self, StoreError> {
        let database =
            Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?;
        let store = Self { database };
        store.create_tables()?;
        Ok(store)
    }

    fn create_tables(&self) -> Result<(), StoreError> {
        let transaction = self.database.begin_write()?;
        transaction.open_table(EVENTS)?;
        transaction.open_multimap_table(FEEDS)?;
        transaction.open_table(LOG)?;
        transaction.open_table(PENDING)?;
        transaction.open_table(SNAPSHOTS)?;
        transaction.open_table(META)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn event_count(&self) -> Result<u64, StoreError> {
        let transaction = self.database.begin_read()?;
        Ok(transaction.open_table(EVENTS)?.len()?)
    }
}

fn snapshot_key(covers: &BTreeSet<EventId>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for head in covers {
        hasher.update(head.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn decode_all<'a>(
    rows: impl Iterator<
        Item = Result<
            (
                redb::AccessGuard<'a, [u8; 32]>,
                redb::AccessGuard<'a, &'static [u8]>,
            ),
            redb::StorageError,
        >,
    >,
) -> Result<Vec<Event>, StoreError> {
    rows.map(|row| {
        let (_, bytes) = row?;
        Ok(Event::decode(bytes.value())?)
    })
    .collect()
}

impl EventStore for RedbStore {
    fn event(&self, id: &EventId) -> Result<Option<Event>, StoreError> {
        let transaction = self.database.begin_read()?;
        let events = transaction.open_table(EVENTS)?;
        let Some(bytes) = events.get(*id.as_bytes())? else {
            return Ok(None);
        };
        Ok(Some(Event::decode(bytes.value())?))
    }

    fn feed_since(&self, author: &DeviceKey, after_seq: u64) -> Result<Vec<Event>, StoreError> {
        let transaction = self.database.begin_read()?;
        let feeds = transaction.open_multimap_table(FEEDS)?;
        let events = transaction.open_table(EVENTS)?;
        let author = *author.as_bytes();
        let mut result = Vec::new();
        for row in feeds.range((author, after_seq.saturating_add(1))..=(author, u64::MAX))? {
            let (_, ids) = row?;
            for id in ids {
                let bytes = events.get(id?.value())?.ok_or_else(|| {
                    StoreError::Inconsistent("a feed points at a missing event".into())
                })?;
                result.push(Event::decode(bytes.value())?);
            }
        }
        Ok(result)
    }

    fn events(&self) -> Result<Vec<Event>, StoreError> {
        let transaction = self.database.begin_read()?;
        let mut events = decode_all(transaction.open_table(EVENTS)?.iter()?)?;
        events.sort_by_key(|event| (event.content.hlc, event.id()));
        Ok(events)
    }

    fn log_len(&self) -> Result<u64, StoreError> {
        let transaction = self.database.begin_read()?;
        Ok(transaction.open_table(LOG)?.len()?)
    }

    fn log_id(&self, position: u64) -> Result<Option<EventId>, StoreError> {
        let transaction = self.database.begin_read()?;
        Ok(transaction
            .open_table(LOG)?
            .get(position)?
            .map(|id| EventId::from_bytes(id.value())))
    }

    fn log_since(&self, position: u64) -> Result<Vec<Event>, StoreError> {
        let transaction = self.database.begin_read()?;
        let log = transaction.open_table(LOG)?;
        let events = transaction.open_table(EVENTS)?;
        let mut result = Vec::new();
        for row in log.range(position..)? {
            let (_, id) = row?;
            let bytes = events.get(id.value())?.ok_or_else(|| {
                StoreError::Inconsistent("the log points at a missing event".into())
            })?;
            result.push(Event::decode(bytes.value())?);
        }
        Ok(result)
    }

    fn pending(&self) -> Result<Vec<Event>, StoreError> {
        let transaction = self.database.begin_read()?;
        decode_all(transaction.open_table(PENDING)?.iter()?)
    }

    fn snapshot(&self) -> Result<Option<StoredSnapshot>, StoreError> {
        let transaction = self.database.begin_read()?;
        let Some(key) = transaction.open_table(META)?.get(LATEST_SNAPSHOT)? else {
            return Ok(None);
        };
        let key: [u8; 32] = key
            .value()
            .try_into()
            .map_err(|_| StoreError::Inconsistent("bad snapshot key".into()))?;
        let Some(blob) = transaction.open_table(SNAPSHOTS)?.get(key)? else {
            return Ok(None);
        };
        let (covers, bytes): (Vec<EventId>, Vec<u8>) = wf_core::encoding::decode(blob.value())?;
        Ok(Some(StoredSnapshot {
            covers: covers.into_iter().collect(),
            bytes,
        }))
    }

    fn commit(&self, commit: Commit<'_>) -> Result<(), StoreError> {
        let transaction = self.database.begin_write()?;
        {
            let mut events = transaction.open_table(EVENTS)?;
            let mut feeds = transaction.open_multimap_table(FEEDS)?;
            let mut log = transaction.open_table(LOG)?;
            for (position, event) in (log.len()?..).zip(commit.append) {
                let id = event.id();
                events.insert(*id.as_bytes(), event.encode()?.as_slice())?;
                feeds.insert(
                    (*event.content.author.as_bytes(), event.content.seq),
                    *id.as_bytes(),
                )?;
                log.insert(position, *id.as_bytes())?;
            }
        }
        if let PendingChange::Update { put, remove } = commit.pending {
            let mut pending = transaction.open_table(PENDING)?;
            for id in remove {
                pending.remove(*id.as_bytes())?;
            }
            for event in put {
                pending.insert(*event.id().as_bytes(), event.encode()?.as_slice())?;
            }
        }
        match commit.snapshot {
            SnapshotChange::Keep => {}
            SnapshotChange::Replace(snapshot) => {
                let mut snapshots = transaction.open_table(SNAPSHOTS)?;
                let mut meta = transaction.open_table(META)?;
                let previous = meta
                    .get(LATEST_SNAPSHOT)?
                    .and_then(|key| <[u8; 32]>::try_from(key.value()).ok());
                if let Some(previous) = previous {
                    snapshots.remove(previous)?;
                }
                let key = snapshot_key(&snapshot.covers);
                let covers: Vec<EventId> = snapshot.covers.iter().copied().collect();
                let blob = wf_core::encoding::encode(&(covers, &snapshot.bytes))?;
                snapshots.insert(key, blob.as_slice())?;
                meta.insert(LATEST_SNAPSHOT, key.as_slice())?;
            }
            SnapshotChange::Discard => {
                let mut snapshots = transaction.open_table(SNAPSHOTS)?;
                let mut meta = transaction.open_table(META)?;
                let previous = meta
                    .remove(LATEST_SNAPSHOT)?
                    .and_then(|key| <[u8; 32]>::try_from(key.value()).ok());
                if let Some(previous) = previous {
                    snapshots.remove(previous)?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }
}
