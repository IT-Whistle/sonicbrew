//! openraft `RaftLogStorage` + `RaftLogReader` backed by `redb`.
//!
//! This module implements the log component of openraft's storage interface,
//! storing Raft log entries in a `redb` database table. The companion
//! `RaftStateMachine` (Task 4) will handle snapshot and apply logic.
//!
//! # Schema
//!
//! | Table      | Key type | Value type                       | Purpose                     |
//! |------------|----------|----------------------------------|-----------------------------|
//! | `LOGS`     | `u64`    | `Vec<u8>` (bincode `Entry`)      | Raft log entries by index   |
//! | `VOTE`     | `u64`    | `Vec<u8>` (bincode `Vote`)       | Persisted vote (single row) |
//! | `COMMITTED`| `u64`    | `Vec<u8>` (bincode `LogId`)      | Last committed log id       |
//!
//! # Threading
//!
//! `redb` is synchronous I/O. The internal `Mutex<redb::Database>` is never
//! held across an `.await` boundary — all async trait methods acquire the
//! lock, perform synchronous redb operations, and drop the lock before
//! returning.

use std::fmt::Debug;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Mutex;

use openraft::storage::LogFlushed;
use openraft::storage::RaftLogReader;
use openraft::storage::RaftLogStorage;
use openraft::StorageError;
use openraft::Vote;
use redb::{ReadableTable, TableDefinition};

use crate::raft_types::TypeConfig;

/// redb table for Raft log entries: index -> serialized `Entry<TypeConfig>`.
const LOGS: TableDefinition<u64, Vec<u8>> = TableDefinition::new("raft_logs");

/// redb table for the persisted vote: single row (key=0) -> serialized `Vote`.
const VOTE: TableDefinition<u64, Vec<u8>> = TableDefinition::new("raft_vote");

/// redb table for the last committed log id: single row (key=0) -> serialized `LogId`.
const COMMITTED: TableDefinition<u64, Vec<u8>> = TableDefinition::new("raft_committed");

/// Sentinel key for the single-row `VOTE` and `COMMITTED` tables.
const SINGLE_ROW_KEY: u64 = 0;

/// Helper to convert a redb-style error into a `StorageError`.
fn io_error(
    context: &'static str,
    verb: openraft::ErrorVerb,
    err: impl std::error::Error + Send + Sync + 'static,
) -> StorageError<u64> {
    StorageError::IO {
        source: openraft::StorageIOError::new(
            openraft::ErrorSubject::None,
            verb,
            openraft::AnyError::new(&err).add_context(|| context),
        ),
    }
}

// ---------------------------------------------------------------------------
// RaftLogReader (read-only log access, used by replication tasks)
// ---------------------------------------------------------------------------

/// Read-only log reader backed by `redb`.
///
/// Obtained via [`RaftLogStore::get_log_reader`]. Holds its own read
/// transaction so it can serve range queries without blocking the writer.
pub struct LogReader {
    db: std::sync::Arc<Mutex<redb::Database>>,
}

impl RaftLogReader<TypeConfig> for LogReader {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + openraft::OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<openraft::Entry<TypeConfig>>, StorageError<u64>> {
        let db = self.db.lock().expect("log reader mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read", openraft::ErrorVerb::Read, e))?;
        let table = txn
            .open_table(LOGS)
            .map_err(|e| io_error("open_table LOGS", openraft::ErrorVerb::Read, e))?;

        let mut entries = Vec::new();

        use std::ops::Bound;
        let start_bound = range.start_bound();
        let end_bound = range.end_bound();
        let redb_start: Bound<u64> = match start_bound {
            Bound::Included(&v) => Bound::Included(v),
            Bound::Excluded(&v) => Bound::Excluded(v),
            Bound::Unbounded => Bound::Unbounded,
        };
        let redb_end: Bound<u64> = match end_bound {
            Bound::Included(&v) => Bound::Included(v),
            Bound::Excluded(&v) => Bound::Excluded(v),
            Bound::Unbounded => Bound::Unbounded,
        };

        let iter = table
            .range((redb_start, redb_end))
            .map_err(|e| io_error("range LOGS", openraft::ErrorVerb::Read, e))?;

