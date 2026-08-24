//! audio-engine — runtime orchestration layer.
//!
//! Owns the live audio [`Graph`] and drives one cycle = `process_cycle` (RT,
//! alloc-free) + `flush_sinks` (between-cycle, ships outbound audio) + an
//! optional between-cycle **rebuild swap** (drains the latest rebuilt graph
//! from a shared slot and replaces the live one). This is the flush-compatible
//! live-reload mechanism: because the engine owns the `Graph` by value,
//! `flush_sinks(&mut self)` is sound (unlike `RtHandle`, which shares `&self`
//! and conflicts with `&mut`). A separate rebuild task (follow-up task) fills
//! the slot from `SessionStore` `TopologyEvent`s via [`build_graph`] (snapshot
//! + factory → compile).
//!
//! # RT-safety model
//!
//! sonicbrew's RT loop is a polling `std::thread` (with `sleep`), NOT a hard
//! SCHED_FIFO callback, and `flush_sinks` already allocates between cycles.
//! The between-cycle swap therefore uses a brief `Mutex` lock + `Graph` drop on
//! the engine thread — acceptable for this soft-RT model. A hard-RT (SCHED_FIFO)
//! deployment would need a lock-free swap slot + off-thread drop (future).

use std::sync::{Arc, Mutex};

use audio_core_bsd::{AudioNode, ProcessContext};
use audio_graph_bsd::{
    Graph, GraphError, PortDir, PortMeta, SampleFmt, SinkNode, TopologySnapshot,
};

pub mod bridge;
pub mod builtins;
pub mod nodes;
// Re-exported for callers building configs / addressing nodes/ports. These are
// re-exported (not privately imported) to avoid an ambiguous double binding;
// they remain usable unqualified inside this module via the `pub use`.
pub use audio_graph_bsd::{GraphConfig, NodeId, PortIdx};
// Session-store types the rebuild task consumes. Only `SessionStore` and
// `TopologyEvent` are re-exported here: `TopologySnapshot` / `Mutation` are
// already reachable via `audio_graph_bsd` (re-exporting them again from
// `session_store` would create a duplicate-binding error in this module).
pub use bridge::GatewayBridge;
pub use session_store::{SessionStore, TopologyEvent};

/// A factory-built node, distinguishing flushable sinks so `build_graph` can
/// register them via `Graph::add_sink` (making `flush_sinks` drain them after a
/// rebuild). Plain nodes use `Graph::add_node`.
pub enum BuiltNode {
    /// A plain (non-flushable) node — registered via `Graph::add_node`.
    Plain(Box<dyn AudioNode>),
    /// A flushable sink — registered via `Graph::add_sink` so `flush_sinks`
    /// drains it. Any type that is both `AudioNode` + `Flushable` is a
    /// `SinkNode` (blanket impl in audio-graph-bsd), so
    /// `Box::new(RingSink::new(...))` coerces directly to `Box<dyn SinkNode>`.
    Sink(Box<dyn SinkNode>),
}

/// A node factory: maps a topology [`NodeId`] to a concrete node for rebuilds.
/// `NodeSnapshot` stores only ports (no type tag), so the engine/caller keeps
/// the id→constructor mapping out-of-band and supplies it here.
pub trait NodeFactory: Send + Sync {
    /// Build the node for `id`, or `None` if unknown.
    fn build(&self, id: NodeId) -> Option<BuiltNode>;
}

/// Errors from graph build/compile.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// `build_graph` (snapshot+factory) or `compile` failed.
    #[error("graph build/compile failed: {0}")]
    Build(String),
}
impl From<GraphError> for EngineError {
    fn from(e: GraphError) -> Self {
        Self::Build(e.to_string())
    }
}

