//! Integration tests for REST endpoints — deep round-trip coverage through the
//! axum router via `tower::ServiceExt::oneshot` (no real TCP listener).
//!
//! Mirrors the inline `rest_*` tests in `lib.rs` but exercises additional
//! sequences: full CRUD lifecycle, edge cases, kind registry visibility, and
//! error-path status codes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use audio_graph_bsd::TopologyEvent;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use control_api::{
    ControlApi, CreateNodeResponse, KindRegistry, LinkRequest, LinkResponse, NodeSpec,
};
use session_store::{Mutation, MutationId, SessionStore, TopologySnapshot};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// In-memory SessionStore (duplicated from lib.rs — integration tests cannot
// access the private `mod tests` helper).
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

// ---------------------------------------------------------------------------
// Router + request helpers (mirrors lib.rs private helpers)
// ---------------------------------------------------------------------------

type Shared = Arc<control_api::GraphController>;

async fn list_nodes_handler(State(ctrl): State<Shared>) -> axum::Json<Vec<control_api::NodeInfo>> {
    axum::Json(ctrl.list_nodes())
}

async fn create_node_handler(
    State(ctrl): State<Shared>,
    axum::Json(spec): axum::Json<NodeSpec>,
) -> Result<(StatusCode, axum::Json<CreateNodeResponse>), StatusCode> {
    ctrl.create_node(spec)
        .map(|id| (StatusCode::CREATED, axum::Json(CreateNodeResponse { id })))
        .map_err(|_| StatusCode::BAD_REQUEST)
}

async fn create_link_handler(
    State(ctrl): State<Shared>,
    axum::Json(req): axum::Json<LinkRequest>,
) -> Result<(StatusCode, axum::Json<LinkResponse>), StatusCode> {
    ctrl.link(req.from, req.to)
        .map(|id| (StatusCode::CREATED, axum::Json(LinkResponse { id })))
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)
}

async fn delete_node_handler(
    State(ctrl): State<Shared>,
    Path(id): Path<control_api::NodeId>,
) -> Result<StatusCode, StatusCode> {
    ctrl.delete_node(id)
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|_| StatusCode::NOT_FOUND)
}

async fn delete_link_handler(
    State(ctrl): State<Shared>,
    Path(id): Path<control_api::LinkId>,
) -> Result<StatusCode, StatusCode> {
    ctrl.delete_link(id)
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|_| StatusCode::NOT_FOUND)
}

fn build_test_router(ctrl: Arc<control_api::GraphController>) -> axum::Router {
    axum::Router::new()
        .route("/nodes", get(list_nodes_handler).post(create_node_handler))
        .route("/nodes/:id", delete(delete_node_handler))
        .route("/links", post(create_link_handler))
        .route("/links/:id", delete(delete_link_handler))
        .with_state(ctrl)
}

fn request(method: &str, uri: &str, body: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    builder
        .body(axum::body::Body::from(
            body.map(str::to_owned).unwrap_or_default(),
        ))
        .expect("request builder")
}

fn app() -> axum::Router {
    build_test_router(Arc::new(control_api::GraphController::new(Arc::new(
        InMemoryStore::new(),
    ))))
}

fn app_with_kinds(kinds: KindRegistry) -> (axum::Router, KindRegistry) {
    let store = Arc::new(InMemoryStore::new());
    let ctrl = control_api::GraphController::new_with_kind_registry(store, Arc::clone(&kinds));
    (build_test_router(Arc::new(ctrl)), kinds)
}

