//! Property-based (proptest) tests for the session-store crate.
//!
//! Covers:
//! - (a) Mutation serde roundtrip — any `Mutation` serialises to JSON and
//!   deserialises back identically.
//! - (b) WAL replay idempotency — applying the same `Mutation` sequence to two
//!   fresh `RaftEngine` instances yields identical `TopologySnapshot`s.
//! - (c) `TopologySnapshot::apply` idempotency — re-applying an idempotent
//!   mutation that has already been applied does not change the snapshot.
//!   (`AddLink` is excluded because it is NOT idempotent.)

use audio_graph_bsd::{
    Mutation, NodeId, NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge, TopologySnapshot,
};
use proptest::prelude::*;
use session_store::{RaftEngine, SessionStore};

// ---------------------------------------------------------------------------
// proptest strategies
// ---------------------------------------------------------------------------

fn arb_port_dir() -> impl Strategy<Value = PortDir> {
    prop_oneof![Just(PortDir::Input), Just(PortDir::Output)]
}

fn arb_sample_fmt() -> impl Strategy<Value = SampleFmt> {
    prop_oneof![
        Just(SampleFmt::F32),
        Just(SampleFmt::F64),
        Just(SampleFmt::I16),
        Just(SampleFmt::I32),
    ]
}

fn arb_port_meta() -> impl Strategy<Value = PortMeta> {
    (arb_port_dir(), 1u16..=8u16, arb_sample_fmt()).prop_map(
        |(direction, channels, sample_format)| PortMeta {
            direction,
            channels,
            sample_format,
        },
    )
}

fn arb_node_snapshot() -> impl Strategy<Value = NodeSnapshot> {
    (
        any::<NodeId>(),
        prop::collection::vec(arb_port_meta(), 0..4),
        prop::collection::vec(arb_port_meta(), 0..4),
    )
        .prop_map(|(id, inputs, outputs)| NodeSnapshot {
            id,
            inputs,
            outputs,
        })
}

fn arb_snapshot_edge(max_nodes: usize) -> impl Strategy<Value = SnapshotEdge> {
    (0usize..max_nodes, 0usize..4, 0usize..max_nodes, 0usize..4).prop_map(
        |(from_node, from_port, to_node, to_port)| SnapshotEdge {
            from: (from_node, from_port),
            to: (to_node, to_port),
        },
    )
}

fn arb_mutation(max_nodes: usize) -> impl Strategy<Value = Mutation> {
    prop_oneof![
        arb_node_snapshot().prop_map(Mutation::AddNode),
        (0..max_nodes).prop_map(Mutation::RemoveNode),
        arb_snapshot_edge(max_nodes).prop_map(Mutation::AddLink),
        (0..max_nodes * 2).prop_map(Mutation::RemoveLink),
    ]
}

/// Idempotent mutations only: AddNode (replace), RemoveNode, RemoveLink.
/// `AddLink` is excluded because it is NOT idempotent (appends a new edge).
fn arb_idempotent_mutation(max_nodes: usize) -> impl Strategy<Value = Mutation> {
    prop_oneof![
        arb_node_snapshot().prop_map(Mutation::AddNode),
        (0..max_nodes).prop_map(Mutation::RemoveNode),
        (0..max_nodes * 2).prop_map(Mutation::RemoveLink),
    ]
}

// ---------------------------------------------------------------------------
// Property (a): Mutation serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn mutation_serde_roundtrip(m in arb_mutation(10)) {
        let json = serde_json::to_vec(&m).expect("serialize");
        let back: Mutation = serde_json::from_slice(&json).expect("deserialize");
        prop_assert_eq!(m, back);
    }
}

// ---------------------------------------------------------------------------
// Property (b): WAL replay idempotency
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn wal_replay_idempotent(mutations in prop::collection::vec(arb_mutation(10), 0..20)) {
        let engine_a = RaftEngine::new();
        for m in &mutations {
            engine_a.apply_mutation(m.clone()).expect("apply");
        }
        let topo_a = engine_a.get_topology();

        let engine_b = RaftEngine::new();
        for m in &mutations {
            engine_b.apply_mutation(m.clone()).expect("apply");
        }
        let topo_b = engine_b.get_topology();

        prop_assert_eq!(topo_a, topo_b);
    }
}

// ---------------------------------------------------------------------------
// Property (c): TopologySnapshot::apply idempotency
//
// AddLink is NOT idempotent (appends), so we restrict to idempotent variants:
// AddNode (replace), RemoveNode, RemoveLink.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn topology_apply_idempotent(
        base_nodes in prop::collection::vec(arb_node_snapshot(), 0..5),
        mutations in prop::collection::vec(arb_idempotent_mutation(5), 0..10),
    ) {
        let mut snap = TopologySnapshot::new();
        snap.nodes = base_nodes;

        for m in &mutations {
            snap.apply(m);
        }
        let after_first = snap.clone();

        for m in &mutations {
            snap.apply(m);
        }

        prop_assert_eq!(snap, after_first);
    }
}
