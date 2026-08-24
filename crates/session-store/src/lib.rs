//! M07 — Session store.
//!
//! MVP scope: persist topology mutations to a `redb` write-ahead log and replay
//! them on restart into an in-memory [`TopologySnapshot`]. A 64-slot tokio
//! broadcast channel fans [`TopologyEvent`]s out to async consumers
//! (`control-api`, the `sonicbrew` binary). The `openraft` single-node
//! self-leader is deferred to P1 (see `BUILD-PLAN.md` §2.1).
//!
//! # Why a broadcast channel instead of `audio-graph-bsd`'s native subscriber?
//!
//! `audio-graph-bsd` ships a blocking `std::sync::mpsc`
//! `subscribe_topology` (behind its `topology` feature). The session-store
//! consumers are async, so [`SessionStore::subscribe`] yields a tokio broadcast
//! receiver instead. A forwarder task drains the sync mpsc into this broadcast
//! channel in the full implementation (BUILD-PLAN §2.1).

pub mod distributed;
pub mod raft_log_store;
pub mod raft_network;
pub mod raft_state_machine;
pub mod raft_types;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use redb::{ReadableTable, TableDefinition};
use tokio::sync::broadcast;

pub use audio_graph_bsd::{LinkId, Mutation, NodeId, TopologyEvent, TopologySnapshot};

/// Monotonic identifier assigned to each applied mutation.
pub type MutationId = u64;

/// redb table mapping mutation index -> serialized [`Mutation`].
///
/// The key is the mutation's [`MutationId`]; redb orders `u64` keys in
/// ascending numeric order, so iterating the table replays mutations strictly
/// in append order.
const MUTATIONS: TableDefinition<u64, Vec<u8>> = TableDefinition::new("mutations");

/// Errors returned by the session store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Operation has not been implemented yet (stub).
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    /// `redb` WAL persistence failure.
    #[error("persistence error: {0}")]
    Persistence(String),
    /// A mutation was rejected as invalid.
    #[error("invalid mutation: {0}")]
    InvalidMutation(String),
}

/// Convenience `Result` alias for the session store.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Read/write view over the live audio-graph topology.
///
/// Consumers (`control-api`, the `sonicbrew` binary) are async, so
/// [`SessionStore::subscribe`] yields a tokio broadcast receiver rather than
/// the blocking `std::sync::mpsc` receiver that `audio-graph-bsd`
/// `subscribe_topology` returns natively. A forwarder task drains the sync
/// mpsc into this broadcast channel in the full implementation.
pub trait SessionStore: Send + Sync {
    /// Return a point-in-time snapshot of the current topology.
    fn get_topology(&self) -> TopologySnapshot;
    /// Apply a mutation and return its assigned id.
    fn apply_mutation(&self, mutation: Mutation) -> Result<MutationId>;
    /// Subscribe to future topology events.
    fn subscribe(&self) -> broadcast::Receiver<TopologyEvent>;
}

/// Maps a [`Mutation`] to the [`TopologyEvent`] that describes the same change.
pub fn mutation_to_event_pub(mutation: &Mutation) -> TopologyEvent {
    mutation_to_event(mutation)
}

/// Maps a [`Mutation`] to the [`TopologyEvent`] that describes the same change.
fn mutation_to_event(mutation: &Mutation) -> TopologyEvent {
    match mutation {
        Mutation::AddNode(node) => TopologyEvent::NodeAdded(node.clone()),
        Mutation::RemoveNode(id) => TopologyEvent::NodeRemoved(*id),
        Mutation::AddLink(edge) => TopologyEvent::LinkAdded(*edge),
        Mutation::RemoveLink(id) => TopologyEvent::LinkRemoved(*id),
    }
}

/// Internal engine state guarded by a `std::sync::Mutex`.
struct EngineState {
    snapshot: TopologySnapshot,
    next_id: MutationId,
    db: redb::Database,
    tx: broadcast::Sender<TopologyEvent>,
}

/// MVP session-store engine: an in-memory [`TopologySnapshot`] backed by a
/// `redb` write-ahead log of [`Mutation`]s, plus a 64-slot [`TopologyEvent`]
/// broadcast channel.
///
/// Mutations are appended to the WAL *before* the in-memory snapshot is
/// touched, so a process restart replays the log to rebuild the exact same
/// topology (WAL semantics). The `openraft` single-node self-leader is deferred
/// to P1; for the MVP a single node is its own authoritative log.
///
/// Open via [`RaftEngine::open`] for a caller-managed (restart-safe) path, or
/// [`RaftEngine::new`] / [`RaftEngine::default`] for an ephemeral temp file
/// that is removed when the engine is dropped.
pub struct RaftEngine {
    inner: Mutex<EngineState>,
    /// When set, this engine owns an ephemeral temp DB file removed on drop.
    ephemeral_path: Option<PathBuf>,
}

