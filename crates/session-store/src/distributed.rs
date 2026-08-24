//! [`DistributedRaftEngine`] — the multi-node `SessionStore` backed by openraft.
//!
//! This is the P1 distributed engine from [ADR-0003]. It implements the
//! synchronous [`SessionStore`] trait (the same contract `control-api` consumes
//! via `Arc<dyn SessionStore>`) while delegating durable, replicated writes to
//! an openraft [`Raft`] core running on a tokio runtime.
//!
//! # Sync↔async bridge
//!
//! openraft is fully async; the public [`SessionStore`] trait is synchronous.
//! [`DistributedRaftEngine::apply_mutation`] bridges the two by **spawning**
//! the `client_write` future on the tokio `Handle` and **blocking** the caller
//! on a `std::sync::mpsc` receiver. Blocking on a std channel (not
//! `Handle::block_on`) is safe even when the caller is *itself* on the tokio
//! runtime — it does not attempt to drive the reactor from a worker thread.
//!
//! # Topology reads
//!
//! [`SessionStore::get_topology`] reads the applied topology straight from the
//! `redb` database shared with the state machine (via [`StateMachineReader`]).
//! This observes the latest *applied* state on the local node without round
//! tripping through the async Raft core, so it is both fast and consistent with
//! what openraft has committed+applied locally.
//!
//! # Cluster bootstrap
//!
//! [`spawn_cluster`] creates an N-node in-process cluster (separate `redb`
//! files per node, shared in-memory [`LoopbackNetworkFactory`]) and bootstraps
//! it with a single `Raft::initialize` call on the first node. It is the test
//! entry point for the integration suite (ADR-0003 §6, TESTING-STANDARDS
//! Layer 1/Concurrency).

use std::collections::BTreeMap;
use std::sync::Arc;

use openraft::Config;
use openraft::Raft;
use tokio::runtime::Handle;
use tokio::sync::broadcast;

use audio_graph_bsd::{Mutation, TopologyEvent, TopologySnapshot};

use crate::raft_log_store::RaftLogStore;
use crate::raft_network::LoopbackNetworkFactory;
use crate::raft_state_machine::{StateMachine, StateMachineReader};
use crate::raft_types::TypeConfig;
use crate::{MutationId, Result, SessionStore, StoreError};
/// Multi-node `SessionStore` backed by openraft (ADR-0003 P1).
///
/// Holds an `Arc<Raft<TypeConfig>>`, a tokio `Handle` for spawning async work,
/// a [`StateMachineReader`] for synchronous topology reads, and a
/// `broadcast::Sender<TopologyEvent>` for the `subscribe` contract.
///
/// This is a **distinct type** from the single-node [`crate::RaftEngine`];
/// reverting to single-node operation is simply not constructing this type.
pub struct DistributedRaftEngine {
    raft: Arc<Raft<TypeConfig>>,
    handle: Handle,
    reader: StateMachineReader,
    tx: broadcast::Sender<TopologyEvent>,
}

impl DistributedRaftEngine {
    /// Wrap an existing, initialized Raft node.
    ///
    /// `reader` must share the `redb` database of the state machine passed to
    /// `Raft::new` (obtain it via [`StateMachine::reader`] *before* moving the
    /// state machine into `Raft::new`). `handle` is the tokio runtime handle
    /// that owns the Raft core task.
    #[must_use]
    pub fn new(raft: Arc<Raft<TypeConfig>>, handle: Handle, reader: StateMachineReader) -> Self {
        let (tx, _rx) = broadcast::channel(64);
        drop(_rx);
        Self {
            raft,
            handle,
            reader,
            tx,
        }
    }

    /// Borrow the underlying Raft handle (for metrics / status inspection).
    pub fn raft(&self) -> &Raft<TypeConfig> {
        &self.raft
    }
}

impl SessionStore for DistributedRaftEngine {
    fn get_topology(&self) -> TopologySnapshot {
        // Synchronous redb read — no async, no runtime involvement.
        self.reader.topology().unwrap_or_default()
    }

    fn apply_mutation(&self, mutation: Mutation) -> Result<MutationId> {
        // Clone the mutation for the event fan-out before it is moved into the
        // spawned task.
        let for_event = mutation.clone();
        let raft = self.raft.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::result::Result<MutationId, String>>(1);
        self.handle.spawn(async move {
            // openraft's ClientWriteResponse<C> wraps our TypeConfig::R in
            // `.data`; extract the mutation_id there.
            let res = raft.client_write(mutation).await;
            let _ = tx.send(res.map(|r| r.data.mutation_id).map_err(|e| format!("{e}")));
        });
        let mapped = rx
            .recv()
            .map_err(|_| StoreError::Persistence("raft client_write task dropped".to_string()))?;
        let mutation_id =
            mapped.map_err(|e| StoreError::Persistence(format!("raft client_write: {e}")))?;
        // Fan out a best-effort event. A send error (no receivers) is benign.
        let event = crate::mutation_to_event_pub(&for_event);
        let _ = self.tx.send(event);
        Ok(mutation_id)
    }

    fn subscribe(&self) -> broadcast::Receiver<TopologyEvent> {
        self.tx.subscribe()
    }
}

// ---------------------------------------------------------------------------
// Cluster bootstrap (test + integration entry point)
// ---------------------------------------------------------------------------

/// A node's identity in a bootstrapped cluster: its Raft handle plus a
/// [`StateMachineReader`] sharing that node's `redb` database.
pub struct ClusterNode {
    pub id: u64,
    pub raft: Arc<Raft<TypeConfig>>,
    pub reader: StateMachineReader,
}

/// Spawn an N-node in-process Raft cluster backed by ephemeral `redb` files
/// and a shared [`LoopbackNetworkFactory`], then bootstrap it.
///
/// `node_ids` are the voter ids (e.g. `[1, 2, 3]`). Each node gets its own
/// log store and state machine on a unique temp file. The cluster is
/// initialized with a single `Raft::initialize(members)` call on the first
/// node; the rest learn the membership through the Raft protocol.
///
/// Returns the nodes in the same order as `node_ids`.
///
/// # Panics
/// Panics if `node_ids` is empty or Raft construction / initialization fails.
pub async fn spawn_cluster(node_ids: &[u64]) -> Vec<ClusterNode> {
    assert!(
        !node_ids.is_empty(),
        "spawn_cluster: need at least one node"
    );

    // Short, deterministic-ish election timeouts so tests converge quickly.
    let config = Config {
        election_timeout_min: 80,
        election_timeout_max: 120,
        heartbeat_interval: 20,
        ..Default::default()
    };
    config.clone().validate().expect("raft config valid");

    let config = Arc::new(config);
    let factory = LoopbackNetworkFactory::new();
    let mut nodes = Vec::with_capacity(node_ids.len());

    for &id in node_ids {
        let log_store = RaftLogStore::new_ephemeral();
        let state_machine = StateMachine::new_ephemeral();
        let reader = state_machine.reader();
        let raft = Raft::new(
            id,
            config.clone(),
            factory.clone(),
            log_store,
            state_machine,
        )
        .await
        .expect("Raft::new");
        let raft = Arc::new(raft);
        factory.register(id, raft.clone());
        nodes.push(ClusterNode { id, raft, reader });
    }

    // Bootstrap: initialize the first node with the full voter set.
    let members: BTreeMap<u64, openraft::BasicNode> = node_ids
        .iter()
        .map(|&id| (id, openraft::BasicNode::new(format!("inproc://{id}"))))
        .collect();
    nodes[0]
        .raft
        .initialize(members)
        .await
        .expect("Raft::initialize");

    nodes
}
