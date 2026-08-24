//! In-process loopback `RaftNetwork` for multi-node Raft testing (ADR-0003 §5).
//!
//! Production transport (TCP + mTLS) is a **separate future ADR**; this module
//! is test-only. The network delivers RPCs directly to the target node's
//! [`Raft`] handle via a shared registry, avoiding any real socket I/O.
//!
//! # Usage
//!
//! 1. Create a single [`LoopbackNetworkFactory`] shared by all nodes.
//! 2. Construct each `Raft<TypeConfig>` with this factory.
//! 3. After constructing each Raft, call [`LoopbackNetworkFactory::register`]
//!    to insert its `Arc<Raft>` into the shared registry.
//! 4. RPCs issued by one node's Raft are delivered synchronously to the
//!    target node's Raft via the registry lookup.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use openraft::error::InstallSnapshotError;
use openraft::error::RPCError;
use openraft::error::RaftError;
use openraft::error::RemoteError;
use openraft::error::Unreachable;
use openraft::network::RPCOption;
use openraft::network::RaftNetwork;
use openraft::network::RaftNetworkFactory;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::InstallSnapshotResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;
use openraft::Raft;

use crate::raft_types::TypeConfig;

/// Shared, thread-safe map of `NodeId -> Arc<Raft<TypeConfig>>`.
type Registry = Arc<Mutex<HashMap<u64, Arc<Raft<TypeConfig>>>>>;

/// Factory for [`LoopbackNetwork`] instances.
///
/// Cloned cheaply (the registry is behind an `Arc`). All nodes in a test
/// cluster share the *same* factory (and hence the same registry) so that RPCs
/// issued to a target resolve to that target's registered Raft handle.
#[derive(Clone, Default)]
pub struct LoopbackNetworkFactory {
    registry: Registry,
}

impl LoopbackNetworkFactory {
    /// Create a new empty factory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a Raft handle so other nodes can send RPCs to it.
    ///
    /// Call this once per node **after** constructing the `Raft` instance.
    pub fn register(&self, id: u64, raft: Arc<Raft<TypeConfig>>) {
        let mut reg = self
            .registry
            .lock()
            .expect("network registry mutex poisoned");
        reg.insert(id, raft);
    }

    /// Remove a node from the registry (used to simulate a node leaving /
    /// crashing in failover tests).
    pub fn deregister(&self, id: u64) {
        let mut reg = self
            .registry
            .lock()
            .expect("network registry mutex poisoned");
        reg.remove(&id);
    }
}

/// A single connection to a target node, backed by the shared registry.
pub struct LoopbackNetwork {
    target: u64,
    registry: Registry,
}

impl LoopbackNetwork {
    /// Look up the target's Raft handle, or return `Unreachable` if it is not
    /// registered (e.g. crashed / removed).
    fn lookup(&self) -> Result<Arc<Raft<TypeConfig>>, Unreachable> {
        let reg = self
            .registry
            .lock()
            .expect("network registry mutex poisoned");
        reg.get(&self.target).cloned().ok_or_else(|| {
            let io = std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("node {} not registered", self.target),
            );
            Unreachable::new(&io)
        })
    }
}

impl RaftNetworkFactory<TypeConfig> for LoopbackNetworkFactory {
    type Network = LoopbackNetwork;

    async fn new_client(&mut self, target: u64, _node: &openraft::BasicNode) -> Self::Network {
        LoopbackNetwork {
            target,
            registry: self.registry.clone(),
        }
    }
}

impl RaftNetwork<TypeConfig> for LoopbackNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, openraft::BasicNode, RaftError<u64>>>
    {
        let raft = self.lookup().map_err(RPCError::Unreachable)?;
        raft.append_entries(rpc)
            .await
            .map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, openraft::BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        let raft = self.lookup().map_err(RPCError::Unreachable)?;
        raft.install_snapshot(rpc)
            .await
            .map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, openraft::BasicNode, RaftError<u64>>> {
        let raft = self.lookup().map_err(RPCError::Unreachable)?;
        raft.vote(rpc)
            .await
            .map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }
}