impl RaftEngine {
    /// Open (or create) the engine at `path`, replaying any existing WAL.
    ///
    /// `redb::Database::create` initializes a fresh file when none exists and
    /// opens an existing one otherwise, so this is safe to call both the first
    /// time and after a restart.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = redb::Database::create(path.as_ref())
            .map_err(|e| StoreError::Persistence(format!("open redb: {e}")))?;
        // Ensure the mutations table exists (idempotent on reopen).
        {
            let txn = db
                .begin_write()
                .map_err(|e| StoreError::Persistence(format!("begin write: {e}")))?;
            {
                let _table = txn
                    .open_table(MUTATIONS)
                    .map_err(|e| StoreError::Persistence(format!("create table: {e}")))?;
            }
            txn.commit()
                .map_err(|e| StoreError::Persistence(format!("commit table create: {e}")))?;
        }
        // Replay the WAL into a fresh in-memory snapshot, in append order.
        let mut snapshot = TopologySnapshot::new();
        let mut next_id: MutationId = 0;
        {
            let txn = db
                .begin_read()
                .map_err(|e| StoreError::Persistence(format!("begin read: {e}")))?;
            let table = txn
                .open_table(MUTATIONS)
                .map_err(|e| StoreError::Persistence(format!("open table: {e}")))?;
            for entry in table
                .iter()
                .map_err(|e| StoreError::Persistence(format!("iter table: {e}")))?
            {
                let (_key, value) =
                    entry.map_err(|e| StoreError::Persistence(format!("read entry: {e}")))?;
                let bytes: Vec<u8> = value.value();
                let mutation: Mutation = serde_json::from_slice(&bytes)
                    .map_err(|e| StoreError::Persistence(format!("deserialize mutation: {e}")))?;
                snapshot.apply(&mutation);
                next_id += 1;
            }
        }
        let (tx, _rx) = broadcast::channel(64);
        // The seed receiver is dropped immediately: subscribers obtain fresh
        // receivers via `subscribe()`, and the channel lives as long as `tx`.
        drop(_rx);
        Ok(Self {
            inner: Mutex::new(EngineState {
                snapshot,
                next_id,
                db,
                tx,
            }),
            ephemeral_path: None,
        })
    }

    /// Create an engine backed by a unique ephemeral temp file.
    ///
    /// Convenience constructor for tests / ad-hoc use. The temp file is removed
    /// when the engine is dropped, so this is **not** restart-safe — use
    /// [`RaftEngine::open`] with a fixed path for persistence across restarts.
    #[must_use]
    pub fn new() -> Self {
        let path = unique_temp_path();
        let mut engine = Self::open(&path).unwrap_or_else(|e| {
            panic!(
                "session-store: ephemeral temp db at {} failed: {e}",
                path.display()
            )
        });
        engine.ephemeral_path = Some(path);
        engine
    }
}

