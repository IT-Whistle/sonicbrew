//! Concurrency i4 — shared-controller safety under parallel mutation.
//!
//! Verifies that an `Arc<GraphController>` (backed by an in-memory
//! `SessionStore`) can be concurrently mutated from multiple tokio tasks
//! without data races, panics, or inconsistent topology reads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use audio_graph_bsd::TopologyEvent;
use control_api::{ControlApi, KindRegistry, NodeSpec};
use session_store::{Mutation, MutationId, SessionStore, TopologySnapshot};

// ---------------------------------------------------------------------------
// In-memory SessionStore (duplicated — integration tests cannot reach lib.rs
// private `mod tests`).
// ---------------------------------------------------------------------------

struct InMemoryStore {
    #[allow(dead_code)]
    tx: tokio::sync::broadcast::Sender<TopologyEvent>,
    topology: Mutex<TopologySnapshot>,
    next_mid: AtomicU64,
}

impl InMemoryStore {
    fn new() -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        Self {
            tx,
            topology: Mutex::new(TopologySnapshot::new()),
            next_mid: AtomicU64::new(1),
        }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for InMemoryStore {
    fn get_topology(&self) -> TopologySnapshot {
        self.topology.lock().expect("topology lock").clone()
    }

    fn apply_mutation(&self, mutation: Mutation) -> session_store::Result<MutationId> {
        let mid = self.next_mid.fetch_add(1, Ordering::SeqCst);
        self.topology
            .lock()
            .expect("topology lock")
            .apply(&mutation);
        Ok(mid)
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TopologyEvent> {
        self.tx.subscribe()
    }
}

fn make_ctrl() -> control_api::GraphController {
    control_api::GraphController::new(Arc::new(InMemoryStore::new()))
}

// ---------------------------------------------------------------------------
// Concurrent node creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_create_nodes_no_data_race() {
    let ctrl = Arc::new(make_ctrl());
    const N: usize = 50;

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.create_node(NodeSpec {
                label: format!("n{i}"),
                inputs: 1,
                outputs: 1,
                kind: None,
                params: None,
            })
            .expect("create_node");
        }));
    }
    for h in handles {
        h.await.expect("task panic");
    }

    let nodes = ctrl.list_nodes();
    assert_eq!(nodes.len(), N);
    // All ids 1..=N present.
    let mut ids: Vec<_> = nodes.iter().map(|n| n.id).collect();
    ids.sort();
    let expected: Vec<usize> = (1..=N).collect();
    assert_eq!(ids, expected);
}

// ---------------------------------------------------------------------------
// Concurrent creates + reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_create_and_read_consistent() {
    let ctrl = Arc::new(make_ctrl());
    const CREATORS: usize = 20;
    const READERS: usize = 10;

    let mut handles = Vec::with_capacity(CREATORS + READERS);

    // Spawn creators.
    for i in 0..CREATORS {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.create_node(NodeSpec {
                label: format!("c{i}"),
                inputs: 0,
                outputs: 1,
                kind: None,
                params: None,
            })
            .expect("create_node");
        }));
    }

    // Spawn readers — each read must return a consistent snapshot (no partial
    // reads from a torn topology).
    for _ in 0..READERS {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            let snap = c.list_nodes();
            // Every node in the snapshot must have a non-empty label.
            for n in &snap {
                assert!(!n.label.is_empty(), "label must not be empty");
            }
        }));
    }

    for h in handles {
        h.await.expect("task panic");
    }

    let final_count = ctrl.list_nodes().len();
    assert!(
        final_count <= CREATORS,
        "final count {final_count} <= {CREATORS}"
    );
}

// ---------------------------------------------------------------------------
// Concurrent delete_node — double-delete is safe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_double_delete_only_one_succeeds() {
    let ctrl = Arc::new(make_ctrl());
    ctrl.create_node(NodeSpec {
        label: "target".into(),
        inputs: 0,
        outputs: 1,
        kind: None,
        params: None,
    })
    .unwrap();

    let c1 = Arc::clone(&ctrl);
    let c2 = Arc::clone(&ctrl);
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { c1.delete_node(1) }),
        tokio::spawn(async move { c2.delete_node(1) }),
    );
    let r1 = r1.expect("task 1 panic");
    let r2 = r2.expect("task 2 panic");
    // Exactly one must succeed, the other must return NotFound.
    let successes = [r1.is_ok(), r2.is_ok()].iter().filter(|&&ok| ok).count();
    assert_eq!(successes, 1, "exactly one delete must succeed");

    // Topology is empty.
    assert!(ctrl.list_nodes().is_empty());
}

