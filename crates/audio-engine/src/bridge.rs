//! Gateway bridge: the gateway worker's stable rtrb handles that survive graph
//! rebuilds. The bridge owns the gateway-side ends (`Mutex<Option<..>>`, locked
//! only by the off-RT worker around push/pop); the graph-side ends live inside
//! `RingSource`/`RingSink` (RT-safe). On each rebuild the factory calls
//! [`GatewayBridge::make_source_node`] / [`make_sink_node`], which allocate a
//! FRESH rtrb pair, box the graph-side node, and store the new gateway-side end
//! — so the worker's next locked access transparently picks up the new ring.

use std::sync::Mutex;

use audio_core_bsd::AudioFrame;
use audio_graph_bsd::{RingSink, RingSource};

use crate::BuiltNode;

/// Default ring capacity (matches `gw-browser`'s `RING_CAPACITY = 128`).
const DEFAULT_CAPACITY: usize = 128;

/// A rebuild-resilient bridge between a gateway worker and the audio graph.
///
/// The worker calls [`push_inbound`] / [`pop_outbound`]; the graph rebuild
/// factory calls [`make_source_node`] / [`make_sink_node`]. All gateway-side
/// state is behind `Mutex` and touched ONLY off-RT (the RT `RingSource`/`RingSink`
/// own their ends and never touch this struct).
///
/// [`push_inbound`]: GatewayBridge::push_inbound
/// [`pop_outbound`]: GatewayBridge::pop_outbound
/// [`make_source_node`]: GatewayBridge::make_source_node
/// [`make_sink_node`]: GatewayBridge::make_sink_node
pub struct GatewayBridge {
    channels: u16,
    sample_rate: u32,
    num_frames: usize,
    capacity: usize,
    /// Gateway-side inbound producer (worker pushes; `RingSource` drains the consumer).
    inbound: Mutex<Option<rtrb::Producer<AudioFrame>>>,
    /// Gateway-side outbound consumer (worker pops; `RingSink` fills the producer via flush).
    outbound: Mutex<Option<rtrb::Consumer<AudioFrame>>>,
}

impl GatewayBridge {
    /// Create with the default ring capacity (128).
    #[must_use]
    pub fn new(channels: u16, sample_rate: u32, num_frames: usize) -> Self {
        Self::with_capacity(channels, sample_rate, num_frames, DEFAULT_CAPACITY)
    }

    /// Create with an explicit ring capacity.
    #[must_use]
    pub fn with_capacity(
        channels: u16,
        sample_rate: u32,
        num_frames: usize,
        capacity: usize,
    ) -> Self {
        Self {
            channels,
            sample_rate,
            num_frames,
            capacity,
            inbound: Mutex::new(None),
            outbound: Mutex::new(None),
        }
    }

    /// Allocate a FRESH inbound ring, store the gateway-side `Producer`, and
    /// return a `RingSource` (Plain node) bound to the consumer. Call on init
    /// and on every rebuild — the worker's next [`push_inbound`] uses the new ring.
    ///
    /// [`push_inbound`]: GatewayBridge::push_inbound
    #[must_use]
    pub fn make_source_node(&self) -> BuiltNode {
        let (producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(self.capacity);
        *self.inbound.lock().expect("inbound lock poisoned") = Some(producer);
        BuiltNode::Plain(Box::new(RingSource::new(
            consumer,
            self.channels,
            self.sample_rate,
            self.num_frames,
        )))
    }

    /// Allocate a FRESH outbound ring, store the gateway-side `Consumer`, and
    /// return a `RingSink` (Sink node → registered via `add_sink` → flushed) bound
    /// to the producer. Call on init and on every rebuild.
    #[must_use]
    pub fn make_sink_node(&self) -> BuiltNode {
        let (producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(self.capacity);
        *self.outbound.lock().expect("outbound lock poisoned") = Some(consumer);
        BuiltNode::Sink(Box::new(RingSink::new(
            producer,
            self.channels,
            self.sample_rate,
            self.num_frames,
        )))
    }

    /// Gateway worker: push a frame into the inbound ring (off-RT). Returns
    /// `PushError::Full(frame)` if the ring is full (treat as xrun/drop) or no
    /// source node exists yet.
    pub fn push_inbound(&self, frame: AudioFrame) -> Result<(), rtrb::PushError<AudioFrame>> {
        match self.inbound.lock().expect("inbound lock poisoned").as_mut() {
            Some(p) => p.push(frame),
            None => Err(rtrb::PushError::Full(frame)),
        }
    }

    /// Gateway worker: pop a frame from the outbound ring (off-RT). Returns
    /// `PopError::Empty` if empty or no sink node exists yet.
    pub fn pop_outbound(&self) -> Result<AudioFrame, rtrb::PopError> {
        match self
            .outbound
            .lock()
            .expect("outbound lock poisoned")
            .as_mut()
        {
            Some(c) => c.pop(),
            None => Err(rtrb::PopError::Empty),
        }
    }
}

// GatewayBridge is Send+Sync: Mutex<Option<{Producer,Consumer}>> + Copy fields.
// (rtrb Producer/Consumer are Send+Sync.) Add a compile-time assert.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_graph, rebuild_slot, GraphEngine, NodeFactory};
    use audio_core_bsd::ProcessContext;
    use audio_graph_bsd::{
        GraphConfig, Mutation, NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge,
        TopologySnapshot,
    };

    const NF: usize = 64;
    const SR: u32 = 48_000;

    fn _assert_send_sync() {
        fn requires<T: Send + Sync>() {}
        requires::<GatewayBridge>();
        requires::<std::sync::Arc<GatewayBridge>>();
    }

    fn mono(direction: PortDir) -> Vec<PortMeta> {
        vec![PortMeta {
            direction,
            channels: 1,
            sample_format: SampleFmt::F32,
        }]
    }

    /// A 2-node topology: source(0) → sink(1).
    fn topo_src_sink() -> TopologySnapshot {
        let mut t = TopologySnapshot::new();
        t.apply(&Mutation::AddNode(NodeSnapshot {
            id: 0,
            inputs: vec![],
            outputs: mono(PortDir::Output),
        }));
        t.apply(&Mutation::AddNode(NodeSnapshot {
            id: 1,
            inputs: mono(PortDir::Input),
            outputs: vec![],
        }));
        t.apply(&Mutation::AddLink(SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        }));
        t
    }

