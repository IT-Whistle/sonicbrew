//! openraft `RaftStateMachine` + `RaftSnapshotBuilder` backed by `redb`.
//!
//! This module implements the state-machine component of openraft's storage
//! interface. Applied `Mutation` entries fold into an
//! [`TopologySnapshot`] (the state machine), and snapshots are serialized
//! snapshots of that topology plus openraft metadata.
//!
//! # Schema
//!
//! | Table     | Key | Value                                            | Purpose              |
//! |-----------|-----|--------------------------------------------------|----------------------|
//! | `STATE`   | `()`| bincode [`PersistedState`]                       | Live state machine   |
//! | `SNAPSHOT`| `()`| bincode [`PersistedSnapshot`]                    | Current snapshot     |
//!
//! # Threading
//!
//! As with [`crate::raft_log_store`], `redb` is synchronous I/O. The internal
//! `Mutex<redb::Database>` is never held across an `.await` boundary — every
//! async trait method locks, performs synchronous redb operations, and drops
//! the lock before returning or awaiting.

// `openraft::StorageError` is ~224 bytes by design; we cannot shrink it. This
// matches the suppression in `raft_log_store.rs`.
#![allow(clippy::result_large_err)]

use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use openraft::storage::RaftSnapshotBuilder;
use openraft::storage::RaftStateMachine;
use openraft::storage::Snapshot;
use openraft::EntryPayload;
use openraft::LogId;
use openraft::SnapshotMeta;
use openraft::StorageError;
use openraft::StoredMembership;
use redb::{ReadableTable, TableDefinition};

use audio_graph_bsd::TopologySnapshot;

use crate::raft_types::{ClientWriteResponse, TypeConfig};

/// redb table for the live state machine: single row () -> bincode `PersistedState`.
const STATE: TableDefinition<(), Vec<u8>> = TableDefinition::new("raft_sm_state");

/// redb table for the current snapshot: single row () -> bincode `PersistedSnapshot`.
const SNAPSHOT: TableDefinition<(), Vec<u8>> = TableDefinition::new("raft_sm_snapshot");

/// The persisted state machine: topology + raft pointers + mutation counter.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct PersistedState {
    topology: TopologySnapshot,
    last_applied: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, openraft::BasicNode>,
    next_mutation_id: u64,
}

/// The persisted snapshot: openraft metadata + serialized topology payload.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PersistedSnapshot {
    meta: SnapshotMeta<u64, openraft::BasicNode>,
    /// bincode-encoded [`TopologySnapshot`] payload.
    payload: Vec<u8>,
}

/// Convert a redb/std error into a `StorageError` for the openraft API.
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

/// openraft state machine backed by `redb`.
///
/// Holds the applied [`TopologySnapshot`], the last applied `LogId`, the last
/// applied membership, and a monotonic mutation counter. All of these are
/// persisted to the `STATE` table before `apply` returns (durable state
/// machine, per openraft's persistent-state-machine option).
pub struct StateMachine {
    db: Arc<Mutex<redb::Database>>,
    /// When set, this engine owns an ephemeral temp DB removed on drop.
    ephemeral_path: Option<PathBuf>,
}

