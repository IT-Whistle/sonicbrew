//! Multi-node Raft cluster integration tests (ADR-0003 §6).
//!
//! These cover the TESTING-STANDARDS *Integration* (Layer 1) and
//! *Concurrency* layers, and the test-coverage-heatmap gap noted at
//! session-store × Concurrency ("다중노드 시 추가 필요"): leader election,
//! log replication, and failover across a 3-node in-process cluster.
//!
//! The cluster runs entirely in memory (`LoopbackNetworkFactory` + ephemeral
//! `redb` files) so it is deterministic and fast — election timeouts are
//! fixed at 80–120 ms.

use std::time::Duration;

use audio_graph_bsd::{Mutation, NodeSnapshot, PortDir, PortMeta, SampleFmt};
use openraft::Raft;
use session_store::distributed::{spawn_cluster, ClusterNode};
use session_store::raft_types::TypeConfig;

/// Minimal mono-in / stereo-out node snapshot used by the topology mutations.
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

/// Polling helper: wait until `cond(&metrics)` is true on `raft`, or panic
/// after `timeout`.
async fn wait_for<F>(raft: &Raft<TypeConfig>, timeout: Duration, msg: &str, cond: F)
where
    F: Fn(&openraft::RaftMetrics<u64, openraft::BasicNode>) -> bool + Send + Sync,
{
    raft.wait(Some(timeout))
        .metrics(|m| cond(m), msg.to_string())
        .await
        .unwrap_or_else(|e| panic!("{msg}: {e}"));
}

/// Resolve the current leader id by reading the first node's metrics (after
/// election every node agrees on the leader).
async fn leader_id(nodes: &[ClusterNode]) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        for n in nodes {
            let m = n.raft.metrics().borrow().clone();
            if let Some(leader) = m.current_leader {
                return leader;
            }
        }
        if tokio::time::Instant::now() > deadline {
            panic!("no leader elected within 3s");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Find the `ClusterNode` whose id matches the current leader.
fn leader_node(nodes: &[ClusterNode], leader: u64) -> &ClusterNode {
    nodes
        .iter()
        .find(|n| n.id == leader)
        .expect("leader node present")
}

// ---------------------------------------------------------------------------
// Test 1 — leader election (Concurrency layer)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_node_cluster_elects_a_leader() {
    let nodes = spawn_cluster(&[1, 2, 3]).await;

    // Every node must eventually agree on a single leader.
    let leader = leader_id(&nodes).await;
    assert!(matches!(leader, 1..=3), "leader is a cluster member");

    for n in &nodes {
        wait_for(
            &n.raft,
            Duration::from_secs(3),
            "all nodes report the same leader",
            |m| m.current_leader == Some(leader),
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Test 2 — log replication (Integration layer; heatmap session-store × Concurrency)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_to_leader_replicates_to_all_followers() {
    let nodes = spawn_cluster(&[1, 2, 3]).await;
    let leader = leader_id(&nodes).await;
    let leader_node = leader_node(&nodes, leader);

    // Submit two AddNode mutations to the leader.
    leader_node
        .raft
        .client_write(Mutation::AddNode(sample_node(10)))
        .await
        .expect("write mutation 1");
    leader_node
        .raft
        .client_write(Mutation::AddNode(sample_node(11)))
        .await
        .expect("write mutation 2");

    // Wait until every node has applied at least 2 entries (the membership
    // init log + 2 mutations, but last_applied advancing past the writes is
    // the real signal). Then read each node's topology from its own redb SM.
    for n in &nodes {
        wait_for(
            &n.raft,
            Duration::from_secs(3),
            "follower applies replicated entries",
            |m| m.last_applied.map(|lid| lid.index >= 2).unwrap_or(false),
        )
        .await;
    }

    // Give a brief moment for the very last apply to flush, then verify each
    // node's persisted topology.
    tokio::time::sleep(Duration::from_millis(100)).await;
    for n in &nodes {
        let topo = n.reader.topology().expect("read topology");
        assert!(
            topo.node(10).is_some(),
            "node {} has replicated node 10",
            n.id
        );
        assert!(
            topo.node(11).is_some(),
            "node {} has replicated node 11",
            n.id
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3 — failover (Concurrency layer; heatmap session-store × Concurrency = i5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cluster_survives_leader_loss_and_re_elects() {
    let nodes = spawn_cluster(&[1, 2, 3]).await;
    let leader = leader_id(&nodes).await;
    let leader_node = leader_node(&nodes, leader);

    // Write one mutation under the original leader.
    leader_node
        .raft
        .client_write(Mutation::AddNode(sample_node(20)))
        .await
        .expect("write before failover");

    // Simulate the leader crashing: shut down its Raft core so it stops
    // sending heartbeats. Survivors will time out and trigger re-election.
    leader_node
        .raft
        .shutdown()
        .await
        .expect("shutdown leader raft");

    let survivors: Vec<&ClusterNode> = nodes.iter().filter(|n| n.id != leader).collect();
    assert_eq!(survivors.len(), 2, "two survivors after leader loss");

    // The survivors must elect a new leader. With the old leader unreachable,
    // a new leader emerges from the survivors.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut new_leader = None;
    while tokio::time::Instant::now() < deadline {
        for n in &survivors {
            let m = n.raft.metrics().borrow().clone();
            if let Some(l) = m.current_leader {
                if l != leader {
                    new_leader = Some(l);
                    break;
                }
            }
        }
        if new_leader.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let new_leader = new_leader.expect("a survivor became the new leader");
    assert_ne!(new_leader, leader, "new leader differs from the old one");

    // Write a new mutation under the new leader and confirm it replicates to
    // the *other* survivor (proving the cluster still commits).
    let new_leader_node = survivors
        .iter()
        .find(|n| n.id == new_leader)
        .expect("new leader is a survivor");
    new_leader_node
        .raft
        .client_write(Mutation::AddNode(sample_node(21)))
        .await
        .expect("write under new leader");

    for n in &survivors {
        wait_for(
            &n.raft,
            Duration::from_secs(4),
            "survivor applies post-failover write",
            |m| m.last_applied.map(|lid| lid.index >= 3).unwrap_or(false),
        )
        .await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    for n in &survivors {
        let topo = n.reader.topology().expect("read topology");
        assert!(topo.node(20).is_some(), "pre-failover write survived");
        assert!(topo.node(21).is_some(), "post-failover write replicated");
    }
}

// ---------------------------------------------------------------------------
// Test 4 — multi-write determinism (Integration layer; smoke for repeated writes)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ten_sequential_writes_all_replicate() {
    let nodes = spawn_cluster(&[1, 2, 3]).await;
    let leader = leader_id(&nodes).await;
    let leader_node = leader_node(&nodes, leader);

    for i in 0..10u32 {
        leader_node
            .raft
            .client_write(Mutation::AddNode(sample_node(100 + i as usize)))
            .await
            .expect("sequential write");
    }

    // Wait for every node to reach the final applied index.
    for n in &nodes {
        wait_for(
            &n.raft,
            Duration::from_secs(5),
            "node catches up to 10 writes",
            |m| m.last_applied.map(|lid| lid.index >= 10).unwrap_or(false),
        )
        .await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    for n in &nodes {
        let topo = n.reader.topology().expect("read topology");
        for i in 0..10u32 {
            assert!(
                topo.node(100 + i as usize).is_some(),
                "node {} has replicated node {}",
                n.id,
                100 + i
            );
        }
    }
}