// ---------------------------------------------------------------------------
// GET /nodes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_nodes_empty_returns_200() {
    let resp = app().oneshot(request("GET", "/nodes", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    assert_eq!(&bytes[..], b"[]");
}

#[tokio::test]
async fn get_nodes_after_create_returns_node() {
    let ctrl = Arc::new(control_api::GraphController::new(Arc::new(
        InMemoryStore::new(),
    )));
    ctrl.create_node(NodeSpec {
        label: "src".into(),
        inputs: 0,
        outputs: 2,
        kind: None,
        params: None,
    })
    .unwrap();
    let app = build_test_router(ctrl);
    let resp = app.oneshot(request("GET", "/nodes", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Vec<control_api::NodeInfo> =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(body.len(), 1);
    assert_eq!(body[0].label, "src");
    assert_eq!(body[0].outputs, 2);
}

// ---------------------------------------------------------------------------
// POST /nodes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_node_returns_201_with_id() {
    let resp = app()
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"mixer","inputs":2,"outputs":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let parsed: CreateNodeResponse =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(parsed.id, 1);
}

#[tokio::test]
async fn post_node_with_kind_roundtrips_through_get() {
    let app = app();
    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"gain","inputs":1,"outputs":1,"kind":"gain"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app.oneshot(request("GET", "/nodes", None)).await.unwrap();
    let body: Vec<control_api::NodeInfo> =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(body[0].kind.as_deref(), Some("gain"));
}

#[tokio::test]
async fn post_node_without_kind_is_backward_compatible() {
    let resp = app()
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"src","inputs":0,"outputs":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn post_node_empty_label_returns_400() {
    let resp = app()
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"   ","inputs":1,"outputs":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_node_empty_string_label_returns_400() {
    let resp = app()
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"","inputs":1,"outputs":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_node_invalid_json_returns_error() {
    let resp = app()
        .oneshot(request("POST", "/nodes", Some("not json")))
        .await
        .unwrap();
    // Handler maps deserialization failure to BAD_REQUEST.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_node_missing_required_field_returns_error() {
    let resp = app()
        .oneshot(request("POST", "/nodes", Some(r#"{"inputs":1}"#)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// POST /links
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_link_returns_201_with_id() {
    let ctrl = Arc::new(control_api::GraphController::new(Arc::new(
        InMemoryStore::new(),
    )));
    ctrl.create_node(NodeSpec {
        label: "a".into(),
        inputs: 0,
        outputs: 1,
        kind: None,
        params: None,
    })
    .unwrap();
    ctrl.create_node(NodeSpec {
        label: "b".into(),
        inputs: 1,
        outputs: 0,
        kind: None,
        params: None,
    })
    .unwrap();
    let app = build_test_router(ctrl);
    let resp = app
        .oneshot(request("POST", "/links", Some(r#"{"from":1,"to":2}"#)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let parsed: LinkResponse =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(parsed.id, 0);
}

#[tokio::test]
async fn post_link_missing_required_field_returns_error() {
    // An empty body or missing fields causes deserialization failure → 422
    // (axum::Json default; the link handler has no explicit error mapping).
    let resp = app()
        .oneshot(request("POST", "/links", Some(r#"{}"#)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// DELETE /nodes/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_node_existing_returns_204() {
    let ctrl = Arc::new(control_api::GraphController::new(Arc::new(
        InMemoryStore::new(),
    )));
    ctrl.create_node(NodeSpec {
        label: "tmp".into(),
        inputs: 0,
        outputs: 1,
        kind: None,
        params: None,
    })
    .unwrap();
    let app = build_test_router(ctrl);
    let resp = app
        .oneshot(request("DELETE", "/nodes/1", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_node_missing_returns_404() {
    let resp = app()
        .oneshot(request("DELETE", "/nodes/99", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_node_then_get_returns_empty() {
    let ctrl = Arc::new(control_api::GraphController::new(Arc::new(
        InMemoryStore::new(),
    )));
    ctrl.create_node(NodeSpec {
        label: "tmp".into(),
        inputs: 0,
        outputs: 1,
        kind: None,
        params: None,
    })
    .unwrap();
    let app = build_test_router(ctrl);
    let resp = app
        .clone()
        .oneshot(request("DELETE", "/nodes/1", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app.oneshot(request("GET", "/nodes", None)).await.unwrap();
    let body: Vec<control_api::NodeInfo> =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();
    assert!(body.is_empty());
}

// ---------------------------------------------------------------------------
// DELETE /links/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_link_existing_returns_204() {
    let ctrl = Arc::new(control_api::GraphController::new(Arc::new(
        InMemoryStore::new(),
    )));
    let a = ctrl
        .create_node(NodeSpec {
            label: "a".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
    let b = ctrl
        .create_node(NodeSpec {
            label: "b".into(),
            inputs: 1,
            outputs: 0,
            kind: None,
            params: None,
        })
        .unwrap();
    ctrl.link(a, b).unwrap();
    let app = build_test_router(ctrl);
    let resp = app
        .oneshot(request("DELETE", "/links/0", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_link_missing_returns_404() {
    let resp = app()
        .oneshot(request("DELETE", "/links/0", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Full CRUD lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_lifecycle_create_link_list_delete() {
    let ctrl = Arc::new(control_api::GraphController::new(Arc::new(
        InMemoryStore::new(),
    )));
    let app = build_test_router(ctrl);

    // Create two nodes.
    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"src","inputs":0,"outputs":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let n1: CreateNodeResponse =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();

    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"sink","inputs":1,"outputs":0}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let n2: CreateNodeResponse =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();

    // Link them.
    let link_body = format!(r#"{{"from":{},"to":{}}}"#, n1.id, n2.id);
    let resp = app
        .clone()
        .oneshot(request("POST", "/links", Some(&link_body)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // List — two nodes present.
    let resp = app
        .clone()
        .oneshot(request("GET", "/nodes", None))
        .await
        .unwrap();
    let nodes: Vec<control_api::NodeInfo> =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(nodes.len(), 2);

    // Delete first node; link should cascade.
    let resp = app
        .clone()
        .oneshot(request("DELETE", "/nodes/1", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app.oneshot(request("GET", "/nodes", None)).await.unwrap();
    let nodes: Vec<control_api::NodeInfo> =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, 2);
}

// ---------------------------------------------------------------------------
// Kind registry visibility through REST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kind_registry_visible_after_rest_post() {
    let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
    let (app, kinds) = app_with_kinds(kinds);
    let resp = app
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"src","inputs":0,"outputs":1,"kind":"oscillator"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        kinds.read().unwrap().get(&1).map(String::as_str),
        Some("oscillator"),
    );
}

#[tokio::test]
async fn kind_registry_empty_when_no_kind() {
    let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
    let (app, _) = app_with_kinds(kinds.clone());
    let resp = app
        .oneshot(request(
            "POST",
            "/nodes",
            Some(r#"{"label":"src","inputs":0,"outputs":1}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(kinds.read().unwrap().is_empty());
}