/// Build & compile a [`Graph`] from a topology snapshot + factory.
///
/// Iterates the snapshot manually (rather than `Graph::from_snapshot`) so that
/// flushable sinks are registered via [`Graph::add_sink`] — `from_snapshot`
/// only ever uses `add_node`, which would leave a rebuilt `RingSink` as a
/// `NodeSlot::Plain` that `flush_sinks` never drains. Snapshot node ids may be
/// non-contiguous (e.g. after `RemoveNode`), so each is remapped to the new
/// graph's contiguous id space before edges are relinked.
pub fn build_graph(
    snapshot: &TopologySnapshot,
    config: GraphConfig,
    factory: &dyn NodeFactory,
) -> Result<Graph, EngineError> {
    use std::collections::HashMap;
    let mut g = Graph::new();
    let mut id_map: HashMap<NodeId, NodeId> = HashMap::new();
    for ns in &snapshot.nodes {
        let built = factory.build(ns.id).ok_or_else(|| {
            EngineError::Build(format!("factory returned None for node {}", ns.id))
        })?;
        let new_id = match built {
            BuiltNode::Plain(node) => g.add_node(node),
            BuiltNode::Sink(sink) => g.add_sink(sink),
        };
        id_map.insert(ns.id, new_id);
    }
    for edge in &snapshot.edges {
        let from_node = *id_map
            .get(&edge.from.0)
            .ok_or_else(|| EngineError::Build(format!("edge from unknown node {}", edge.from.0)))?;
        let to_node = *id_map
            .get(&edge.to.0)
            .ok_or_else(|| EngineError::Build(format!("edge to unknown node {}", edge.to.0)))?;
        g.link((from_node, edge.from.1), (to_node, edge.to.1))?;
    }
    g.compile(config)?;
    Ok(g)
}

/// Shared single-slot "latest rebuild wins" channel between a rebuild task
/// (producer) and the engine (consumer). The rebuild task stores a freshly-built
/// `Graph`; the engine takes it between cycles.
pub type RebuildSlot = Arc<Mutex<Option<Graph>>>;

/// Create an empty rebuild slot.
#[must_use]
pub fn rebuild_slot() -> RebuildSlot {
    Arc::new(Mutex::new(None))
}

/// Build a graph from the store's CURRENT snapshot and store it in the slot
/// (latest-wins). Best-effort: build errors are traced, not fatal.
fn rebuild_once(
    store: &dyn SessionStore,
    config: GraphConfig,
    factory: &dyn NodeFactory,
    slot: &RebuildSlot,
) {
    let topo = store.get_topology();
    match build_graph(&topo, config, factory) {
        Ok(g) => {
            *slot.lock().expect("rebuild slot poisoned") = Some(g);
        }
        Err(e) => tracing::warn!(error = %e, "rebuild: build_graph failed for current topology"),
    }
}

/// Spawn a `std::thread` that subscribes to the store's [`TopologyEvent`]s and
/// rebuilds the graph (build_graph + compile) into `slot` whenever the
/// topology changes.
///
/// The thread also performs ONE initial rebuild on startup, so a topology that
/// pre-existed the spawn is reflected. It polls the tokio broadcast receiver
/// via `try_recv` (no tokio runtime needed — `subscribe()` and `try_recv()` are
/// plain sync operations on `broadcast::Sender`/`Receiver`). The thread
/// self-exits when the store's broadcast channel closes (i.e. when the store is
/// dropped). Returns the worker `JoinHandle` (dropping the handle detaches the
/// thread; it still exits on its own when the store goes away).
///
/// # Panics
///
/// Panics if the OS-level `std::thread::spawn` fails (extremely rare).
#[must_use]
pub fn spawn_rebuild_task(
    store: Arc<dyn SessionStore>,
    config: GraphConfig,
    factory: Arc<dyn NodeFactory>,
    slot: RebuildSlot,
) -> std::thread::JoinHandle<()> {
    let mut sub = store.subscribe();
    std::thread::Builder::new()
        .name("sonicbrew-rebuild".into())
        .spawn(move || {
            // Initial rebuild reflects any topology that pre-existed this spawn.
            rebuild_once(&*store, config, &*factory, &slot);
            loop {
                match sub.try_recv() {
                    Ok(_event) => rebuild_once(&*store, config, &*factory, &slot),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        tracing::info!("rebuild: store broadcast closed, exiting");
                        break;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        // Missed events under burst — force a fresh rebuild from
                        // the latest snapshot (the source of truth).
                        rebuild_once(&*store, config, &*factory, &slot);
                    }
                }
            }
        })
        .expect("spawn sonicbrew-rebuild thread")
}

/// Owns the live `Graph` and runs one engine tick per audio cycle.
pub struct GraphEngine {
    graph: Graph,
    slot: RebuildSlot,
}
impl GraphEngine {
    /// Create an engine owning `graph` (already compiled), sharing `slot` with
    /// a rebuild task. The engine is the slot consumer.
    #[must_use]
    pub fn new(graph: Graph, slot: RebuildSlot) -> Self {
        Self { graph, slot }
    }