impl StateMachine {
    /// Open (or create) the state machine at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError<u64>> {
        let db = redb::Database::create(path.as_ref())
            .map_err(|e| io_error("open redb", openraft::ErrorVerb::Write, e))?;
        // Ensure both tables exist and seed STATE if empty.
        {
            let txn = db
                .begin_write()
                .map_err(|e| io_error("begin_write table init", openraft::ErrorVerb::Write, e))?;
            {
                let mut state_tbl = txn
                    .open_table(STATE)
                    .map_err(|e| io_error("open STATE", openraft::ErrorVerb::Write, e))?;
                if state_tbl
                    .get(())
                    .map_err(|e| io_error("get STATE", openraft::ErrorVerb::Read, e))?
                    .is_none()
                {
                    let init = PersistedState::default();
                    let bytes = bincode::serialize(&init).map_err(|e| {
                        io_error("serialize init state", openraft::ErrorVerb::Write, e)
                    })?;
                    state_tbl.insert((), &bytes).map_err(|e| {
                        io_error("insert init STATE", openraft::ErrorVerb::Write, e)
                    })?;
                }
                let _snapshot_tbl = txn
                    .open_table(SNAPSHOT)
                    .map_err(|e| io_error("open SNAPSHOT", openraft::ErrorVerb::Write, e))?;
            }
            txn.commit()
                .map_err(|e| io_error("commit table init", openraft::ErrorVerb::Write, e))?;
        }
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            ephemeral_path: None,
        })
    }

    /// Create a state machine backed by a unique ephemeral temp file.
    pub fn new_ephemeral() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("sb-raft-sm-{}-{}.redb", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        let mut sm = Self::open(&path).unwrap_or_else(|e| {
            panic!(
                "raft_state_machine: ephemeral temp db at {} failed: {e}",
                path.display()
            )
        });
        sm.ephemeral_path = Some(path);
        sm
    }

    /// Read the current persisted state.
    fn read_state(&self) -> Result<PersistedState, StorageError<u64>> {
        let db = self.db.lock().expect("sm mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read STATE", openraft::ErrorVerb::Read, e))?;
        let table = txn
            .open_table(STATE)
            .map_err(|e| io_error("open_table STATE", openraft::ErrorVerb::Read, e))?;
        let row = table
            .get(())
            .map_err(|e| io_error("get STATE", openraft::ErrorVerb::Read, e))?;
        let bytes: Vec<u8> = row
            .ok_or_else(|| {
                io_error(
                    "STATE row missing",
                    openraft::ErrorVerb::Read,
                    std::io::Error::new(std::io::ErrorKind::NotFound, "STATE row missing"),
                )
            })?
            .value();
        bincode::deserialize(&bytes)
            .map_err(|e| io_error("deserialize STATE", openraft::ErrorVerb::Read, e))
    }

    /// Overwrite the persisted state (durable: commits before returning).
    fn write_state(&self, state: &PersistedState) -> Result<(), StorageError<u64>> {
        let db = self.db.lock().expect("sm mutex poisoned");
        let bytes = bincode::serialize(state)
            .map_err(|e| io_error("serialize STATE", openraft::ErrorVerb::Write, e))?;
        let txn = db
            .begin_write()
            .map_err(|e| io_error("begin_write STATE", openraft::ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(STATE)
                .map_err(|e| io_error("open_table STATE", openraft::ErrorVerb::Write, e))?;
            table
                .insert((), &bytes)
                .map_err(|e| io_error("insert STATE", openraft::ErrorVerb::Write, e))?;
        }
        txn.commit()
            .map_err(|e| io_error("commit STATE", openraft::ErrorVerb::Write, e))
    }

    /// Read the current persisted snapshot, if any.
    fn read_snapshot(&self) -> Result<Option<PersistedSnapshot>, StorageError<u64>> {
        let db = self.db.lock().expect("sm mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read SNAPSHOT", openraft::ErrorVerb::Read, e))?;
        let table = txn
            .open_table(SNAPSHOT)
            .map_err(|e| io_error("open_table SNAPSHOT", openraft::ErrorVerb::Read, e))?;
        let row = table
            .get(())
            .map_err(|e| io_error("get SNAPSHOT", openraft::ErrorVerb::Read, e))?;
        match row {
            None => Ok(None),
            Some(guard) => {
                let bytes: Vec<u8> = guard.value();
                let snap: PersistedSnapshot = bincode::deserialize(&bytes)
                    .map_err(|e| io_error("deserialize SNAPSHOT", openraft::ErrorVerb::Read, e))?;
                Ok(Some(snap))
            }
        }
    }

    /// Overwrite the persisted snapshot (durable: commits before returning).
    fn write_snapshot(&self, snap: &PersistedSnapshot) -> Result<(), StorageError<u64>> {
        let db = self.db.lock().expect("sm mutex poisoned");
        let bytes = bincode::serialize(snap)
            .map_err(|e| io_error("serialize SNAPSHOT", openraft::ErrorVerb::Write, e))?;
        let txn = db
            .begin_write()
            .map_err(|e| io_error("begin_write SNAPSHOT", openraft::ErrorVerb::Write, e))?;
        {
            let mut table = txn
                .open_table(SNAPSHOT)
                .map_err(|e| io_error("open_table SNAPSHOT", openraft::ErrorVerb::Write, e))?;
            table
                .insert((), &bytes)
                .map_err(|e| io_error("insert SNAPSHOT", openraft::ErrorVerb::Write, e))?;
        }
        txn.commit()
            .map_err(|e| io_error("commit SNAPSHOT", openraft::ErrorVerb::Write, e))
    }
}

