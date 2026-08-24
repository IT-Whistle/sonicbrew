//! openraft type configuration for the sonicbrew session store (ADR-0003).
//!
//! This module wires `audio-graph-bsd`'s topology types into openraft's
//! [`RaftTypeConfig`], so that a [`Mutation`] becomes a Raft log entry and a
//! [`TopologySnapshot`] becomes the state-machine state. The single-node
//! self-leader MVP ([`crate::RaftEngine`]) is untouched; this module provides
//! the type-level foundation that the P1 multi-node `RaftLogStore` /
//! `RaftStateMachine` implementations will build upon.
//!
//! # Type choices
//!
//! | Associated type    | Concrete type                         | Rationale |
//! |--------------------|---------------------------------------|-----------|
//! | `D` (entry data)   | [`Mutation`]                          | Already `Serialize + Deserialize`; each committed entry is one topology mutation. |
//! | `R` (response)     | [`ClientWriteResponse`]               | Returns the assigned [`MutationId`] back to the client. |
//! | `NodeId`           | [`RaftNodeId`] (`u64`)                | Aliased to avoid a name clash with [`audio_graph_bsd::NodeId`]. |
//! | `Node`             | [`openraft::BasicNode`]               | Stores the peer's network address (`addr: String`). |
//! | `Entry`            | [`openraft::Entry`]` <Self>`          | openraft's default entry envelope. |
//! | `SnapshotData`     | [`Cursor`]` <`[`Vec`]`<u8>>`          | `bincode`-encoded [`TopologySnapshot`] stream. |
//! | `Responder`        | [`openraft::impls::OneshotResponder`] | openraft's default client-write responder. |
//! | `AsyncRuntime`     | [`openraft::TokioRuntime`]            | sonicbrew runs on tokio. |
//!
//! [`RaftTypeConfig`]: openraft::RaftTypeConfig
//! [`Mutation`]: audio_graph_bsd::Mutation
//! [`TopologySnapshot`]: audio_graph_bsd::TopologySnapshot
//! [`ClientWriteResponse`]: struct@ClientWriteResponse
//! [`MutationId`]: crate::MutationId
//! [`RaftNodeId`]: type@RaftNodeId
//! [`Cursor`]: std::io::Cursor
//! [`openraft::Entry`]: openraft::Entry
//! [`openraft::BasicNode`]: openraft::BasicNode
//! [`openraft::TokioRuntime`]: openraft::TokioRuntime
//! [`openraft::impls::OneshotResponder`]: openraft::impls::OneshotResponder

use std::io::Cursor;

use openraft::declare_raft_types;

use crate::MutationId;

/// Raft node identifier.
///
/// This is an alias for `u64`, introduced so that openraft's `NodeId` associated
/// type does not shadow [`audio_graph_bsd::NodeId`] (the *audio-graph* node
/// identifier) in modules that import both.
///
/// [`audio_graph_bsd::NodeId`]: https://docs.rs/audio-graph-bsd
pub type RaftNodeId = u64;

/// Response returned to a client after its [`Mutation`][crate::Mutation] is
/// committed and applied to the state machine.
///
/// Carries the [`MutationId`] assigned by the session store so callers can
/// correlate the committed entry with their local request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct ClientWriteResponse {
    /// The monotonic id assigned to the applied mutation.
    pub mutation_id: MutationId,
}

// ---------------------------------------------------------------------------
// RaftTypeConfig
// ---------------------------------------------------------------------------

declare_raft_types!(
    /// openraft type configuration binding sonicbrew's topology types into the
    /// Raft engine. See the [module docs][self] for the full type table.
    pub TypeConfig:
        D            = audio_graph_bsd::Mutation,
        R            = ClientWriteResponse,
        NodeId       = RaftNodeId,
        Node         = openraft::BasicNode,
        Entry        = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        Responder    = openraft::impls::OneshotResponder<TypeConfig>,
        AsyncRuntime = openraft::TokioRuntime,
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The `declare_raft_types!` macro must produce a unit struct that
    /// implements [`openraft::RaftTypeConfig`] and all its supertraits.
    #[test]
    fn type_config_implements_raft_type_config() {
        // Compile-time proof: if `TypeConfig` did not implement
        // `RaftTypeConfig`, this trait bound would fail to compile.
        fn assert_tc<T: openraft::RaftTypeConfig>() {}
        assert_tc::<TypeConfig>();
    }

    /// `RaftNodeId` is `u64` and satisfies openraft's `NodeId` trait
    /// (auto-implemented for `u64` via the blanket impl).
    #[test]
    fn raft_node_id_is_u64() {
        let id: RaftNodeId = 42;
        assert_eq!(id, 42u64);
    }

    /// `ClientWriteResponse` round-trips through serde so it can be serialized
    /// over the Raft responder channel.
    #[test]
    fn client_write_response_round_trips() {
        let resp = ClientWriteResponse { mutation_id: 7 };
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: ClientWriteResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(resp, back);
    }
}