    /// Run one cycle: `process_cycle` (RT) → `flush_sinks` (between-cycle) →
    /// drain the rebuild slot and swap if a new graph is pending. Best-effort:
    /// process/flush errors are traced (warn) and the cycle continues.
    ///
    /// Note: the rebuild swap happens *after* process+flush, so a graph deposited
    /// into the slot during this call is first *processed* on the next `step`.
    pub fn step(&mut self, ctx: &mut ProcessContext) {
        if let Err(e) = self.graph.process_cycle(ctx) {
            tracing::error!(error = %e, "engine process_cycle failed; continuing");
        }
        let (flushed, ferr) = self.graph.flush_sinks();
        if let Some(e) = ferr {
            tracing::warn!(error = %e, flushed, "engine flush_sinks reported an error");
        }
        // Between-cycle rebuild swap (latest-wins). Drop of the old graph happens
        // here (engine thread, NOT inside process_cycle) — see RT-safety model.
        if let Some(new_graph) = self.slot.lock().expect("rebuild slot poisoned").take() {
            tracing::debug!("engine swapping in a rebuilt graph");
            self.graph = new_graph;
        }
    }

    /// Read-only access to the live graph (e.g. test taps via `read_input`).
    pub fn graph(&self) -> &Graph {
        &self.graph
    }
}

// ===== Test helpers (also useful for callers) =====