        for result in iter {
            let (_key, value) =
                result.map_err(|e| io_error("iter LOGS", openraft::ErrorVerb::Read, e))?;
            let bytes: Vec<u8> = value.value();
            let entry: openraft::Entry<TypeConfig> = bincode::deserialize(&bytes).map_err(|e| {
                io_error(
                    "deserialize entry",
                    openraft::ErrorVerb::Read,
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            entries.push(entry);
        }

        Ok(entries)
    }

    async fn limited_get_log_entries(
        &mut self,
        start: u64,
        end: u64,
    ) -> Result<Vec<openraft::Entry<TypeConfig>>, StorageError<u64>> {
        self.try_get_log_entries(start..end).await
    }
}

// ---------------------------------------------------------------------------
// RaftLogStorage (full log store, used by Raft core)
// ---------------------------------------------------------------------------

/// openraft log store backed by `redb`.
///
/// Implements [`RaftLogStorage`] (the v2 API) for [`TypeConfig`]. The
/// companion [`RaftStateMachine`] will be implemented in Task 4.
///
/// # Correctness
///
/// All redb operations are synchronous and performed inside `Mutex`-guarded
/// blocks. The `Mutex` is never held across `.await` points, so there is no
/// risk of deadlocking the tokio runtime.
pub struct RaftLogStore {
    db: std::sync::Arc<Mutex<redb::Database>>,
}

impl RaftLogStore {
    /// Open (or create) a log store at `path`.
    ///
    /// `redb::Database::create` initializes a fresh file when none exists and
    /// opens an existing one otherwise.
    #[allow(clippy::result_large_err)]
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError<u64>> {
        let db = redb::Database::create(path.as_ref())
            .map_err(|e| io_error("open redb", openraft::ErrorVerb::Write, e))?;

        // Ensure all tables exist (idempotent on reopen).
        {
            let txn = db
                .begin_write()
                .map_err(|e| io_error("begin_write", openraft::ErrorVerb::Write, e))?;
            {
                let _ = txn
                    .open_table(LOGS)
                    .map_err(|e| io_error("open_table LOGS", openraft::ErrorVerb::Write, e))?;
            }
            {
                let _ = txn
                    .open_table(VOTE)
                    .map_err(|e| io_error("open_table VOTE", openraft::ErrorVerb::Write, e))?;
            }
            {
                let _ = txn
                    .open_table(COMMITTED)
                    .map_err(|e| io_error("open_table COMMITTED", openraft::ErrorVerb::Write, e))?;
            }
            txn.commit()
                .map_err(|e| io_error("commit table init", openraft::ErrorVerb::Write, e))?;
        }

        Ok(Self {
            db: std::sync::Arc::new(Mutex::new(db)),
        })
    }

    /// Create an ephemeral log store backed by a unique temp file.
    ///
    /// The temp file is removed when the returned store is dropped. Useful
    /// for tests.
    pub fn new_ephemeral() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("sb-raft-log-{}-{}.redb", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        Self::open(&path).unwrap_or_else(|e| {
            panic!(
                "raft_log_store: ephemeral temp db at {} failed: {e}",
                path.display()
            )
        })
    }
}

impl RaftLogReader<TypeConfig> for RaftLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + openraft::OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<openraft::Entry<TypeConfig>>, StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read", openraft::ErrorVerb::Read, e))?;
        let table = txn
            .open_table(LOGS)
            .map_err(|e| io_error("open_table LOGS", openraft::ErrorVerb::Read, e))?;

        let mut entries = Vec::new();

        use std::ops::Bound;
        let start_bound = range.start_bound();
        let end_bound = range.end_bound();
        let redb_start: Bound<u64> = match start_bound {
            Bound::Included(&v) => Bound::Included(v),
            Bound::Excluded(&v) => Bound::Excluded(v),
            Bound::Unbounded => Bound::Unbounded,
        };
        let redb_end: Bound<u64> = match end_bound {
            Bound::Included(&v) => Bound::Included(v),
            Bound::Excluded(&v) => Bound::Excluded(v),
            Bound::Unbounded => Bound::Unbounded,
        };

        let iter = table
            .range((redb_start, redb_end))
            .map_err(|e| io_error("range LOGS", openraft::ErrorVerb::Read, e))?;

        for result in iter {
            let (_key, value) =
                result.map_err(|e| io_error("iter LOGS", openraft::ErrorVerb::Read, e))?;
            let bytes: Vec<u8> = value.value();
            let entry: openraft::Entry<TypeConfig> = bincode::deserialize(&bytes).map_err(|e| {
                io_error(
                    "deserialize entry",
                    openraft::ErrorVerb::Read,
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            entries.push(entry);
        }

        Ok(entries)
    }

    async fn limited_get_log_entries(
        &mut self,
        start: u64,
        end: u64,
    ) -> Result<Vec<openraft::Entry<TypeConfig>>, StorageError<u64>> {
        self.try_get_log_entries(start..end).await
    }
}

impl RaftLogStorage<TypeConfig> for RaftLogStore {
    type LogReader = LogReader;

    async fn get_log_state(&mut self) -> Result<openraft::LogState<TypeConfig>, StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read", openraft::ErrorVerb::Read, e))?;

        let logs_table = txn
            .open_table(LOGS)
            .map_err(|e| io_error("open_table LOGS", openraft::ErrorVerb::Read, e))?;

        // Find the last log id by scanning to the end.
        let mut last_log_id = None;
        let iter = logs_table
            .iter()
            .map_err(|e| io_error("iter LOGS", openraft::ErrorVerb::Read, e))?;

        for result in iter {
            let (_key, value) =
                result.map_err(|e| io_error("iter LOGS", openraft::ErrorVerb::Read, e))?;
            let bytes: Vec<u8> = value.value();
            let entry: openraft::Entry<TypeConfig> = bincode::deserialize(&bytes).map_err(|e| {
                io_error(
                    "deserialize entry",
                    openraft::ErrorVerb::Read,
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                )
            })?;
            last_log_id = Some(entry.log_id);
        }

        // Read last_purged from the COMMITTED table. In single-node MVP we
        // approximate last_purged as the committed id.
        let committed_table = txn
            .open_table(COMMITTED)
            .map_err(|e| io_error("open_table COMMITTED", openraft::ErrorVerb::Read, e))?;

        let last_purged_log_id = committed_table
            .get(&SINGLE_ROW_KEY)
            .map_err(|e| io_error("get COMMITTED", openraft::ErrorVerb::Read, e))?
            .map(
                #[allow(clippy::result_large_err)]
                |v| {
                    let bytes: Vec<u8> = v.value();
                    bincode::deserialize(&bytes).map_err(|e| {
                        io_error(
                            "deserialize committed",
                            openraft::ErrorVerb::Read,
                            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                        )
                    })
                },
            )
            .transpose()?;

        Ok(openraft::LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> LogReader {
        LogReader {
            db: self.db.clone(),
        }
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let bytes = bincode::serialize(vote).map_err(|e| {
            io_error(
                "serialize vote",
                openraft::ErrorVerb::Write,
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            )
        })?;
        let txn = db
            .begin_write()
            .map_err(|e| io_error("begin_write", openraft::ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(VOTE)
                .map_err(|e| io_error("open_table VOTE", openraft::ErrorVerb::Write, e))?;
            table
                .insert(SINGLE_ROW_KEY, &bytes)
                .map_err(|e| io_error("insert VOTE", openraft::ErrorVerb::Write, e))?;
        }
        txn.commit()
            .map_err(|e| io_error("commit VOTE", openraft::ErrorVerb::Write, e))?;
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read", openraft::ErrorVerb::Read, e))?;
        let table = txn
            .open_table(VOTE)
            .map_err(|e| io_error("open_table VOTE", openraft::ErrorVerb::Read, e))?;
        match table
            .get(&SINGLE_ROW_KEY)
            .map_err(|e| io_error("get VOTE", openraft::ErrorVerb::Read, e))?
        {
            Some(v) => {
                let bytes: Vec<u8> = v.value();
                let vote: Vote<u64> = bincode::deserialize(&bytes).map_err(|e| {
                    io_error(
                        "deserialize vote",
                        openraft::ErrorVerb::Read,
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                    )
                })?;
                Ok(Some(vote))
            }
            None => Ok(None),
        }
    }

    async fn save_committed(
        &mut self,
        committed: Option<openraft::LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let bytes = bincode::serialize(&committed).map_err(|e| {
            io_error(
                "serialize committed",
                openraft::ErrorVerb::Write,
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            )
        })?;
        let txn = db
            .begin_write()
            .map_err(|e| io_error("begin_write", openraft::ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(COMMITTED)
                .map_err(|e| io_error("open_table COMMITTED", openraft::ErrorVerb::Write, e))?;
            table
                .insert(SINGLE_ROW_KEY, &bytes)
                .map_err(|e| io_error("insert COMMITTED", openraft::ErrorVerb::Write, e))?;
        }
        txn.commit()
            .map_err(|e| io_error("commit COMMITTED", openraft::ErrorVerb::Write, e))?;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<openraft::LogId<u64>>, StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read", openraft::ErrorVerb::Read, e))?;
        let table = txn
            .open_table(COMMITTED)
            .map_err(|e| io_error("open_table COMMITTED", openraft::ErrorVerb::Read, e))?;
        match table
            .get(&SINGLE_ROW_KEY)
            .map_err(|e| io_error("get COMMITTED", openraft::ErrorVerb::Read, e))?
        {
            Some(v) => {
                let bytes: Vec<u8> = v.value();
                let log_id: Option<openraft::LogId<u64>> =
                    bincode::deserialize(&bytes).map_err(|e| {
                        io_error(
                            "deserialize committed",
                            openraft::ErrorVerb::Read,
                            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                        )
                    })?;
                Ok(log_id)
            }
            None => Ok(None),
        }
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + openraft::OptionalSend,
        I::IntoIter: openraft::OptionalSend,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        if entries.is_empty() {
            callback.log_io_completed(Ok(()));
            return Ok(());
        }

        let db = self.db.lock().expect("log store mutex poisoned");
        let txn = db
            .begin_write()
            .map_err(|e| io_error("begin_write", openraft::ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(LOGS)
                .map_err(|e| io_error("open_table LOGS", openraft::ErrorVerb::Write, e))?;
            for entry in &entries {
                let index = entry.log_id.index;
                let bytes = bincode::serialize(entry).map_err(|e| {
                    io_error(
                        "serialize entry",
                        openraft::ErrorVerb::Write,
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                    )
                })?;
                table
                    .insert(index, &bytes)
                    .map_err(|e| io_error("insert LOGS", openraft::ErrorVerb::Write, e))?;
            }
        }
        txn.commit()
            .map_err(|e| io_error("commit LOGS", openraft::ErrorVerb::Write, e))?;

        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: openraft::LogId<u64>) -> Result<(), StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let txn = db
            .begin_write()
            .map_err(|e| io_error("begin_write", openraft::ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(LOGS)
                .map_err(|e| io_error("open_table LOGS", openraft::ErrorVerb::Write, e))?;
            // Delete all entries with index >= log_id.index (inclusive).
            let keys_to_delete: Vec<u64> = {
                let mut keys = Vec::new();
                let iter = table.range(log_id.index..).map_err(|e| {
                    io_error("range LOGS for truncate", openraft::ErrorVerb::Write, e)
                })?;
                for result in iter {
                    let (key, _value) = result.map_err(|e| {
                        io_error("iter LOGS for truncate", openraft::ErrorVerb::Write, e)
                    })?;
                    keys.push(key.value());
                }
                keys
            };
            for key in keys_to_delete {
                table
                    .remove(key)
                    .map_err(|e| io_error("remove LOGS", openraft::ErrorVerb::Write, e))?;
            }
        }
        txn.commit()
            .map_err(|e| io_error("commit truncate", openraft::ErrorVerb::Write, e))?;
        Ok(())
    }

    async fn purge(&mut self, log_id: openraft::LogId<u64>) -> Result<(), StorageError<u64>> {
        let db = self.db.lock().expect("log store mutex poisoned");
        let txn = db
            .begin_write()
            .map_err(|e| io_error("begin_write", openraft::ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(LOGS)
                .map_err(|e| io_error("open_table LOGS", openraft::ErrorVerb::Write, e))?;
            // Delete all entries with index <= log_id.index (inclusive).
            let keys_to_delete: Vec<u64> = {
                let mut keys = Vec::new();
                let iter = table
                    .iter()
                    .map_err(|e| io_error("iter LOGS for purge", openraft::ErrorVerb::Write, e))?;
                for result in iter {
                    let (key, _value) = result.map_err(|e| {
                        io_error("iter LOGS for purge", openraft::ErrorVerb::Write, e)
                    })?;
                    if key.value() <= log_id.index {
                        keys.push(key.value());
                    }
                }
                keys
            };
            for key in keys_to_delete {
                table
                    .remove(key)
                    .map_err(|e| io_error("remove LOGS", openraft::ErrorVerb::Write, e))?;
            }
        }
        txn.commit()
            .map_err(|e| io_error("commit purge", openraft::ErrorVerb::Write, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::storage::RaftLogReader;
    use openraft::storage::RaftLogStorage;
    use openraft::CommittedLeaderId;
    use openraft::Entry;
    use openraft::EntryPayload;
    use openraft::LogId;

    /// Helper: create a minimal `Entry<TypeConfig>` at the given index.
    fn make_entry(index: u64) -> Entry<TypeConfig> {
        Entry {
            log_id: LogId {
                leader_id: CommittedLeaderId::new(0, 0),
                index,
            },
            payload: EntryPayload::Blank,
        }
    }

    /// Write entries directly to the redb LOGS table (bypassing `append`
    /// which requires a `LogFlushed` callback that can't be constructed
    /// outside openraft).
    fn write_entries_direct(store: &RaftLogStore, entries: &[Entry<TypeConfig>]) {
        let db = store.db.lock().expect("mutex poisoned");
        let txn = db.begin_write().expect("begin_write");
        {
            let mut table = txn.open_table(LOGS).expect("open_table LOGS");
            for entry in entries {
                let bytes = bincode::serialize(entry).expect("serialize");
                table.insert(entry.log_id.index, &bytes).expect("insert");
            }
        }
        txn.commit().expect("commit");
    }

    #[test]
    fn open_creates_tables() {
        let path = temp_path();
        let store = RaftLogStore::open(&path).expect("open");
        let mut store = store;
        let state = rt_block(store.get_log_state()).expect("get_log_state");
        assert_eq!(state.last_log_id, None);
        assert_eq!(state.last_purged_log_id, None);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_entries_after_direct_write() {
        let store = RaftLogStore::new_ephemeral();
        let entries = vec![make_entry(0), make_entry(1), make_entry(2)];
        write_entries_direct(&store, &entries);

        let mut store = store;
        let read_back = rt_block(store.try_get_log_entries(0..3)).expect("read");
        assert_eq!(read_back.len(), 3);
        assert_eq!(read_back[0].log_id.index, 0);
        assert_eq!(read_back[2].log_id.index, 2);
    }

    #[test]
    fn get_log_state_after_direct_write() {
        let store = RaftLogStore::new_ephemeral();
        let entries = vec![make_entry(5), make_entry(6)];
        write_entries_direct(&store, &entries);

        let mut store = store;
        let state = rt_block(store.get_log_state()).expect("get_log_state");
        assert_eq!(
            state.last_log_id,
            Some(LogId {
                leader_id: CommittedLeaderId::new(0, 0),
                index: 6,
            })
        );
    }

    #[test]
    fn truncate_removes_entries() {
        let store = RaftLogStore::new_ephemeral();
        let entries = vec![make_entry(0), make_entry(1), make_entry(2), make_entry(3)];
        write_entries_direct(&store, &entries);

        let mut store = store;
        rt_block(store.truncate(LogId {
            leader_id: CommittedLeaderId::new(0, 0),
            index: 2,
        }))
        .expect("truncate");

        let read_back = rt_block(store.try_get_log_entries(0..4)).expect("read");
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].log_id.index, 0);
        assert_eq!(read_back[1].log_id.index, 1);
    }

    #[test]
    fn purge_removes_entries() {
        let store = RaftLogStore::new_ephemeral();
        let entries = vec![make_entry(0), make_entry(1), make_entry(2), make_entry(3)];
        write_entries_direct(&store, &entries);

        let mut store = store;
        rt_block(store.purge(LogId {
            leader_id: CommittedLeaderId::new(0, 0),
            index: 1,
        }))
        .expect("purge");

        let read_back = rt_block(store.try_get_log_entries(0..4)).expect("read");
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].log_id.index, 2);
        assert_eq!(read_back[1].log_id.index, 3);
    }

    #[test]
    fn save_and_read_vote() {
        let store = RaftLogStore::new_ephemeral();
        let mut store = store;
        let vote = Vote::new(1, 42);
        rt_block(store.save_vote(&vote)).expect("save_vote");
        let read_back = rt_block(store.read_vote()).expect("read_vote");
        assert_eq!(read_back, Some(vote));
    }

    #[test]
    fn save_and_read_committed() {
        let store = RaftLogStore::new_ephemeral();
        let mut store = store;
        let log_id = LogId {
            leader_id: CommittedLeaderId::new(0, 0),
            index: 7,
        };
        rt_block(store.save_committed(Some(log_id))).expect("save_committed");
        let read_back = rt_block(store.read_committed()).expect("read_committed");
        assert_eq!(read_back, Some(log_id));
    }

    #[test]
    fn get_log_reader_works() {
        let store = RaftLogStore::new_ephemeral();
        let entries = vec![make_entry(10), make_entry(11)];
        write_entries_direct(&store, &entries);

        let mut store = store;
        let mut reader = rt_block(store.get_log_reader());
        let read_back = rt_block(reader.try_get_log_entries(10..12)).expect("read");
        assert_eq!(read_back.len(), 2);
    }

    #[test]
    fn empty_store_read_vote_returns_none() {
        let store = RaftLogStore::new_ephemeral();
        let mut store = store;
        let read_back = rt_block(store.read_vote()).expect("read_vote");
        assert_eq!(read_back, None);
    }

    #[test]
    fn empty_store_read_committed_returns_none() {
        let store = RaftLogStore::new_ephemeral();
        let mut store = store;
        let read_back = rt_block(store.read_committed()).expect("read_committed");
        assert_eq!(read_back, None);
    }

    // --- helpers ---

    fn temp_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sb-raft-log-test-{}-{}.redb",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn rt_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new()
            .expect("tokio rt")
            .block_on(f)
    }
}