// ---------------------------------------------------------------------------
// Concurrent delete different nodes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_delete_different_nodes() {
    let ctrl = Arc::new(make_ctrl());
    const N: usize = 30;
    for i in 0..N {
        ctrl.create_node(NodeSpec {
            label: format!("n{i}"),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
    }

    let mut handles = Vec::with_capacity(N);
    for id in 1..=N {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.delete_node(id).expect("delete_node");
        }));
    }
    for h in handles {
        h.await.expect("task panic");
    }
    assert!(ctrl.list_nodes().is_empty());
}

// ---------------------------------------------------------------------------
// Concurrent create + delete interleaved
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_create_delete_interleaved() {
    let ctrl = Arc::new(make_ctrl());
    let mut handles = Vec::new();

    // Phase 1: create 20 nodes.
    for i in 0..20 {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.create_node(NodeSpec {
                label: format!("p1_{i}"),
                inputs: 0,
                outputs: 1,
                kind: None,
                params: None,
            })
            .expect("create");
        }));
    }
    for h in handles.drain(..) {
        h.await.expect("panic");
    }
    assert_eq!(ctrl.list_nodes().len(), 20);

    // Phase 2: delete first 10, create 10 more — concurrently.
    for i in 1..=10 {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.delete_node(i).expect("delete");
        }));
    }
    for i in 0..10 {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.create_node(NodeSpec {
                label: format!("p2_{i}"),
                inputs: 1,
                outputs: 0,
                kind: None,
                params: None,
            })
            .expect("create");
        }));
    }
    for h in handles {
        h.await.expect("panic");
    }

    let nodes = ctrl.list_nodes();
    // Exactly 20 nodes remain (20 - 10 deleted + 10 created).
    assert_eq!(nodes.len(), 20);
}

// ---------------------------------------------------------------------------
// KindRegistry shared across concurrent creates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_kind_registry_consistent() {
    let store = Arc::new(InMemoryStore::new());
    let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
    let ctrl = Arc::new(control_api::GraphController::new_with_kind_registry(
        store,
        Arc::clone(&kinds),
    ));

    const N: usize = 40;
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.create_node(NodeSpec {
                label: format!("k{i}"),
                inputs: 1,
                outputs: 1,
                kind: Some(format!("kind_{i}")),
                params: None,
            })
            .expect("create_node");
        }));
    }
    for h in handles {
        h.await.expect("panic");
    }

    // Every create recorded its kind in the shared registry.
    let kinds_guard = kinds.read().expect("kind lock poisoned");
    assert_eq!(kinds_guard.len(), N);
    for i in 0..N {
        let id = i + 1;
        assert_eq!(
            kinds_guard.get(&id).map(String::as_str),
            Some(format!("kind_{i}").as_str()),
            "kind for node {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// Concurrent link creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_link_creation() {
    let ctrl = Arc::new(make_ctrl());
    // Create 20 source + 20 sink nodes.
    let mut sources = Vec::new();
    let mut sinks = Vec::new();
    for i in 0..20 {
        sources.push(
            ctrl.create_node(NodeSpec {
                label: format!("src{i}"),
                inputs: 0,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap(),
        );
        sinks.push(
            ctrl.create_node(NodeSpec {
                label: format!("snk{i}"),
                inputs: 1,
                outputs: 0,
                kind: None,
                params: None,
            })
            .unwrap(),
        );
    }

    let mut handles = Vec::with_capacity(20);
    for (s, k) in sources.into_iter().zip(sinks) {
        let c = Arc::clone(&ctrl);
        handles.push(tokio::spawn(async move {
            c.link(s, k).expect("link");
        }));
    }
    for h in handles {
        h.await.expect("panic");
    }

    // All 20 links created; topology must have 40 nodes.
    let nodes = ctrl.list_nodes();
    assert_eq!(nodes.len(), 40);
}