impl Drop for StateMachine {
    fn drop(&mut self) {
        if let Some(path) = &self.ephemeral_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Clonable read-only handle to the state machine's `redb` database.
///
/// Created via [`StateMachine::reader`]. The underlying `redb::Database` is
/// shared (behind an `Arc<Mutex>`) with the [`StateMachine`] that was handed
/// to openraft, so reads observe the latest applied topology without going
/// through the async Raft core. This is how [`crate::DistributedRaftEngine`]
/// implements the synchronous `SessionStore::get_topology`.
#[derive(Clone)]
pub struct StateMachineReader {
    db: Arc<Mutex<redb::Database>>,
}

impl StateMachineReader {
    /// Read the current applied topology from the persisted state.
    ///
    /// Returns the [`TopologySnapshot`] held in the `STATE` table. This
    /// reflects every entry that openraft has applied (committed + applied to
    /// the state machine), on this node.
    pub fn topology(&self) -> Result<TopologySnapshot, StorageError<u64>> {
        let db = self.db.lock().expect("reader mutex poisoned");
        let txn = db
            .begin_read()
            .map_err(|e| io_error("begin_read reader", openraft::ErrorVerb::Read, e))?;
        let table = txn
            .open_table(STATE)
            .map_err(|e| io_error("open_table STATE reader", openraft::ErrorVerb::Read, e))?;
        let row = table
            .get(())
            .map_err(|e| io_error("get STATE reader", openraft::ErrorVerb::Read, e))?;
        match row {
            None => Ok(TopologySnapshot::default()),
            Some(guard) => {
                let bytes: Vec<u8> = guard.value();
                let state: PersistedState = bincode::deserialize(&bytes).map_err(|e| {
                    io_error("deserialize STATE reader", openraft::ErrorVerb::Read, e)
                })?;
                Ok(state.topology)
            }
        }
    }
}

impl StateMachine {
    /// Create a clonable read-only handle sharing this state machine's `redb`
    /// database. Call this **before** moving the `StateMachine` into
    /// `Raft::new`; the returned reader stays usable afterwards because it
    /// shares the `Arc<Mutex<redb::Database>>`.
    #[must_use]
    pub fn reader(&self) -> StateMachineReader {
        StateMachineReader {
            db: self.db.clone(),
        }
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotBuilder = SnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (
            Option<LogId<u64>>,
            StoredMembership<u64, openraft::BasicNode>,
        ),
        StorageError<u64>,
    > {
        let state = self.read_state()?;
        Ok((state.last_applied, state.last_membership))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<ClientWriteResponse>, StorageError<u64>>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + openraft::OptionalSend,
        I::IntoIter: openraft::OptionalSend,
    {
        let mut state = self.read_state()?;
        let mut responses = Vec::new();

        for entry in entries {
            // Advance last-applied pointer for every entry, regardless of payload.
            state.last_applied = Some(entry.log_id);
            match entry.payload {
                EntryPayload::Blank => {
                    responses.push(ClientWriteResponse::default());
                }
                EntryPayload::Normal(mutation) => {
                    let mutation_id = state.next_mutation_id;
                    state.next_mutation_id += 1;
                    state.topology.apply(&mutation);
                    responses.push(ClientWriteResponse { mutation_id });
                }
                EntryPayload::Membership(membership) => {
                    state.last_membership = StoredMembership::new(Some(entry.log_id), membership);
                    responses.push(ClientWriteResponse::default());
                }
            }
        }

        // Durable state machine: persist before returning.
        self.write_state(&state)?;
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SnapshotBuilder {
            db: self.db.clone(),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, openraft::BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        let payload = snapshot.into_inner();
        let topology: TopologySnapshot = bincode::deserialize(&payload)
            .map_err(|e| io_error("deserialize install payload", openraft::ErrorVerb::Read, e))?;

        // Replace the live state machine with the snapshot contents.
        let state = PersistedState {
            topology,
            last_applied: meta.last_log_id,
            last_membership: meta.last_membership.clone(),
            // Reset the mutation counter on snapshot install; the snapshot
            // subsumes all prior mutations.
            next_mutation_id: 0,
        };
        self.write_state(&state)?;

        // Record the snapshot as the current one.
        let persisted = PersistedSnapshot {
            meta: meta.clone(),
            payload,
        };
        self.write_snapshot(&persisted)
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<u64>> {
        let snap = self.read_snapshot()?;
        match snap {
            None => Ok(None),
            Some(persisted) => Ok(Some(Snapshot {
                meta: persisted.meta,
                snapshot: Box::new(Cursor::new(persisted.payload)),
            })),
        }
    }
}

/// Builds a snapshot from the current state machine.
pub struct SnapshotBuilder {
    db: Arc<Mutex<redb::Database>>,
}

impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<u64>> {
        // Read current state under the lock (sync), then drop the lock.
        let (topology, last_applied, last_membership) = {
            let db = self.db.lock().expect("snapshot builder mutex poisoned");
            let txn = db
                .begin_read()
                .map_err(|e| io_error("begin_read build_snapshot", openraft::ErrorVerb::Read, e))?;
            let table = txn
                .open_table(STATE)
                .map_err(|e| io_error("open_table STATE", openraft::ErrorVerb::Read, e))?;
            let row = table
                .get(())
                .map_err(|e| io_error("get STATE", openraft::ErrorVerb::Read, e))?;
            let bytes: Vec<u8> = row
                .ok_or_else(|| {
                    io_error(
                        "STATE missing",
                        openraft::ErrorVerb::Read,
                        std::io::Error::new(std::io::ErrorKind::NotFound, "STATE row missing"),
                    )
                })?
                .value();
            let state: PersistedState = bincode::deserialize(&bytes)
                .map_err(|e| io_error("deserialize STATE", openraft::ErrorVerb::Read, e))?;
            (state.topology, state.last_applied, state.last_membership)
        };

        let payload = bincode::serialize(&topology)
            .map_err(|e| io_error("serialize topology", openraft::ErrorVerb::Write, e))?;

        let meta = SnapshotMeta {
            last_log_id: last_applied,
            last_membership,
            snapshot_id: format!(
                "{}-{}",
                last_applied.map(|l| l.index).unwrap_or(0),
                payload.len()
            ),
        };

        // Persist the snapshot as current.
        let persisted = PersistedSnapshot {
            meta: meta.clone(),
            payload: payload.clone(),
        };
        {
            let db = self.db.lock().expect("snapshot builder mutex poisoned");
            let bytes = bincode::serialize(&persisted)
                .map_err(|e| io_error("serialize SNAPSHOT", openraft::ErrorVerb::Write, e))?;
            let txn = db
                .begin_write()
                .map_err(|e| io_error("begin_write SNAPSHOT", openraft::ErrorVerb::Write, e))?;
            {
                let mut table = txn
                    .open_table(SNAPSHOT)
                    .map_err(|e| io_error("open_table SNAPSHOT", openraft::ErrorVerb::Write, e))?;
                table
                    .insert((), &bytes)
                    .map_err(|e| io_error("insert SNAPSHOT", openraft::ErrorVerb::Write, e))?;
            }
            txn.commit()
                .map_err(|e| io_error("commit SNAPSHOT", openraft::ErrorVerb::Write, e))?;
        }

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(payload)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_graph_bsd::{Mutation, NodeSnapshot};
    use openraft::CommittedLeaderId;

    /// Minimal node snapshot for tests.
    fn sample_node(id: usize) -> audio_graph_bsd::NodeSnapshot {
        use audio_graph_bsd::{PortDir, PortMeta, SampleFmt};
        NodeSnapshot {
            id,
            inputs: vec![PortMeta {
                direction: PortDir::Input,
                channels: 1,
                sample_format: SampleFmt::F32,
            }],
            outputs: vec![PortMeta {
                direction: PortDir::Output,
                channels: 2,
                sample_format: SampleFmt::F32,
            }],
        }
    }

    fn log_id(term: u64, node_id: u64, index: u64) -> LogId<u64> {
        LogId::new(CommittedLeaderId::new(term, node_id), index)
    }

    fn normal_entry(
        term: u64,
        node_id: u64,
        index: u64,
        mutation: Mutation,
    ) -> openraft::Entry<TypeConfig> {
        openraft::Entry {
            log_id: log_id(term, node_id, index),
            payload: EntryPayload::Normal(mutation),
        }
    }

    fn blank_entry(term: u64, node_id: u64, index: u64) -> openraft::Entry<TypeConfig> {
        openraft::Entry {
            log_id: log_id(term, node_id, index),
            payload: EntryPayload::Blank,
        }
    }

    #[tokio::test]
    async fn apply_normal_mutation_updates_topology() {
        let mut sm = StateMachine::new_ephemeral();
        let entries = vec![normal_entry(1, 0, 1, Mutation::AddNode(sample_node(7)))];
        let resp = sm.apply(entries).await.expect("apply");
        assert_eq!(resp.len(), 1);
        assert_eq!(resp[0].mutation_id, 0);
        let (last_applied, _membership) = sm.applied_state().await.expect("applied_state");
        assert_eq!(last_applied, Some(log_id(1, 0, 1)));
        // Topology must reflect the applied node — read state directly.
        let state = sm.read_state().expect("read_state");
        assert!(
            state.topology.node(7).is_some(),
            "node 7 present after apply"
        );
    }

    #[tokio::test]
    async fn apply_blank_updates_last_applied_only() {
        let mut sm = StateMachine::new_ephemeral();
        sm.apply(vec![blank_entry(1, 0, 0)])
            .await
            .expect("apply blank");
        let (last_applied, _) = sm.applied_state().await.expect("applied_state");
        assert_eq!(last_applied, Some(log_id(1, 0, 0)));
        let state = sm.read_state().expect("read_state");
        assert!(
            state.topology.nodes.is_empty(),
            "topology unchanged by blank"
        );
    }

    #[tokio::test]
    async fn apply_membership_updates_membership() {
        use openraft::Membership;
        use std::collections::BTreeSet;
        let mut sm = StateMachine::new_ephemeral();
        let membership = Membership::new(vec![BTreeSet::from([0u64])], ());
        let entry = openraft::Entry::<TypeConfig> {
            log_id: log_id(1, 0, 1),
            payload: EntryPayload::Membership(membership),
        };
        sm.apply(vec![entry]).await.expect("apply membership");
        let (_last_applied, stored_membership) = sm.applied_state().await.expect("applied_state");
        assert_eq!(
            stored_membership.log_id(),
            &Some(log_id(1, 0, 1)),
            "membership log id recorded"
        );
    }

    #[tokio::test]
    async fn build_snapshot_then_get_current() {
        let mut sm = StateMachine::new_ephemeral();
        sm.apply(vec![normal_entry(
            1,
            0,
            1,
            Mutation::AddNode(sample_node(3)),
        )])
        .await
        .expect("apply");
        let mut builder = sm.get_snapshot_builder().await;
        let snap = builder.build_snapshot().await.expect("build_snapshot");
        assert_eq!(snap.meta.last_log_id, Some(log_id(1, 0, 1)));
        assert!(
            !snap.snapshot.get_ref().is_empty(),
            "snapshot payload non-empty"
        );
        // get_current_snapshot should now return it.
        let current = sm
            .get_current_snapshot()
            .await
            .expect("get_current")
            .expect("some snapshot");
        assert_eq!(current.meta.last_log_id, Some(log_id(1, 0, 1)));
    }

    #[tokio::test]
    async fn install_snapshot_replaces_state() {
        let mut sm = StateMachine::new_ephemeral();
        // Seed with one node, then install a snapshot with a different topology.
        sm.apply(vec![normal_entry(
            1,
            0,
            1,
            Mutation::AddNode(sample_node(1)),
        )])
        .await
        .expect("apply");

        let new_topo = {
            let mut t = TopologySnapshot::default();
            t.apply(&Mutation::AddNode(sample_node(42)));
            t.apply(&Mutation::AddNode(sample_node(43)));
            t
        };
        let payload = bincode::serialize(&new_topo).expect("serialize");
        let meta = SnapshotMeta {
            last_log_id: Some(log_id(2, 0, 5)),
            last_membership: StoredMembership::default(),
            snapshot_id: "test".to_string(),
        };
        sm.install_snapshot(&meta, Box::new(Cursor::new(payload)))
            .await
            .expect("install");
        let state = sm.read_state().expect("read_state");
        assert!(state.topology.node(42).is_some());
        assert!(state.topology.node(43).is_some());
        assert!(state.topology.node(1).is_none(), "old node replaced");
        assert_eq!(state.last_applied, Some(log_id(2, 0, 5)));
    }

    #[tokio::test]
    async fn restart_restores_state() {
        let path = std::env::temp_dir().join(format!(
            "sb-raft-sm-restart-{}-{}.redb",
            std::process::id(),
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        {
            let mut sm = StateMachine::open(&path).expect("open");
            sm.apply(vec![normal_entry(
                1,
                0,
                1,
                Mutation::AddNode(sample_node(9)),
            )])
            .await
            .expect("apply");
        }
        // Reopen — state must persist.
        let mut sm = StateMachine::open(&path).expect("reopen");
        let (last_applied, _) = sm.applied_state().await.expect("applied_state");
        assert_eq!(last_applied, Some(log_id(1, 0, 1)));
        let state = sm.read_state().expect("read_state");
        assert!(
            state.topology.node(9).is_some(),
            "node 9 restored after restart"
        );
        assert_eq!(state.next_mutation_id, 1, "mutation counter restored");
        drop(sm);
        let _ = std::fs::remove_file(&path);
    }
}