    /// Factory: id 0 → bridge source, id 1 → bridge sink.
    struct BridgeFactory {
        bridge: std::sync::Arc<GatewayBridge>,
    }
    impl NodeFactory for BridgeFactory {
        fn build(&self, id: crate::NodeId) -> Option<BuiltNode> {
            match id {
                0 => Some(self.bridge.make_source_node()),
                1 => Some(self.bridge.make_sink_node()),
                _ => None,
            }
        }
    }

    fn run_one_cycle(eng: &mut GraphEngine, bridge: &GatewayBridge, amp: f32) -> f32 {
        // Worker pushes a frame; engine processes+flushes; worker pops the result.
        bridge
            .push_inbound(AudioFrame::from_planar(1, SR, vec![amp; NF]))
            .expect("push");
        let mut ctx = ProcessContext::new(NF, 0, SR);
        eng.step(&mut ctx);
        match bridge.pop_outbound() {
            Ok(f) => f.samples.iter().map(|s| s.abs()).fold(0.0_f32, f32::max),
            Err(_) => 0.0,
        }
    }

    #[test]
    fn bridge_roundtrip() {
        let bridge = std::sync::Arc::new(GatewayBridge::new(1, SR, NF));
        let factory = BridgeFactory {
            bridge: bridge.clone(),
        };
        let graph = build_graph(&topo_src_sink(), GraphConfig::new(NF, SR, 1), &factory).unwrap();
        let slot = rebuild_slot();
        let mut eng = GraphEngine::new(graph, slot);
        let peak = run_one_cycle(&mut eng, &bridge, 0.5);
        assert!(
            peak > 0.4,
            "bridge roundtrip: outbound peak {peak:.4} not ~0.5"
        );
    }

    #[test]
    fn bridge_survives_rebuild() {
        let bridge = std::sync::Arc::new(GatewayBridge::new(1, SR, NF));
        let factory = BridgeFactory {
            bridge: bridge.clone(),
        };
        let topo = topo_src_sink();
        let config = GraphConfig::new(NF, SR, 1);

        // Graph A — roundtrip works.
        let graph_a = build_graph(&topo, config, &factory).unwrap();
        let slot = rebuild_slot();
        let mut eng = GraphEngine::new(graph_a, slot.clone());
        let peak_a = run_one_cycle(&mut eng, &bridge, 0.5);
        assert!(peak_a > 0.4, "phase A peak {peak_a:.4}");

        // Rebuild: build_graph B via the SAME bridge factory → make_source/make_sink
        // allocate FRESH rings and update the bridge to the new gateway-side ends.
        let graph_b = build_graph(&topo, config, &factory).unwrap();
        // Deposit B; engine swaps to B between cycles (drops A).
        *slot.lock().expect("slot") = Some(graph_b);
        let mut ctx = ProcessContext::new(NF, 0, SR);
        eng.step(&mut ctx); // processes A (idle), then swaps to B

        // The SAME bridge API now drives graph B (new rings) — the gateway survived.
        let peak_b = run_one_cycle(&mut eng, &bridge, 0.5);
        assert!(
            peak_b > 0.4,
            "post-rebuild peak {peak_b:.4} — bridge did not survive rebuild"
        );
    }
}