/// Convenience: build a mono input/output [`PortMeta`] list.
#[must_use]
pub fn mono_port(direction: PortDir) -> Vec<PortMeta> {
    vec![PortMeta {
        direction,
        channels: 1,
        sample_format: SampleFmt::F32,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_graph_bsd::{Mutation, NodeSnapshot, SnapshotEdge};

    const SR: u32 = 48_000;
    const NF: usize = 64;

    /// A factory that maps ids 0/1/2 → SineSource/Gain/Capture (builtins).
    struct TestFactory {
        gain: f32,
    }
    impl NodeFactory for TestFactory {
        fn build(&self, id: NodeId) -> Option<BuiltNode> {
            match id {
                0 => Some(BuiltNode::Plain(Box::new(builtins::SineSource::new(
                    440.0, 0.5, NF, SR,
                )))),
                1 => Some(BuiltNode::Plain(Box::new(builtins::Gain::new(self.gain)))),
                2 => Some(BuiltNode::Plain(Box::new(builtins::Capture::new()))),
                _ => None,
            }
        }
    }

    fn topo_3() -> TopologySnapshot {
        let mut t = TopologySnapshot::new();
        t.apply(&Mutation::AddNode(NodeSnapshot {
            id: 0,
            inputs: vec![],
            outputs: mono_port(PortDir::Output),
        }));
        t.apply(&Mutation::AddNode(NodeSnapshot {
            id: 1,
            inputs: mono_port(PortDir::Input),
            outputs: mono_port(PortDir::Output),
        }));
        t.apply(&Mutation::AddNode(NodeSnapshot {
            id: 2,
            inputs: mono_port(PortDir::Input),
            outputs: vec![],
        }));
        t.apply(&Mutation::AddLink(SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        }));
        t.apply(&Mutation::AddLink(SnapshotEdge {
            from: (1, 0),
            to: (2, 0),
        }));
        t
    }

    #[test]
    fn build_graph_from_snapshot() {
        let g = build_graph(
            &topo_3(),
            GraphConfig::new(NF, SR, 1),
            &TestFactory { gain: 1.0 },
        )
        .expect("build+compile");
        assert_eq!(g.node_count(), 3);
    }

    #[test]
    fn engine_step_processes_and_flushes() {
        let g = build_graph(
            &topo_3(),
            GraphConfig::new(NF, SR, 1),
            &TestFactory { gain: 1.0 },
        )
        .expect("build+compile");
        let slot = rebuild_slot();
        let mut eng = GraphEngine::new(g, slot);
        let mut ctx = ProcessContext::new(NF, 0, SR);
        eng.step(&mut ctx);
        eng.step(&mut ctx);
        // Audio reached the Capture sink (read its input scratch). Gain 1.0 ×
        // sine amp 0.5 → ~0.5.
        let peak = eng.graph().read_input(2, 0).map_or(0.0, |f| {
            f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max)
        });
        assert!(peak > 0.4 && peak < 0.6, "peak {peak} not ~0.5");
    }

    #[test]
    fn engine_swaps_rebuilt_graph_between_cycles() {
        // Engine starts with gain 1.0.
        let g_a = build_graph(
            &topo_3(),
            GraphConfig::new(NF, SR, 1),
            &TestFactory { gain: 1.0 },
        )
        .expect("build+compile A");
        let slot = rebuild_slot();
        let mut eng = GraphEngine::new(g_a, slot.clone());
        let mut ctx = ProcessContext::new(NF, 0, SR);
        eng.step(&mut ctx);
        let peak_a = eng.graph().read_input(2, 0).map_or(0.0, |f| {
            f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max)
        });

        // A rebuild task (simulated) deposits a gain-0.5 graph into the slot.
        let g_b = build_graph(
            &topo_3(),
            GraphConfig::new(NF, SR, 1),
            &TestFactory { gain: 0.5 },
        )
        .expect("build+compile B");
        *slot.lock().unwrap() = Some(g_b);
        // The swap happens at the END of step (after process+flush), so this
        // first step still processes graph_a and then installs graph_b.
        eng.step(&mut ctx);
        // A second step is required so the freshly-installed graph_b actually
        // runs process_cycle and its sink-input scratch reflects gain 0.5.
        eng.step(&mut ctx);
        let peak_b = eng.graph().read_input(2, 0).map_or(0.0, |f| {
            f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max)
        });

        assert!(
            peak_b < peak_a * 0.6,
            "peak did not drop after swap (a={peak_a:.4} b={peak_b:.4})"
        );
        assert!(peak_b > 0.2, "peak too low after swap (b={peak_b:.4})");
    }

    #[test]
    fn rebuild_task_rebuilds_on_topology_event() {
        use session_store::{RaftEngine, SessionStore};
        use std::time::{Duration, Instant};

        let store: Arc<dyn SessionStore> = Arc::new(RaftEngine::default());
        let slot = rebuild_slot();
        let config = GraphConfig::new(NF, SR, 1);
        let factory = Arc::new(TestFactory { gain: 1.0 });
        // Detached: the thread exits on its own when `store` (and thus its
        // broadcast sender) is dropped at the end of this test.
        let _handle = spawn_rebuild_task(store.clone(), config, factory, slot.clone());

        // Apply the 3-node + 2-link topology via the store (broadcasts
        // TopologyEvents that the rebuild thread consumes).
        store
            .apply_mutation(Mutation::AddNode(NodeSnapshot {
                id: 0,
                inputs: vec![],
                outputs: mono_port(PortDir::Output),
            }))
            .unwrap();
        store
            .apply_mutation(Mutation::AddNode(NodeSnapshot {
                id: 1,
                inputs: mono_port(PortDir::Input),
                outputs: mono_port(PortDir::Output),
            }))
            .unwrap();
        store
            .apply_mutation(Mutation::AddNode(NodeSnapshot {
                id: 2,
                inputs: mono_port(PortDir::Input),
                outputs: vec![],
            }))
            .unwrap();
        store
            .apply_mutation(Mutation::AddLink(SnapshotEdge {
                from: (0, 0),
                to: (1, 0),
            }))
            .unwrap();
        store
            .apply_mutation(Mutation::AddLink(SnapshotEdge {
                from: (1, 0),
                to: (2, 0),
            }))
            .unwrap();

        // Poll until the rebuild task has deposited the COMPLETE 3-node/2-link
        // graph in the slot (checking only node_count would race the link events).
        let deadline = Instant::now() + Duration::from_secs(2);
        let got = loop {
            let (n, l) = slot
                .lock()
                .unwrap()
                .as_ref()
                .map_or((0_usize, 0_usize), |g| (g.node_count(), g.link_count()));
            if n >= 3 && l >= 2 {
                break n;
            }
            assert!(
                Instant::now() < deadline,
                "rebuild never reached 3 nodes (got {n})"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(got, 3);

        // The engine can swap it in and run it.
        let rebuilt = slot.lock().unwrap().take().unwrap();
        let mut eng = GraphEngine::new(rebuilt, slot.clone());
        let mut ctx = ProcessContext::new(NF, 0, SR);
        eng.step(&mut ctx);
        // Audio reached the Capture sink (gain 1.0 × sine amp 0.5 → ~0.5).
        let peak = eng.graph().read_input(2, 0).map_or(0.0, |f| {
            f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max)
        });
        assert!(
            peak > 0.4,
            "rebuilt graph produced no audio (peak {peak:.4})"
        );
    }

    /// Regression guard for the composition bug where `build_graph` (via
    /// `Graph::from_snapshot`) registered EVERY node as `NodeSlot::Plain`,
    /// including flushable `RingSink`s — so `flush_sinks` never drained a
    /// rebuilt sink and no outbound audio shipped after a live rebuild.
    ///
    /// With the fix, `build_graph` routes flushable sinks through
    /// `Graph::add_sink` (`NodeSlot::Sink`), so `flush_sinks` pushes the
    /// stashed frame into the sink's `rtrb::Producer` and the test's consumer
    /// receives it. The test builds the graph TWICE to also cover the rebuild
    /// path (the second build's sink must again be `Sink`-registered).
    #[test]
    fn rebuilt_sink_is_flushed_via_add_sink() {
        use audio_core_bsd::AudioFrame;
        use audio_graph_bsd::RingSink;
        use rtrb::RingBuffer;

        // Factory: id 0 → Plain(SineSource), id 1 → Sink(RingSink). The sink's
        // consumer is captured in a shared cell so the test can pop the frame
        // the engine flushes between cycles.
        struct SinkFactory {
            cons: Arc<std::sync::Mutex<Option<rtrb::Consumer<AudioFrame>>>>,
        }
        impl NodeFactory for SinkFactory {
            fn build(&self, id: NodeId) -> Option<BuiltNode> {
                match id {
                    0 => Some(BuiltNode::Plain(Box::new(builtins::SineSource::new(
                        440.0, 0.5, NF, SR,
                    )))),
                    1 => {
                        let (prod, cons) = RingBuffer::<AudioFrame>::new(16);
                        *self.cons.lock().unwrap() = Some(cons);
                        Some(BuiltNode::Sink(Box::new(RingSink::new(prod, 1, SR, NF))))
                    }
                    _ => None,
                }
            }
        }

        fn topo_src_sink() -> TopologySnapshot {
            let mut t = TopologySnapshot::new();
            t.apply(&Mutation::AddNode(NodeSnapshot {
                id: 0,
                inputs: vec![],
                outputs: mono_port(PortDir::Output),
            }));
            t.apply(&Mutation::AddNode(NodeSnapshot {
                id: 1,
                inputs: mono_port(PortDir::Input),
                outputs: vec![],
            }));
            t.apply(&Mutation::AddLink(SnapshotEdge {
                from: (0, 0),
                to: (1, 0),
            }));
            t
        }

        // --- Build #1 (initial) ---
        let cons1 = Arc::new(std::sync::Mutex::new(None));
        let g1 = build_graph(
            &topo_src_sink(),
            GraphConfig::new(NF, SR, 1),
            &SinkFactory {
                cons: Arc::clone(&cons1),
            },
        )
        .expect("build+compile #1");
        let slot = rebuild_slot();
        let mut eng = GraphEngine::new(g1, slot);
        let mut ctx = ProcessContext::new(NF, 0, SR);
        // process_cycle (SineSource → RingSink stash) + flush_sinks (push to ring).
        eng.step(&mut ctx);
        let received1 = cons1.lock().unwrap().as_mut().and_then(|c| c.pop().ok());
        assert!(
            received1.is_some(),
            "initial build: flush_sinks delivered no frame — sink was not Sink-registered"
        );
        let peak1 = received1
            .map(|f| {
                f.samples
                    .iter()
                    .copied()
                    .map(|s| s.abs())
                    .fold(0.0_f32, f32::max)
            })
            .unwrap_or(0.0);
        assert!(
            peak1 > 0.4 && peak1 < 0.6,
            "initial peak {peak1} not ~0.5 (gain 1.0 × sine amp 0.5)"
        );

        // --- Build #2 (rebuild) — the bug-fix path: the rebuilt sink MUST again
        // be registered via add_sink, so flush_sinks drains the NEW ring. With
        // the old `from_snapshot` code this second consumer would stay empty. ---
        let cons2 = Arc::new(std::sync::Mutex::new(None));
        let g2 = build_graph(
            &topo_src_sink(),
            GraphConfig::new(NF, SR, 1),
            &SinkFactory {
                cons: Arc::clone(&cons2),
            },
        )
        .expect("build+compile #2");
        let mut eng2 = GraphEngine::new(g2, rebuild_slot());
        let mut ctx2 = ProcessContext::new(NF, 0, SR);
        eng2.step(&mut ctx2);
        let received2 = cons2.lock().unwrap().as_mut().and_then(|c| c.pop().ok());
        assert!(
            received2.is_some(),
            "rebuilt graph: flush_sinks delivered no frame — rebuilt sink was Plain, not Sink (the bug)"
        );
    }
}