impl Default for RaftEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RaftEngine {
    fn drop(&mut self) {
        // Only ephemeral (new/default) engines own their temp file. Engines
        // opened via `open` leave the file in place so a later reopen can
        // restore the topology from the WAL. On Unix unlinking an open file is
        // fine; the `redb::Database` inside `inner` is dropped after this body.
        if let Some(path) = &self.ephemeral_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl SessionStore for RaftEngine {
    fn get_topology(&self) -> TopologySnapshot {
        let state = self.inner.lock().expect("engine mutex poisoned");
        state.snapshot.clone()
    }

    fn apply_mutation(&self, mutation: Mutation) -> Result<MutationId> {
        let mut state = self.inner.lock().expect("engine mutex poisoned");
        let id = state.next_id;
        // WAL semantics: persist first, then mutate the in-memory snapshot so a
        // persistence failure never leaves the snapshot ahead of the log.
        let bytes = serde_json::to_vec(&mutation)
            .map_err(|e| StoreError::Persistence(format!("serialize mutation {id}: {e}")))?;
        {
            let txn = state
                .db
                .begin_write()
                .map_err(|e| StoreError::Persistence(format!("begin write: {e}")))?;
            {
                let mut table = txn
                    .open_table(MUTATIONS)
                    .map_err(|e| StoreError::Persistence(format!("open table: {e}")))?;
                table
                    .insert(id, &bytes)
                    .map_err(|e| StoreError::Persistence(format!("insert mutation {id}: {e}")))?;
            }
            txn.commit()
                .map_err(|e| StoreError::Persistence(format!("commit mutation {id}: {e}")))?;
        }
        state.snapshot.apply(&mutation);
        state.next_id = id + 1;
        // Fan out to any subscribers; a send error (no receivers) is benign.
        let _ = state.tx.send(mutation_to_event(&mutation));
        Ok(id)
    }

    fn subscribe(&self) -> broadcast::Receiver<TopologyEvent> {
        let state = self.inner.lock().expect("engine mutex poisoned");
        state.tx.subscribe()
    }
}

/// Builds a unique temp path under the OS temp dir (used by `new`/`default`).
fn unique_temp_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "sb-session-store-{}-{}.redb",
        std::process::id(),
        n
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_graph_bsd::{NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge};

    /// Minimal node snapshot: one mono input and one stereo output.
    fn sample_node(id: usize) -> NodeSnapshot {
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

    /// Unique, pre-cleaned temp path for one test DB.
    fn temp_db_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("sb-m07-test-{}-{}.redb", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        path
    }

    // --- Mutation -> TopologyEvent mapping ---

    #[test]
    fn maps_add_node_to_node_added() {
        let node = sample_node(1);
        assert_eq!(
            mutation_to_event(&Mutation::AddNode(node.clone())),
            TopologyEvent::NodeAdded(node)
        );
    }

    #[test]
    fn maps_remove_node_to_node_removed() {
        assert_eq!(
            mutation_to_event(&Mutation::RemoveNode(5)),
            TopologyEvent::NodeRemoved(5)
        );
    }

    #[test]
    fn maps_add_link_to_link_added() {
        let edge = SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        };
        assert_eq!(
            mutation_to_event(&Mutation::AddLink(edge)),
            TopologyEvent::LinkAdded(edge)
        );
    }

    #[test]
    fn maps_remove_link_to_link_removed() {
        assert_eq!(
            mutation_to_event(&Mutation::RemoveLink(2)),
            TopologyEvent::LinkRemoved(2)
        );
    }

    // --- MVP acceptance criterion 1: apply then get reflects ---

    #[test]
    fn apply_then_get_reflects() {
        let path = temp_db_path();
        let engine = RaftEngine::open(&path).expect("open engine");
        let id = engine
            .apply_mutation(Mutation::AddNode(sample_node(7)))
            .expect("apply mutation");
        assert_eq!(id, 0, "first mutation gets id 0");
        let topo = engine.get_topology();
        assert!(topo.node(7).is_some(), "node 7 present after AddNode");
        assert_eq!(topo.nodes.len(), 1);
        drop(engine);
        let _ = std::fs::remove_file(&path);
    }

    // --- MVP acceptance criterion 2: restart restores from WAL ---

    #[test]
    fn restart_restores_from_wal() {
        let path = temp_db_path();
        {
            let engine = RaftEngine::open(&path).expect("open first time");
            engine
                .apply_mutation(Mutation::AddNode(sample_node(3)))
                .expect("apply node 3");
            engine
                .apply_mutation(Mutation::AddNode(sample_node(4)))
                .expect("apply node 4");
        }
        // Simulate a restart: drop the engine, reopen the same path.
        let engine = RaftEngine::open(&path).expect("reopen after restart");
        let topo = engine.get_topology();
        assert_eq!(topo.nodes.len(), 2, "both nodes restored from WAL");
        assert!(topo.node(3).is_some());
        assert!(topo.node(4).is_some());
        // The mutation id counter continues past the replayed log.
        let next = engine
            .apply_mutation(Mutation::AddNode(sample_node(5)))
            .expect("apply after reopen");
        assert_eq!(next, 2, "next id continues from replayed count");
        drop(engine);
        let _ = std::fs::remove_file(&path);
    }

    // --- MVP acceptance criterion 3: subscribe receives an event ---

    #[test]
    fn subscribe_receives_event() {
        let path = temp_db_path();
        let engine = RaftEngine::open(&path).expect("open engine");
        let mut rx = engine.subscribe();
        let node = sample_node(11);
        engine
            .apply_mutation(Mutation::AddNode(node.clone()))
            .expect("apply mutation");
        let event = rx
            .try_recv()
            .expect("subscriber should receive the NodeAdded event");
        assert_eq!(event, TopologyEvent::NodeAdded(node));
        drop(engine);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_engine_is_usable() {
        // Ephemeral temp DB, cleaned up on drop via `ephemeral_path`.
        let engine = RaftEngine::default();
        assert!(engine.get_topology().nodes.is_empty());
        assert_eq!(
            engine
                .apply_mutation(Mutation::AddNode(sample_node(1)))
                .expect("apply"),
            0
        );
        assert!(engine.get_topology().node(1).is_some());
    }
}
