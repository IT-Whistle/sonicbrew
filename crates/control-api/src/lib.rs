//! M13 — Control API (REST over axum / hyper).
//!
//! MVP scope: static-link only. REST endpoints mutate the session store and
//! surface topology; hot plugin loading is deferred to M15 (BUILD-PLAN §4).
//!
//! This crate exposes the [`ControlApi`] trait contract plus a concrete
//! [`GraphController`] that drives a [`SessionStore`] and an axum [`RestApi`]
//! façade satisfying BUILD-PLAN p11 §7a M13:
//!
//! - `GET /nodes`  → [`ControlApi::list_nodes`] as JSON (200).
//! - `POST /nodes` (body [`NodeSpec`]) → [`ControlApi::create_node`] → 201 +
//!   [`CreateNodeResponse`].
//! - `DELETE /nodes/{id}` → [`ControlApi::delete_node`] → 204 (or 404 when the
//!   node is absent). The session store cascades incident links.
//! - `POST /links` (body [`LinkRequest`]) → [`ControlApi::link`] → 201 +
//!   [`LinkResponse`].
//! - `DELETE /links/{id}` → [`ControlApi::delete_link`] → 204 (or 404).
//! - `GET /preset` → [`GraphController::export_preset`] as JSON (200).
//! - `POST /preset` (body [`Preset`]) → [`GraphController::import_preset`] →
//!   204 (or 422/404 through the usual [`status_for`] mapping).
//! - gRPC is optional and skipped in the MVP (REST only).
//! - [`ControlApi::load_module`] returns [`ControlError::Unimplemented`]
//!   (static-link only; M15 is P1).
//!
//! # Node id strategy
//!
//! `audio_graph_bsd`'s [`NodeSnapshot`] carries an `id`, but the session store
//! assigns *mutation* ids ([`session_store::MutationId`]), not node ids. For
//! the MVP the controller derives a new node's id as `max(existing node ids) +
//! 1` (0 when the graph is empty). This is deterministic, observable through
//! [`SessionStore::get_topology`], and independent of the (still stubbed) store
//! engine.
//!
//! # Labels
//!
//! [`NodeSnapshot`] has no label field, yet [`NodeSpec`] / [`NodeInfo`] carry a
//! human-readable label. The controller therefore keeps a side registry
//! (`labels`) mapping `NodeId -> label`; nodes present in the topology without a
//! recorded label fall back to `node-{id}`.
//!
//! # Node kinds
//!
//! [`NodeSpec`] / [`NodeInfo`] optionally carry a `kind` (e.g. `"gain"`). As
//! with labels, [`NodeSnapshot`] has no kind field, so the controller keeps a
//! second side registry (`kinds`) mapping `NodeId -> kind`. The registry is a
//! [`KindRegistry`] (an `Arc<RwLock<…>>`) so an external reader — e.g. the
//! binary's audio-engine rebuild factory — can observe the kinds `create_node`
//! records. The plain [`GraphController::new`] / [`RestApi::new`] build a fresh
//! private registry; pass a shared one via the `new_with_kind_registry`
//! constructors. **This is ephemeral: `kind` lives only in memory and is lost
//! on restart.** Persisting it needs a `kind` field on [`NodeSnapshot`]
//! upstream (a follow-up).
//!
//! `create_node` validates that `kind` and `params` agree: when `params` is
//! present, its variant (via [`NodeParams::kind_name`]) must map to the
//! declared `kind`. A mismatch — e.g. `{"kind":"eq","params":{"Gain":…}}` —
//! is rejected with [`ControlError::BadRequest`] (REST 400) instead of being
//! silently accepted and falling back to per-kind factory defaults at the
//! rebuild factory. Conversely, when `kind` is omitted but `params` is
//! present, the kind is inferred from the params variant and recorded as if
//! it had been declared explicitly.
//!
//! # Presets
//!
//! Because `kind`/`params` live only in the in-memory side registries, a
//! restart loses them even though the topology itself persists. [`Preset`]
//! closes that gap at the API level: [`GraphController::export_preset`]
//! snapshots the ENTIRE graph state (nodes with label/kind/params + links)
//! into a serializable value, and [`GraphController::import_preset`] restores
//! it — anywhere: the same process, a fresh controller, or a later run via
//! [`Preset::to_json_file`] / [`Preset::from_json_file`]. Import is
//! **replace-semantics and non-transactional**: it first deletes every
//! existing node (the store cascades incident links), then re-creates the
//! preset's nodes and links. A failure mid-import leaves the partially
//! applied state in place (no rollback).
//!
//! # Deletions — known limitations
//!
//! - [`LinkId`] is **positional** into the snapshot's edge vector. Removing a
//!   link shifts the ids of every later edge, so a [`LinkId`] obtained before a
//!   deletion may refer to a different edge afterwards. Clients must re-fetch
//!   topology (or link ids) after any mutation that changes edges.
//! - [`ControlApi::delete_node`] does not emit `Mutation::RemoveLink`s for the
//!   node's incident links: the session store cascades them inside
//!   `Mutation::RemoveNode`
//!   (`edges.retain(|e| e.from.0 != id && e.to.0 != id)`).

pub use audio_graph_bsd::{LinkId, NodeId};

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use audio_graph_bsd::{NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use session_store::{Mutation, SessionStore, StoreError};

// --- DTOs -------------------------------------------------------------------

/// Read-only node descriptor returned by [`ControlApi::list_nodes`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeInfo {
    /// Stable node id.
    pub id: NodeId,
    /// Human-readable label.
    pub label: String,
    /// Number of input ports.
    pub inputs: u16,
    /// Number of output ports.
    pub outputs: u16,
    /// Optional node kind (e.g. `"gain"`). Ephemeral: held only in the
    /// controller's in-memory side registry (see module docs).
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional kind-specific parameters, round-tripped from [`NodeSpec`].
    /// Ephemeral (same persistence caveat as `kind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<NodeParams>,
}

/// Node creation request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeSpec {
    /// Human-readable label.
    pub label: String,
    /// Number of input ports.
    pub inputs: u16,
    /// Number of output ports.
    pub outputs: u16,
    /// Optional node kind (e.g. `"gain"`). Defaults to `None` when omitted,
    /// keeping the request backward-compatible with older clients.
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional kind-specific parameters. Omitting `params` (or sending
    /// `null`) is backward-compatible — the factory applies per-kind
    /// defaults. See [`NodeParams`] for the per-kind payload shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<NodeParams>,
}

/// REST response body for `POST /nodes`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateNodeResponse {
    /// Stable id of the newly created node.
    pub id: NodeId,
}

/// REST request body for `POST /links`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkRequest {
    /// Source node id.
    pub from: NodeId,
    /// Source output port index (0-based). Defaults to 0 when omitted,
    /// keeping the request backward-compatible.
    #[serde(default)]
    pub from_port: Option<u16>,
    /// Destination node id.
    pub to: NodeId,
    /// Destination input port index (0-based). Defaults to 0 when omitted.
    #[serde(default)]
    pub to_port: Option<u16>,
}

/// REST response body for `POST /links`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkResponse {
    /// Positional id of the newly created link.
    pub id: LinkId,
}

/// Read-only link descriptor returned by [`ControlApi::list_links`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkInfo {
    /// Positional link id (index into the topology's edge vector).
    pub id: LinkId,
    /// Source node id.
    pub from: NodeId,
    /// Source output port index.
    pub from_port: u16,
    /// Destination node id.
    pub to: NodeId,
    /// Destination input port index.
    pub to_port: u16,
}

/// Full topology snapshot: all nodes and links in one response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TopologyInfo {
    /// All installed nodes.
    pub nodes: Vec<NodeInfo>,
    /// All installed links.
    pub links: Vec<LinkInfo>,
}

/// Serializable full-graph preset: nodes (with kind/params) + links.
/// Used for persistence (save/restore across restarts) and shareable presets.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Preset {
    /// Preset format version (for future migrations).
    pub version: u32,
    /// All nodes with their kind/params.
    pub nodes: Vec<PresetNode>,
    /// All links.
    pub links: Vec<PresetLink>,
}

/// A single node inside a [`Preset`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PresetNode {
    /// Node id (preserved on import).
    pub id: NodeId,
    /// Human-readable label.
    pub label: String,
    /// Input port count.
    pub inputs: u16,
    /// Output port count.
    pub outputs: u16,
    /// Node kind (may be None).
    #[serde(default)]
    pub kind: Option<String>,
    /// Typed params (may be None).
    #[serde(default)]
    pub params: Option<NodeParams>,
}

/// A single link inside a [`Preset`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PresetLink {
    /// Source node id + port.
    pub from: NodeId,
    pub from_port: u16,
    /// Destination node id + port.
    pub to: NodeId,
    pub to_port: u16,
}

/// Current [`Preset`] format version written by
/// [`GraphController::export_preset`]. Bump on breaking format changes and
/// migrate in `import` paths.
pub const PRESET_VERSION: u32 = 1;

impl Preset {
    /// Serialize to pretty JSON and write to `path` (worker thread — file I/O;
    /// call from a blocking-friendly context, not the async runtime).
    ///
    /// # Errors
    /// Returns [`std::io::Error`] when serialization or the write fails.
    pub fn to_json_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Read + deserialize a preset from `path`.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] when the read fails or the content is not a
    /// valid preset (deserialization errors map to
    /// [`std::io::ErrorKind::InvalidData`]).
    pub fn from_json_file(path: &std::path::Path) -> std::io::Result<Preset> {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Opaque id for a loaded module (MVP: always statically linked).
pub type ModuleId = u64;

/// Shared, externally-readable node-kind registry: `NodeId -> kind` (e.g.
/// `"gain"`). [`GraphController::create_node`] records `NodeSpec::kind` here;
/// an external factory (e.g. the binary's audio-engine rebuild factory) reads
/// it to render the node. In-memory/ephemeral (lost on restart — persistence
/// needs a `kind` field on [`NodeSnapshot`] upstream, a follow-up).
pub type KindRegistry = Arc<RwLock<HashMap<NodeId, String>>>;

/// Kind-specific node parameters, carried alongside `kind` in [`NodeSpec`] /
/// [`NodeInfo`]. All fields are `#[serde(default)]` so clients can send partial
/// params (e.g. only `freq` for an EQ node). When [`NodeSpec::params`] is
/// `None`, the factory applies sensible defaults per kind.
///
/// # Wire format
///
/// Externally tagged (serde default representation): the variant name wraps an
/// inner object.
///
/// ```json
/// {"label":"eq1","inputs":1,"outputs":1,"kind":"eq","params":{"Eq":{"filter_type":"peaking","freq":1000,"gain_db":3,"q":0.707}}}
/// {"label":"comp","inputs":1,"outputs":1,"kind":"compressor","params":{"Compressor":{"threshold_db":-20,"ratio":4,"attack_ms":5,"release_ms":100,"makeup_db":6}}}
/// {"label":"gain","inputs":1,"outputs":1,"kind":"gain","params":{"Gain":{"gain":0.5}}}
/// {"label":"meter","inputs":1,"outputs":1,"kind":"meter","params":"Meter"}
/// ```
///
/// Omitting `params` entirely is backward-compatible (the factory applies
/// defaults). When present, any subset of fields may be supplied; missing
/// fields fall back to their per-variant defaults. The variant must agree
/// with the sibling `kind` (see [`NodeParams::kind_name`]):
/// [`GraphController::create_node`] rejects a mismatch with
/// [`ControlError::BadRequest`] and infers `kind` from the variant when
/// omitted.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NodeParams {
    /// Parameters for `kind = "gain"`.
    Gain {
        #[serde(default = "default_gain")]
        gain: f32,
    },
    /// Parameters for `kind = "mixer"`.
    Mixer {
        #[serde(default = "default_mixer_inputs")]
        inputs: usize,
        #[serde(default = "default_mixer_gains")]
        gains: Vec<f32>,
    },
    /// Parameters for `kind = "eq"`.
    Eq {
        #[serde(default = "default_filter_type")]
        filter_type: String,
        #[serde(default = "default_freq")]
        freq: f32,
        #[serde(default)]
        gain_db: f32,
        #[serde(default = "default_q")]
        q: f32,
    },
    /// Parameters for `kind = "compressor"`.
    Compressor {
        #[serde(default = "default_threshold")]
        threshold_db: f32,
        #[serde(default = "default_ratio")]
        ratio: f32,
        #[serde(default = "default_attack")]
        attack_ms: f32,
        #[serde(default = "default_release")]
        release_ms: f32,
        #[serde(default)]
        makeup_db: f32,
    },
    /// Parameters for `kind = "limiter"`.
    Limiter {
        #[serde(default = "default_limiter_threshold")]
        threshold_db: f32,
    },
    /// Parameters for `kind = "meter"` (no fields).
    Meter,
    /// Parameters for `kind = "channel_map"`.
    ChannelMap {
        #[serde(default = "default_channel_mode")]
        mode: String,
        #[serde(default)]
        pan: Option<f32>,
    },
    /// Parameters for `kind = "delay"`. Times are in milliseconds; the factory
    /// converts them to samples using the configured sample rate.
    Delay {
        #[serde(default = "default_delay_max_ms")]
        max_delay_ms: f32,
        #[serde(default = "default_delay_ms")]
        delay_ms: f32,
        #[serde(default = "default_feedback")]
        feedback: f32,
        #[serde(default = "default_mix")]
        mix: f32,
    },
    /// Parameters for `kind = "noise_gate"`.
    NoiseGate {
        #[serde(default = "default_gate_threshold")]
        threshold_db: f32,
        #[serde(default = "default_gate_attack")]
        attack_ms: f32,
        #[serde(default = "default_gate_hold")]
        hold_ms: f32,
        #[serde(default = "default_gate_release")]
        release_ms: f32,
    },
    /// Parameters for `kind = "noise"` (noise source: 0-in / 1-out).
    Noise {
        #[serde(default = "default_noise_color")]
        color: String,
        #[serde(default = "default_noise_amp")]
        amp: f32,
        #[serde(default = "default_seed")]
        seed: u64,
    },
    /// Parameters for `kind = "tone"` (tone generator: 0-in / 1-out).
    Tone {
        #[serde(default = "default_waveform")]
        waveform: String,
        #[serde(default = "default_tone_freq")]
        freq: f32,
        #[serde(default = "default_tone_amp")]
        amp: f32,
    },
    /// Parameters for `kind = "reverb"` (Schroeder/Freeverb reverb).
    Reverb {
        #[serde(default = "default_room_size")]
        room_size: f32,
        #[serde(default = "default_damping")]
        damping: f32,
        #[serde(default = "default_wet")]
        wet: f32,
        #[serde(default = "default_dry")]
        dry: f32,
    },
    /// Parameters for `kind = "chorus"` (LFO-modulated delay chorus).
    Chorus {
        #[serde(default = "default_chorus_rate")]
        rate_hz: f32,
        #[serde(default = "default_chorus_depth")]
        depth_ms: f32,
        #[serde(default = "default_chorus_center")]
        center_delay_ms: f32,
        #[serde(default = "default_chorus_mix")]
        mix: f32,
    },
    /// Parameters for `kind = "distortion"` (waveshaper).
    Distortion {
        #[serde(default = "default_distortion_mode")]
        mode: String,
        #[serde(default = "default_drive")]
        drive: f32,
        #[serde(default = "default_distortion_threshold")]
        threshold: f32,
        #[serde(default = "default_output_level")]
        output_level: f32,
    },
    /// Parameters for `kind = "flanger"` (LFO + feedback delay).
    Flanger {
        #[serde(default = "default_flanger_rate")]
        rate_hz: f32,
        #[serde(default = "default_flanger_depth")]
        depth_ms: f32,
        #[serde(default = "default_flanger_center")]
        center_delay_ms: f32,
        #[serde(default = "default_flanger_feedback")]
        feedback: f32,
        #[serde(default = "default_flanger_mix")]
        mix: f32,
    },
    /// Parameters for `kind = "aux_send"` (1-in / 2-out splitter).
    AuxSend {
        #[serde(default = "default_send_level")]
        send_level: f32,
    },
    /// Parameters for `kind = "phaser"` (LFO-swept allpass cascade).
    Phaser {
        #[serde(default = "default_phaser_rate")]
        rate_hz: f32,
        #[serde(default = "default_phaser_base")]
        base_freq: f32,
        #[serde(default = "default_phaser_depth")]
        depth: f32,
        #[serde(default = "default_phaser_feedback")]
        feedback: f32,
        #[serde(default = "default_phaser_mix")]
        mix: f32,
        #[serde(default = "default_phaser_stages")]
        stages: u32,
    },
    /// Parameters for `kind = "bitcrusher"` (bit depth + sample rate reduction).
    Bitcrusher {
        #[serde(default = "default_bits")]
        bits: u32,
        #[serde(default = "default_hold_factor")]
        hold_factor: u32,
    },
    /// Parameters for `kind = "tremolo"` (LFO amplitude modulation).
    Tremolo {
        #[serde(default = "default_tremolo_rate")]
        rate_hz: f32,
        #[serde(default = "default_tremolo_depth")]
        depth: f32,
    },
    /// Parameters for `kind = "stereo_widener"` (mid/side width control).
    StereoWidener {
        #[serde(default = "default_width")]
        width: f32,
    },
}

impl NodeParams {
    /// The `kind` string this variant corresponds to (e.g. `Eq` → `"eq"`).
    /// Used to validate that a [`NodeSpec`]'s `kind` and `params` agree.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            NodeParams::Gain { .. } => "gain",
            NodeParams::Mixer { .. } => "mixer",
            NodeParams::Eq { .. } => "eq",
            NodeParams::Compressor { .. } => "compressor",
            NodeParams::Limiter { .. } => "limiter",
            NodeParams::Meter => "meter",
            NodeParams::ChannelMap { .. } => "channel_map",
            NodeParams::Delay { .. } => "delay",
            NodeParams::NoiseGate { .. } => "noise_gate",
            NodeParams::Noise { .. } => "noise",
            NodeParams::Tone { .. } => "tone",
            NodeParams::Reverb { .. } => "reverb",
            NodeParams::Chorus { .. } => "chorus",
            NodeParams::Distortion { .. } => "distortion",
            NodeParams::Flanger { .. } => "flanger",
            NodeParams::AuxSend { .. } => "aux_send",
            NodeParams::Phaser { .. } => "phaser",
            NodeParams::Bitcrusher { .. } => "bitcrusher",
            NodeParams::Tremolo { .. } => "tremolo",
            NodeParams::StereoWidener { .. } => "stereo_widener",
        }
    }
}

// --- NodeParams default value functions -------------------------------------

#[allow(clippy::missing_docs_in_private_items)]
fn default_gain() -> f32 {
    1.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_mixer_inputs() -> usize {
    2
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_mixer_gains() -> Vec<f32> {
    vec![0.5, 0.5]
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_filter_type() -> String {
    "peaking".to_string()
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_freq() -> f32 {
    1000.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_q() -> f32 {
    0.707
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_threshold() -> f32 {
    -12.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_ratio() -> f32 {
    4.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_attack() -> f32 {
    1.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_release() -> f32 {
    50.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_limiter_threshold() -> f32 {
    -1.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_channel_mode() -> String {
    "passthrough".to_string()
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_delay_max_ms() -> f32 {
    500.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_delay_ms() -> f32 {
    250.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_feedback() -> f32 {
    0.3
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_mix() -> f32 {
    0.3
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_gate_threshold() -> f32 {
    -50.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_gate_attack() -> f32 {
    1.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_gate_hold() -> f32 {
    50.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_gate_release() -> f32 {
    100.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_noise_color() -> String {
    "white".to_string()
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_noise_amp() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_seed() -> u64 {
    12_345
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_waveform() -> String {
    "sine".to_string()
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_tone_freq() -> f32 {
    440.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_tone_amp() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_room_size() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_damping() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_wet() -> f32 {
    0.3
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_dry() -> f32 {
    0.7
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_chorus_rate() -> f32 {
    1.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_chorus_depth() -> f32 {
    3.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_chorus_center() -> f32 {
    20.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_chorus_mix() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_distortion_mode() -> String {
    "soft_clip".to_string()
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_drive() -> f32 {
    3.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_distortion_threshold() -> f32 {
    0.7
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_output_level() -> f32 {
    1.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_flanger_rate() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_flanger_depth() -> f32 {
    2.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_flanger_center() -> f32 {
    3.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_flanger_feedback() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_flanger_mix() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_send_level() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_phaser_rate() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_phaser_base() -> f32 {
    800.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_phaser_depth() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_phaser_feedback() -> f32 {
    0.3
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_phaser_mix() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_phaser_stages() -> u32 {
    4
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_bits() -> u32 {
    8
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_hold_factor() -> u32 {
    1
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_tremolo_rate() -> f32 {
    5.0
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_tremolo_depth() -> f32 {
    0.5
}
#[allow(clippy::missing_docs_in_private_items)]
fn default_width() -> f32 {
    1.0
}

/// Shared, externally-readable node-params registry: `NodeId -> NodeParams`.
/// Mirrors [`KindRegistry`]; written by [`GraphController::create_node`] and
/// read by an external factory (e.g. the binary's audio-engine rebuild
/// factory) to render nodes with user-supplied parameters.
pub type ParamsRegistry = Arc<RwLock<HashMap<NodeId, NodeParams>>>;

// --- Errors -----------------------------------------------------------------

/// Errors returned by the control API.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// Operation has not been implemented yet (stub).
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    /// Malformed request body or parameters.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Underlying graph operation failed.
    #[error("graph: {0}")]
    Graph(String),
    /// Referenced node or link does not exist.
    #[error("not found: {0}")]
    NotFound(String),
}

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, ControlError>;

// --- ControlApi trait -------------------------------------------------------

/// Top-level control surface exposed over REST.
pub trait ControlApi: Send + Sync {
    /// Enumerate currently-installed nodes.
    fn list_nodes(&self) -> Vec<NodeInfo>;
    /// Enumerate currently-installed links.
    fn list_links(&self) -> Vec<LinkInfo>;
    /// Install a new node from a spec.
    fn create_node(&self, spec: NodeSpec) -> Result<NodeId>;
    /// Connect `from`'s output to `to`'s input.
    fn link(&self, from: NodeId, to: NodeId) -> Result<LinkId>;
    /// Load a plugin module. MVP: static-link only.
    fn load_module(&self, path: &str) -> Result<ModuleId>;
    /// Remove a node by id. The session store cascades the node's incident
    /// links, so the controller does not emit `RemoveLink` mutations. Returns
    /// [`ControlError::NotFound`] when the node is absent.
    fn delete_node(&self, id: NodeId) -> Result<()>;
    /// Remove a positional [`LinkId`]. Returns [`ControlError::NotFound`] when
    /// the id is out of range. Note: deletion shifts the ids of later edges.
    fn delete_link(&self, id: LinkId) -> Result<()>;
}

// --- GraphController --------------------------------------------------------

/// Shared controller state behind the REST handlers.
type SharedController = Arc<GraphController>;

/// Concrete [`ControlApi`] that translates calls into session-store
/// [`Mutation`]s and reads topology back through [`SessionStore::get_topology`].
pub struct GraphController {
    store: Arc<dyn SessionStore>,
    labels: RwLock<HashMap<NodeId, String>>,
    /// Shared side registry for [`NodeSpec::kind`] / [`NodeInfo::kind`],
    /// mirroring `labels` (see module docs for the persistence caveat). This
    /// is a [`KindRegistry`] (an `Arc`) so an external reader — e.g. the
    /// binary's rebuild factory — can observe the kinds `create_node` records.
    /// `labels` stays per-instance.
    kinds: KindRegistry,
    /// Shared side registry for [`NodeSpec::params`] / [`NodeInfo::params`].
    /// Same ephemeral/persistence caveat as `kinds`.
    params: ParamsRegistry,
}

impl GraphController {
    /// Wrap a session store with the control surface. Builds a fresh private
    /// [`KindRegistry`] and [`ParamsRegistry`] (no external reader). For
    /// shared registries, use [`GraphController::new_with_registries`].
    #[must_use]
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self::new_with_registries(
            store,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
        )
    }

    /// Create with a SHARED [`KindRegistry`] so an external reader (e.g. the
    /// binary's rebuild factory) sees the kinds `create_node` records. `labels`
    /// stays per-instance (internal). Builds a fresh private [`ParamsRegistry`].
    #[must_use]
    pub fn new_with_kind_registry(store: Arc<dyn SessionStore>, kinds: KindRegistry) -> Self {
        Self::new_with_registries(store, kinds, Arc::new(RwLock::new(HashMap::new())))
    }

    /// Create with SHARED [`KindRegistry`] **and** [`ParamsRegistry`] so an
    /// external reader (e.g. the binary's rebuild factory) sees both the kinds
    /// and the typed parameters `create_node` records.
    #[must_use]
    pub fn new_with_registries(
        store: Arc<dyn SessionStore>,
        kinds: KindRegistry,
        params: ParamsRegistry,
    ) -> Self {
        Self {
            store,
            labels: RwLock::new(HashMap::new()),
            kinds,
            params,
        }
    }
}

impl ControlApi for GraphController {
    fn list_nodes(&self) -> Vec<NodeInfo> {
        let topo = self.store.get_topology();
        let labels = self.labels.read().expect("label lock poisoned");
        let kinds = self.kinds.read().expect("kind lock poisoned");
        let params = self.params.read().expect("params lock poisoned");
        topo.nodes
            .iter()
            .map(|ns| NodeInfo {
                id: ns.id,
                label: labels
                    .get(&ns.id)
                    .cloned()
                    .unwrap_or_else(|| format!("node-{}", ns.id)),
                inputs: ns.inputs.len() as u16,
                outputs: ns.outputs.len() as u16,
                kind: kinds.get(&ns.id).cloned(),
                params: params.get(&ns.id).cloned(),
            })
            .collect()
    }

    fn list_links(&self) -> Vec<LinkInfo> {
        let topo = self.store.get_topology();
        topo.edges
            .iter()
            .enumerate()
            .map(|(id, edge)| LinkInfo {
                id,
                from: edge.from.0,
                from_port: edge.from.1 as u16,
                to: edge.to.0,
                to_port: edge.to.1 as u16,
            })
            .collect()
    }

    fn create_node(&self, mut spec: NodeSpec) -> Result<NodeId> {
        if spec.label.trim().is_empty() {
            return Err(ControlError::BadRequest(
                "node label must not be empty".into(),
            ));
        }
        // kind/params agreement: a params variant that does not match the
        // declared kind would silently fall back to per-kind defaults at the
        // factory — reject it instead. When `kind` is omitted but `params`
        // is present, infer the kind from the params variant so downstream
        // readers (kind registry, factory, list_nodes) see a consistent kind.
        if let Some(params) = &spec.params {
            let actual = params.kind_name();
            match spec.kind.as_deref() {
                Some(expected) if expected != actual => {
                    return Err(ControlError::BadRequest(format!(
                        "params variant '{actual}' does not match kind '{expected}'"
                    )));
                }
                Some(_) => {}
                None => spec.kind = Some(actual.to_string()),
            }
        }
        // Node id strategy: max existing id + 1 (see module docs).
        let topo = self.store.get_topology();
        let new_id = topo
            .nodes
            .iter()
            .map(|n| n.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let snapshot = NodeSnapshot {
            id: new_id,
            inputs: build_ports(spec.inputs, PortDir::Input),
            outputs: build_ports(spec.outputs, PortDir::Output),
        };
        self.store
            .apply_mutation(Mutation::AddNode(snapshot))
            .map_err(store_err_to_control)?;
        // Record the label only after the mutation succeeds, so a failed
        // create never leaves a dangling label entry.
        self.labels
            .write()
            .expect("label lock poisoned")
            .insert(new_id, spec.label);
        // Likewise record the (optional) kind only after success.
        if let Some(kind) = spec.kind {
            self.kinds
                .write()
                .expect("kind lock poisoned")
                .insert(new_id, kind);
        }
        // Likewise record the (optional) params only after success.
        if let Some(params) = spec.params {
            self.params
                .write()
                .expect("params lock poisoned")
                .insert(new_id, params);
        }
        Ok(new_id)
    }

    fn link(&self, from: NodeId, to: NodeId) -> Result<LinkId> {
        self.link_ports(from, 0, to, 0)
    }

    fn delete_node(&self, id: NodeId) -> Result<()> {
        // `Mutation::RemoveNode` is a silent no-op when the id is absent, so
        // check existence first to surface a precise NotFound.
        if self.store.get_topology().node(id).is_none() {
            return Err(ControlError::NotFound(format!("node {id}")));
        }
        self.store
            .apply_mutation(Mutation::RemoveNode(id))
            .map_err(store_err_to_control)?;
        // Prune both side registries; the store cascades incident links inside
        // `RemoveNode` (`edges.retain(...)`), so no RemoveLink is needed here.
        self.labels
            .write()
            .expect("label lock poisoned")
            .remove(&id);
        self.kinds.write().expect("kind lock poisoned").remove(&id);
        self.params
            .write()
            .expect("params lock poisoned")
            .remove(&id);
        Ok(())
    }

    fn delete_link(&self, id: LinkId) -> Result<()> {
        // LinkId is positional; an out-of-range `RemoveLink` is a silent no-op
        // at the store level, so check the edge count first.
        if id >= self.store.get_topology().edges.len() {
            return Err(ControlError::NotFound(format!("link {id}")));
        }
        self.store
            .apply_mutation(Mutation::RemoveLink(id))
            .map_err(store_err_to_control)?;
        Ok(())
    }

    fn load_module(&self, _path: &str) -> Result<ModuleId> {
        Err(ControlError::Unimplemented(
            "hot plugin loading is M15 (P1); MVP is static-link only",
        ))
    }
}

impl GraphController {
    /// Link a specific output port of `from` to a specific input port of `to`.
    ///
    /// Unlike [`ControlApi::link`] (which always uses port 0→0), this method
    /// lets callers target individual ports on multi-port nodes (e.g. mixer
    /// input 1 vs input 0, or an aux-send node's aux output port).
    ///
    /// Validates that both nodes exist and that the port indices are in range
    /// before applying the mutation. Returns [`ControlError::NotFound`] for a
    /// missing node, [`ControlError::BadRequest`] for an out-of-range port.
    pub fn link_ports(
        &self,
        from: NodeId,
        from_port: u16,
        to: NodeId,
        to_port: u16,
    ) -> Result<LinkId> {
        let topo = self.store.get_topology();
        // Validate source node + output port.
        let src = topo
            .node(from)
            .ok_or_else(|| ControlError::NotFound(format!("source node {from}")))?;
        if from_port as usize >= src.outputs.len() {
            return Err(ControlError::BadRequest(format!(
                "source node {from} has {} output port(s); from_port {from_port} is out of range",
                src.outputs.len()
            )));
        }
        // Validate destination node + input port.
        let dst = topo
            .node(to)
            .ok_or_else(|| ControlError::NotFound(format!("destination node {to}")))?;
        if to_port as usize >= dst.inputs.len() {
            return Err(ControlError::BadRequest(format!(
                "destination node {to} has {} input port(s); to_port {to_port} is out of range",
                dst.inputs.len()
            )));
        }
        let edge = SnapshotEdge {
            from: (from, from_port as usize),
            to: (to, to_port as usize),
        };
        self.store
            .apply_mutation(Mutation::AddLink(edge))
            .map_err(store_err_to_control)?;
        let link_id = self
            .store
            .get_topology()
            .edges
            .len()
            .checked_sub(1)
            .ok_or_else(|| ControlError::Graph("link mutation did not persist".into()))?;
        Ok(link_id)
    }
}

impl GraphController {
    /// Snapshot the ENTIRE graph state (nodes with kind/params + links) as a
    /// serializable [`Preset`]. Labels fall back to `node-{id}` like
    /// `list_nodes`.
    #[must_use]
    pub fn export_preset(&self) -> Preset {
        Preset {
            version: PRESET_VERSION,
            nodes: self
                .list_nodes()
                .into_iter()
                .map(|n| PresetNode {
                    id: n.id,
                    label: n.label,
                    inputs: n.inputs,
                    outputs: n.outputs,
                    kind: n.kind,
                    params: n.params,
                })
                .collect(),
            links: self
                .list_links()
                .into_iter()
                .map(|l| PresetLink {
                    from: l.from,
                    from_port: l.from_port,
                    to: l.to,
                    to_port: l.to_port,
                })
                .collect(),
        }
    }

    /// Replace the current topology with a [`Preset`]. Deletes every existing
    /// node (store cascades links), then re-creates nodes (with explicit ids
    /// from the preset) and links, repopulating the label/kind/params
    /// registries. Fails with Graph error if the store rejects a mutation.
    ///
    /// **Not transactional**: a failure mid-import leaves the partially
    /// applied state in place (no rollback).
    ///
    /// # Errors
    /// [`ControlError::Graph`] when the session store rejects any mutation.
    pub fn import_preset(&self, preset: &Preset) -> Result<()> {
        // Clear the current graph. RemoveNode cascades incident links inside
        // the store; prune the side registries per id like `delete_node`.
        let existing: Vec<NodeId> = self
            .store
            .get_topology()
            .nodes
            .iter()
            .map(|n| n.id)
            .collect();
        for id in existing {
            self.store
                .apply_mutation(Mutation::RemoveNode(id))
                .map_err(store_err_to_control)?;
            self.labels
                .write()
                .expect("label lock poisoned")
                .remove(&id);
            self.kinds.write().expect("kind lock poisoned").remove(&id);
            self.params
                .write()
                .expect("params lock poisoned")
                .remove(&id);
        }
        // Re-create nodes with the preset's explicit ids, then record
        // label/kind/params in the side registries.
        for node in &preset.nodes {
            let snapshot = NodeSnapshot {
                id: node.id,
                inputs: build_ports(node.inputs, PortDir::Input),
                outputs: build_ports(node.outputs, PortDir::Output),
            };
            self.store
                .apply_mutation(Mutation::AddNode(snapshot))
                .map_err(store_err_to_control)?;
            self.labels
                .write()
                .expect("label lock poisoned")
                .insert(node.id, node.label.clone());
            if let Some(kind) = &node.kind {
                self.kinds
                    .write()
                    .expect("kind lock poisoned")
                    .insert(node.id, kind.clone());
            }
            if let Some(params) = &node.params {
                self.params
                    .write()
                    .expect("params lock poisoned")
                    .insert(node.id, params.clone());
            }
        }
        // Finally re-create links (nodes they reference now exist).
        for link in &preset.links {
            let edge = SnapshotEdge {
                from: (link.from, link.from_port as usize),
                to: (link.to, link.to_port as usize),
            };
            self.store
                .apply_mutation(Mutation::AddLink(edge))
                .map_err(store_err_to_control)?;
        }
        Ok(())
    }
}

/// Builds `count` mono f32 port descriptors in the given direction.
fn build_ports(count: u16, dir: PortDir) -> Vec<PortMeta> {
    (0..count)
        .map(|_| PortMeta {
            direction: dir,
            channels: 1,
            sample_format: SampleFmt::F32,
        })
        .collect()
}

/// Maps a session-store error onto the control-API error space.
fn store_err_to_control(e: StoreError) -> ControlError {
    match e {
        StoreError::InvalidMutation(msg) => ControlError::Graph(msg),
        StoreError::Persistence(msg) => ControlError::Graph(msg),
        StoreError::Unimplemented(msg) => ControlError::Graph(format!("store: {msg}")),
    }
}

// --- RestApi + router -------------------------------------------------------

/// axum / hyper REST façade over a [`SessionStore`].
pub struct RestApi {
    store: Arc<dyn SessionStore>,
    /// Shared node-kind registry threaded into every controller built by
    /// [`RestApi::controller`] (see [`KindRegistry`]).
    kinds: KindRegistry,
    /// Shared node-params registry threaded into every controller built by
    /// [`RestApi::controller`] (see [`ParamsRegistry`]).
    params: ParamsRegistry,
}

impl RestApi {
    /// Wrap a session store with a REST façade. Builds a fresh private
    /// [`KindRegistry`] and [`ParamsRegistry`]. For shared registries, use
    /// [`RestApi::new_with_registries`].
    #[must_use]
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self::new_with_registries(
            store,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
        )
    }

    /// Create with a SHARED [`KindRegistry`] (see [`KindRegistry`]). The
    /// registry is threaded into every [`GraphController`] built by
    /// [`RestApi::controller`], so an external reader observing the same `Arc`
    /// sees the kinds `POST /nodes` records. Builds a fresh private
    /// [`ParamsRegistry`].
    #[must_use]
    pub fn new_with_kind_registry(store: Arc<dyn SessionStore>, kinds: KindRegistry) -> Self {
        Self::new_with_registries(store, kinds, Arc::new(RwLock::new(HashMap::new())))
    }

    /// Create with SHARED [`KindRegistry`] **and** [`ParamsRegistry`] so an
    /// external reader (e.g. the binary's rebuild factory) sees both the kinds
    /// and the typed parameters `POST /nodes` records.
    #[must_use]
    pub fn new_with_registries(
        store: Arc<dyn SessionStore>,
        kinds: KindRegistry,
        params: ParamsRegistry,
    ) -> Self {
        Self {
            store,
            kinds,
            params,
        }
    }

    /// Build a [`GraphController`] over the wrapped store, sharing this API's
    /// [`KindRegistry`] and [`ParamsRegistry`].
    #[must_use]
    pub fn controller(&self) -> GraphController {
        GraphController::new_with_registries(
            Arc::clone(&self.store),
            Arc::clone(&self.kinds),
            Arc::clone(&self.params),
        )
    }

    /// Bind `addr` and serve the REST API until shutdown.
    pub async fn serve(self, addr: std::net::SocketAddr) -> Result<()> {
        let controller = Arc::new(self.controller());
        let app = build_router(controller);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| ControlError::Graph(format!("bind {addr}: {e}")))?;
        tracing::info!(target: "control_api", "REST control API listening on {addr}");
        axum::serve(listener, app)
            .await
            .map_err(|e| ControlError::Graph(format!("serve: {e}")))?;
        Ok(())
    }
}

/// Builds the axum router with the controller injected as [`State`].
fn build_router(state: SharedController) -> Router {
    Router::new()
        .route("/nodes", get(list_nodes).post(create_node))
        .route("/nodes/:id", delete(delete_node_handler))
        .route("/links", get(list_links).post(create_link))
        .route("/links/:id", delete(delete_link_handler))
        .route("/topology", get(get_topology))
        .route("/preset", get(get_preset).post(import_preset_handler))
        .with_state(state)
}

/// Maps a control-API error onto an HTTP status code.
fn status_for(e: &ControlError) -> StatusCode {
    match e {
        ControlError::BadRequest(_) => StatusCode::BAD_REQUEST,
        ControlError::Graph(_) => StatusCode::UNPROCESSABLE_ENTITY,
        ControlError::NotFound(_) => StatusCode::NOT_FOUND,
        ControlError::Unimplemented(_) => StatusCode::NOT_IMPLEMENTED,
    }
}

async fn list_nodes(State(ctrl): State<SharedController>) -> Json<Vec<NodeInfo>> {
    Json(ctrl.list_nodes())
}

async fn list_links(State(ctrl): State<SharedController>) -> Json<Vec<LinkInfo>> {
    Json(ctrl.list_links())
}

async fn get_topology(State(ctrl): State<SharedController>) -> Json<TopologyInfo> {
    Json(TopologyInfo {
        nodes: ctrl.list_nodes(),
        links: ctrl.list_links(),
    })
}

async fn get_preset(State(ctrl): State<SharedController>) -> Json<Preset> {
    Json(ctrl.export_preset())
}

async fn import_preset_handler(
    State(ctrl): State<SharedController>,
    Json(preset): Json<Preset>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    match ctrl.import_preset(&preset) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => {
            tracing::warn!(target: "control_api", error = %e, "import_preset failed");
            Err((status_for(&e), e.to_string()))
        }
    }
}

async fn create_node(
    State(ctrl): State<SharedController>,
    Json(spec): Json<NodeSpec>,
) -> std::result::Result<(StatusCode, Json<CreateNodeResponse>), StatusCode> {
    match ctrl.create_node(spec) {
        Ok(id) => Ok((StatusCode::CREATED, Json(CreateNodeResponse { id }))),
        Err(e) => {
            tracing::warn!(target: "control_api", error = %e, "create_node failed");
            Err(status_for(&e))
        }
    }
}

async fn create_link(
    State(ctrl): State<SharedController>,
    Json(req): Json<LinkRequest>,
) -> std::result::Result<(StatusCode, Json<LinkResponse>), StatusCode> {
    let from_port = req.from_port.unwrap_or(0);
    let to_port = req.to_port.unwrap_or(0);
    match ctrl.link_ports(req.from, from_port, req.to, to_port) {
        Ok(id) => Ok((StatusCode::CREATED, Json(LinkResponse { id }))),
        Err(e) => {
            tracing::warn!(target: "control_api", error = %e, "create_link failed");
            Err(status_for(&e))
        }
    }
}

async fn delete_node_handler(
    State(ctrl): State<SharedController>,
    Path(id): Path<NodeId>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    match ctrl.delete_node(id) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => {
            tracing::warn!(target: "control_api", error = %e, "delete_node failed");
            Err((status_for(&e), e.to_string()))
        }
    }
}

async fn delete_link_handler(
    State(ctrl): State<SharedController>,
    Path(id): Path<LinkId>,
) -> std::result::Result<StatusCode, (StatusCode, String)> {
    match ctrl.delete_link(id) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => {
            tracing::warn!(target: "control_api", error = %e, "delete_link failed");
            Err((status_for(&e), e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use audio_graph_bsd::TopologyEvent;
    use session_store::{MutationId, TopologySnapshot};
    use tower::ServiceExt;

    /// In-memory `SessionStore` that records applied mutations and keeps a
    /// live topology via `TopologySnapshot::apply`. Avoids the redb engine so
    /// the tests stay fast and FreeBSD-free.
    struct InMemoryStore {
        tx: tokio::sync::broadcast::Sender<TopologyEvent>,
        mutations: Mutex<Vec<Mutation>>,
        topology: Mutex<TopologySnapshot>,
        next_mid: AtomicU64,
    }

    impl InMemoryStore {
        fn new() -> Self {
            let (tx, _rx) = tokio::sync::broadcast::channel(16);
            Self {
                tx,
                mutations: Mutex::new(Vec::new()),
                topology: Mutex::new(TopologySnapshot::new()),
                next_mid: AtomicU64::new(1),
            }
        }

        fn recorded(&self) -> Vec<Mutation> {
            self.mutations.lock().expect("mutations lock").clone()
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
            self.mutations
                .lock()
                .expect("mutations lock")
                .push(mutation);
            Ok(mid)
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TopologyEvent> {
            self.tx.subscribe()
        }
    }

    fn fake_controller() -> GraphController {
        GraphController::new(Arc::new(InMemoryStore::new()))
    }

    // --- unit: list_nodes ---

    #[test]
    fn list_nodes_empty() {
        let ctrl = fake_controller();
        assert!(ctrl.list_nodes().is_empty());
    }

    // --- unit: create_node ---

    #[test]
    fn create_node_applies_mutation() {
        let store = Arc::new(InMemoryStore::new());
        let ctrl = GraphController::new(store.clone());

        let id = ctrl
            .create_node(NodeSpec {
                label: "src".into(),
                inputs: 0,
                outputs: 1,
                kind: None,
                params: None,
            })
            .expect("create_node");

        // First node id is max(none)+1 == 1.
        assert_eq!(id, 1);

        let recorded = store.recorded();
        assert_eq!(recorded.len(), 1, "exactly one mutation applied");
        match &recorded[0] {
            Mutation::AddNode(ns) => {
                assert_eq!(ns.id, 1);
                assert_eq!(ns.inputs.len(), 0);
                assert_eq!(ns.outputs.len(), 1);
            }
            other => panic!("expected AddNode, got {other:?}"),
        }

        // Label roundtrips through the side registry.
        let info = &ctrl.list_nodes()[0];
        assert_eq!(info.id, 1);
        assert_eq!(info.label, "src");
        assert_eq!(info.outputs, 1);
    }

    #[test]
    fn create_node_rejects_empty_label() {
        let ctrl = fake_controller();
        let err = ctrl
            .create_node(NodeSpec {
                label: "   ".into(),
                inputs: 1,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap_err();
        assert!(matches!(err, ControlError::BadRequest(_)));
    }

    // --- unit: link ---

    #[test]
    fn link_applies_mutation() {
        let store = Arc::new(InMemoryStore::new());
        let ctrl = GraphController::new(store.clone());
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

        let link = ctrl.link(a, b).expect("link");
        assert_eq!(link, 0, "first edge is positional id 0");

        let recorded = store.recorded();
        let add_links: Vec<&Mutation> = recorded
            .iter()
            .filter(|m| matches!(m, Mutation::AddLink(_)))
            .collect();
        assert_eq!(add_links.len(), 1);
        match add_links[0] {
            Mutation::AddLink(edge) => {
                assert_eq!(edge.from, (a, 0));
                assert_eq!(edge.to, (b, 0));
            }
            _ => unreachable!(),
        }
    }

    // --- unit: link_ports (multi-port) ---

    #[test]
    fn link_ports_targets_specific_port() {
        let store = Arc::new(InMemoryStore::new());
        let ctrl = GraphController::new(store.clone());
        // Source: 1 output.
        let src = ctrl
            .create_node(NodeSpec {
                label: "src".into(),
                inputs: 0,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap();
        // Mixer: 2 inputs.
        let mixer = ctrl
            .create_node(NodeSpec {
                label: "mixer".into(),
                inputs: 2,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap();

        // Link src → mixer input port 1 (not the default 0).
        let link = ctrl.link_ports(src, 0, mixer, 1).expect("link_ports");
        assert_eq!(link, 0);

        let recorded = store.recorded();
        match &recorded[recorded.len() - 1] {
            Mutation::AddLink(edge) => {
                assert_eq!(edge.from, (src, 0));
                assert_eq!(edge.to, (mixer, 1), "should target input port 1");
            }
            other => panic!("expected AddLink, got {other:?}"),
        }
    }

    #[test]
    fn link_ports_rejects_out_of_range() {
        let ctrl = fake_controller();
        let src = ctrl
            .create_node(NodeSpec {
                label: "src".into(),
                inputs: 0,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap();
        let dst = ctrl
            .create_node(NodeSpec {
                label: "dst".into(),
                inputs: 1,
                outputs: 0,
                kind: None,
                params: None,
            })
            .unwrap();
        // to_port 5 on a 1-input node → BadRequest.
        let err = ctrl.link_ports(src, 0, dst, 5).unwrap_err();
        assert!(matches!(err, ControlError::BadRequest(_)));
    }

    #[test]
    fn link_ports_source_not_found() {
        let ctrl = fake_controller();
        ctrl.create_node(NodeSpec {
            label: "dst".into(),
            inputs: 1,
            outputs: 0,
            kind: None,
            params: None,
        })
        .unwrap();
        let err = ctrl.link_ports(99, 0, 1, 0).unwrap_err();
        assert!(matches!(err, ControlError::NotFound(_)));
    }

    // --- unit: delete_node ---

    #[test]
    fn delete_node_applies_remove_mutation() {
        let store = Arc::new(InMemoryStore::new());
        let ctrl = GraphController::new(store.clone());
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();

        ctrl.delete_node(1).expect("delete_node");

        let recorded = store.recorded();
        let removes: Vec<&Mutation> = recorded
            .iter()
            .filter(|m| matches!(m, Mutation::RemoveNode(_)))
            .collect();
        assert_eq!(removes.len(), 1, "exactly one RemoveNode mutation");
        match removes[0] {
            Mutation::RemoveNode(id) => assert_eq!(*id, 1),
            _ => unreachable!(),
        }
    }

    #[test]
    fn delete_node_not_found() {
        let ctrl = fake_controller();
        let err = ctrl.delete_node(42).unwrap_err();
        assert!(matches!(err, ControlError::NotFound(_)));
    }

    // --- unit: delete_link ---

    #[test]
    fn delete_link_applies_remove_mutation() {
        let store = Arc::new(InMemoryStore::new());
        let ctrl = GraphController::new(store.clone());
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
        let link = ctrl.link(a, b).unwrap();
        assert_eq!(link, 0);

        ctrl.delete_link(0).expect("delete_link");

        let recorded = store.recorded();
        let removes: Vec<&Mutation> = recorded
            .iter()
            .filter(|m| matches!(m, Mutation::RemoveLink(_)))
            .collect();
        assert_eq!(removes.len(), 1, "exactly one RemoveLink mutation");
        match removes[0] {
            Mutation::RemoveLink(id) => assert_eq!(*id, 0),
            _ => unreachable!(),
        }
    }

    #[test]
    fn delete_link_not_found() {
        let ctrl = fake_controller();
        let err = ctrl.delete_link(0).unwrap_err();
        assert!(matches!(err, ControlError::NotFound(_)));
    }

    // --- unit: kind side-registry ---

    #[test]
    fn kind_round_trips() {
        let ctrl = fake_controller();
        ctrl.create_node(NodeSpec {
            label: "gain".into(),
            inputs: 1,
            outputs: 1,
            kind: Some("gain".into()),
            params: None,
        })
        .unwrap();
        let info = &ctrl.list_nodes()[0];
        assert_eq!(info.kind.as_deref(), Some("gain"));
    }

    #[test]
    fn kind_none_default() {
        let ctrl = fake_controller();
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        let info = &ctrl.list_nodes()[0];
        assert!(info.kind.is_none());
    }

    #[test]
    fn shared_kind_registry_visible_after_create() {
        let store = Arc::new(InMemoryStore::new());
        let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
        let ctrl = GraphController::new_with_kind_registry(store, Arc::clone(&kinds));
        let id = ctrl
            .create_node(NodeSpec {
                label: "g".into(),
                inputs: 1,
                outputs: 1,
                kind: Some("gain".into()),
                params: None,
            })
            .unwrap();
        // The external `kinds` Arc (not held by the controller's private
        // fields) observes the kind that `create_node` recorded.
        assert_eq!(
            kinds.read().unwrap().get(&id).map(String::as_str),
            Some("gain"),
        );
    }

    // --- unit: params side-registry ---

    #[test]
    fn create_node_with_eq_params_roundtrips() {
        let ctrl = fake_controller();
        ctrl.create_node(NodeSpec {
            label: "eq1".into(),
            inputs: 1,
            outputs: 1,
            kind: Some("eq".into()),
            params: Some(NodeParams::Eq {
                filter_type: "peaking".into(),
                freq: 2000.0,
                gain_db: 6.0,
                q: 1.0,
            }),
        })
        .unwrap();
        let info = &ctrl.list_nodes()[0];
        assert_eq!(info.kind.as_deref(), Some("eq"));
        let params = info.params.as_ref().expect("params present");
        match params {
            NodeParams::Eq {
                filter_type,
                freq,
                gain_db,
                q,
            } => {
                assert_eq!(filter_type, "peaking");
                assert!((freq - 2000.0).abs() < f32::EPSILON);
                assert!((gain_db - 6.0).abs() < f32::EPSILON);
                assert!((q - 1.0).abs() < f32::EPSILON);
            }
            other => panic!("expected Eq, got {other:?}"),
        }
    }

    #[test]
    fn create_node_params_backward_compatible() {
        // A POST without params must still work — params is None.
        let ctrl = fake_controller();
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 1,
            outputs: 1,
            kind: Some("gain".into()),
            params: None,
        })
        .unwrap();
        let info = &ctrl.list_nodes()[0];
        assert!(info.params.is_none());
    }

    #[test]
    fn delete_node_prunes_params_registry() {
        let store = Arc::new(InMemoryStore::new());
        let params: ParamsRegistry = Arc::new(RwLock::new(HashMap::new()));
        let ctrl = GraphController::new_with_registries(
            store.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::clone(&params),
        );
        ctrl.create_node(NodeSpec {
            label: "eq".into(),
            inputs: 1,
            outputs: 1,
            kind: Some("eq".into()),
            params: Some(NodeParams::Eq {
                filter_type: "lowpass".into(),
                freq: 500.0,
                gain_db: 0.0,
                q: 0.707,
            }),
        })
        .unwrap();
        assert!(params.read().unwrap().contains_key(&1));

        ctrl.delete_node(1).unwrap();
        assert!(
            !params.read().unwrap().contains_key(&1),
            "params registry pruned"
        );
    }

    #[test]
    fn shared_params_registry_visible_after_create() {
        let store = Arc::new(InMemoryStore::new());
        let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
        let params: ParamsRegistry = Arc::new(RwLock::new(HashMap::new()));
        let ctrl =
            GraphController::new_with_registries(store, Arc::clone(&kinds), Arc::clone(&params));
        let id = ctrl
            .create_node(NodeSpec {
                label: "comp".into(),
                inputs: 1,
                outputs: 1,
                kind: Some("compressor".into()),
                params: Some(NodeParams::Compressor {
                    threshold_db: -20.0,
                    ratio: 4.0,
                    attack_ms: 5.0,
                    release_ms: 100.0,
                    makeup_db: 6.0,
                }),
            })
            .unwrap();
        let recorded = params.read().unwrap().get(&id).cloned();
        assert!(matches!(recorded, Some(NodeParams::Compressor { .. })));
    }

    // --- unit: kind/params agreement ---

    #[test]
    fn params_variant_mismatch_rejected() {
        // kind="eq" with Gain params: previously accepted silently (the
        // factory fell back to EQ defaults); now rejected as BadRequest.
        let ctrl = fake_controller();
        let err = ctrl
            .create_node(NodeSpec {
                label: "eq1".into(),
                inputs: 1,
                outputs: 1,
                kind: Some("eq".into()),
                params: Some(NodeParams::Gain { gain: 0.5 }),
            })
            .unwrap_err();
        assert!(matches!(err, ControlError::BadRequest(_)));
        // The rejected spec must not leave anything behind.
        assert!(ctrl.list_nodes().is_empty());
    }

    #[test]
    fn params_without_kind_infers_kind() {
        let store = Arc::new(InMemoryStore::new());
        let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
        let ctrl = GraphController::new_with_kind_registry(store, Arc::clone(&kinds));
        let id = ctrl
            .create_node(NodeSpec {
                label: "eq1".into(),
                inputs: 1,
                outputs: 1,
                kind: None,
                params: Some(NodeParams::Eq {
                    filter_type: "peaking".into(),
                    freq: 1000.0,
                    gain_db: 3.0,
                    q: 0.707,
                }),
            })
            .expect("create_node");
        // list_nodes surfaces the inferred kind...
        let info = &ctrl.list_nodes()[0];
        assert_eq!(info.kind.as_deref(), Some("eq"));
        // ...and the shared kinds registry recorded it (factory-visible).
        assert_eq!(
            kinds.read().unwrap().get(&id).map(String::as_str),
            Some("eq"),
        );
    }

    #[test]
    fn params_matching_kind_accepted() {
        // Existing behaviour preserved: agreeing kind + params still create.
        let ctrl = fake_controller();
        let id = ctrl
            .create_node(NodeSpec {
                label: "eq1".into(),
                inputs: 1,
                outputs: 1,
                kind: Some("eq".into()),
                params: Some(NodeParams::Eq {
                    filter_type: "peaking".into(),
                    freq: 1000.0,
                    gain_db: 3.0,
                    q: 0.707,
                }),
            })
            .expect("create_node");
        assert_eq!(id, 1);
        assert_eq!(ctrl.list_nodes()[0].kind.as_deref(), Some("eq"));
    }

    #[tokio::test]
    async fn rest_post_node_params_mismatch_returns_400() {
        let app = build_router(Arc::new(fake_controller()));
        let resp = app
            .oneshot(request(
                "POST",
                "/nodes",
                Some(r#"{"label":"x","inputs":1,"outputs":1,"kind":"eq","params":{"Gain":{"gain":0.5}}}"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rest_shared_kind_registry() {
        let store = Arc::new(InMemoryStore::new());
        let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
        let ctrl = GraphController::new_with_kind_registry(store, Arc::clone(&kinds));
        let app = build_router(Arc::new(ctrl));
        let resp = app
            .oneshot(request(
                "POST",
                "/nodes",
                Some(r#"{"label":"src","inputs":0,"outputs":1,"kind":"gain"}"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::CREATED);
        // The shared registry — reachable from outside the REST handler — sees
        // the kind recorded by the POST.
        assert_eq!(
            kinds.read().unwrap().get(&1).map(String::as_str),
            Some("gain"),
        );
    }

    #[tokio::test]
    async fn rest_post_node_with_params() {
        let store = Arc::new(InMemoryStore::new());
        let kinds: KindRegistry = Arc::new(RwLock::new(HashMap::new()));
        let params: ParamsRegistry = Arc::new(RwLock::new(HashMap::new()));
        let ctrl =
            GraphController::new_with_registries(store, Arc::clone(&kinds), Arc::clone(&params));
        let app = build_router(Arc::new(ctrl));
        let resp = app
            .oneshot(request(
                "POST",
                "/nodes",
                Some(r#"{"label":"eq1","inputs":1,"outputs":1,"kind":"eq","params":{"Eq":{"filter_type":"peaking","freq":1000,"gain_db":3,"q":0.707}}}"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Shared params registry sees the typed params recorded by the POST.
        let p = params.read().unwrap().get(&1).cloned();
        assert!(matches!(p, Some(NodeParams::Eq { .. })));
        if let Some(NodeParams::Eq { freq, .. }) = p {
            assert!((freq - 1000.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn delete_node_prunes_registries() {
        // Create a node carrying both a label and a kind, then delete it. To
        // prove the *side registries* (not merely the topology) were pruned,
        // re-insert a node snapshot with the same id directly into the store
        // (bypassing the controller) and confirm list_nodes now falls back to
        // the synthesized label `node-{id}` and a `None` kind.
        let store = Arc::new(InMemoryStore::new());
        let ctrl = GraphController::new(store.clone());
        ctrl.create_node(NodeSpec {
            label: "ephemeral".into(),
            inputs: 1,
            outputs: 1,
            kind: Some("gain".into()),
            params: None,
        })
        .unwrap();
        assert_eq!(ctrl.list_nodes()[0].label, "ephemeral");
        assert_eq!(ctrl.list_nodes()[0].kind.as_deref(), Some("gain"));

        ctrl.delete_node(1).unwrap();
        assert!(ctrl.list_nodes().is_empty(), "node gone from topology");

        // Re-insert the same id directly; registries must NOT remember it.
        store
            .apply_mutation(Mutation::AddNode(NodeSnapshot {
                id: 1,
                inputs: build_ports(1, PortDir::Input),
                outputs: build_ports(1, PortDir::Output),
            }))
            .unwrap();
        let info = &ctrl.list_nodes()[0];
        assert_eq!(info.label, "node-1", "label registry pruned");
        assert!(info.kind.is_none(), "kind registry pruned");
        assert!(info.params.is_none(), "params registry pruned");
    }

    #[test]
    fn load_module_is_unimplemented() {
        let ctrl = fake_controller();
        let err = ctrl.load_module("/dev/null").unwrap_err();
        assert!(matches!(err, ControlError::Unimplemented(_)));
    }

    // --- rest: oneshot integration ---

    fn request(
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> axum::http::Request<axum::body::Body> {
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

    #[tokio::test]
    async fn rest_get_nodes_returns_empty_200() {
        let app = build_router(Arc::new(fake_controller()));
        let resp = app
            .oneshot(request("GET", "/nodes", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(&bytes[..], b"[]");
    }

    #[tokio::test]
    async fn rest_post_node_returns_201() {
        let app = build_router(Arc::new(fake_controller()));
        let resp = app
            .oneshot(request(
                "POST",
                "/nodes",
                Some(r#"{"label":"src","inputs":0,"outputs":1}"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        let parsed: CreateNodeResponse = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(parsed.id, 1);
    }

    #[tokio::test]
    async fn rest_post_link_returns_201() {
        // Two nodes up front, then link them through the REST surface.
        let ctrl = Arc::new(fake_controller());
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        ctrl.create_node(NodeSpec {
            label: "sink".into(),
            inputs: 1,
            outputs: 0,
            kind: None,
            params: None,
        })
        .unwrap();
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request("POST", "/links", Some(r#"{"from":1,"to":2}"#)))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        let parsed: LinkResponse = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(parsed.id, 0);
    }

    #[tokio::test]
    async fn rest_post_link_with_ports_returns_201() {
        // Multi-port link: link source to mixer's input port 1.
        let ctrl = Arc::new(fake_controller());
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        ctrl.create_node(NodeSpec {
            label: "mixer".into(),
            inputs: 2,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request(
                "POST",
                "/links",
                Some(r#"{"from":1,"from_port":0,"to":2,"to_port":1}"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn rest_post_link_without_ports_backward_compatible() {
        // Old clients omitting from_port/to_port → defaults to 0→0, still 201.
        let ctrl = Arc::new(fake_controller());
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        ctrl.create_node(NodeSpec {
            label: "sink".into(),
            inputs: 1,
            outputs: 0,
            kind: None,
            params: None,
        })
        .unwrap();
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request("POST", "/links", Some(r#"{"from":1,"to":2}"#)))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn rest_post_link_bad_port_returns_400() {
        let ctrl = Arc::new(fake_controller());
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        ctrl.create_node(NodeSpec {
            label: "dst".into(),
            inputs: 1,
            outputs: 0,
            kind: None,
            params: None,
        })
        .unwrap();
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request(
                "POST",
                "/links",
                Some(r#"{"from":1,"from_port":0,"to":2,"to_port":9}"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn list_links_empty() {
        let ctrl = fake_controller();
        assert!(ctrl.list_links().is_empty());
    }

    #[test]
    fn list_links_returns_created_links() {
        let ctrl = fake_controller();
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
        let links = ctrl.list_links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].id, 0);
        assert_eq!(links[0].from, a);
        assert_eq!(links[0].to, b);
    }

    #[test]
    fn list_links_with_ports() {
        let ctrl = fake_controller();
        let src = ctrl
            .create_node(NodeSpec {
                label: "src".into(),
                inputs: 0,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap();
        let mixer = ctrl
            .create_node(NodeSpec {
                label: "mixer".into(),
                inputs: 2,
                outputs: 1,
                kind: None,
                params: None,
            })
            .unwrap();
        ctrl.link_ports(src, 0, mixer, 1).unwrap();
        let links = ctrl.list_links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].from_port, 0);
        assert_eq!(links[0].to_port, 1);
    }

    #[tokio::test]
    async fn rest_get_links_returns_200() {
        let ctrl = Arc::new(fake_controller());
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
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request("GET", "/links", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096)
            .await
            .expect("body");
        let links: Vec<LinkInfo> = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].from, 1);
        assert_eq!(links[0].to, 2);
    }

    #[tokio::test]
    async fn rest_get_topology_returns_nodes_and_links() {
        let ctrl = Arc::new(fake_controller());
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        ctrl.create_node(NodeSpec {
            label: "sink".into(),
            inputs: 1,
            outputs: 0,
            kind: None,
            params: None,
        })
        .unwrap();
        // Link via REST to exercise the full path.
        let app = build_router(ctrl.clone());
        let _ = app
            .oneshot(request("POST", "/links", Some(r#"{"from":1,"to":2}"#)))
            .await;
        // GET topology.
        let app2 = build_router(ctrl);
        let resp = app2
            .oneshot(request("GET", "/topology", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 8192)
            .await
            .expect("body");
        let topo: TopologyInfo = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(topo.nodes.len(), 2);
        assert_eq!(topo.links.len(), 1);
    }

    #[tokio::test]
    async fn rest_delete_node_returns_204() {
        let ctrl = Arc::new(fake_controller());
        ctrl.create_node(NodeSpec {
            label: "src".into(),
            inputs: 0,
            outputs: 1,
            kind: None,
            params: None,
        })
        .unwrap();
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request("DELETE", "/nodes/1", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn rest_delete_node_missing_returns_404() {
        let app = build_router(Arc::new(fake_controller()));
        let resp = app
            .oneshot(request("DELETE", "/nodes/99", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rest_delete_link_returns_204() {
        let ctrl = Arc::new(fake_controller());
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
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request("DELETE", "/links/0", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn rest_delete_link_missing_returns_404() {
        let app = build_router(Arc::new(fake_controller()));
        let resp = app
            .oneshot(request("DELETE", "/links/0", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rest_post_node_without_kind_is_backward_compatible() {
        // A POST body omitting `kind` must still deserialize (serde `default`).
        let app = build_router(Arc::new(fake_controller()));
        let resp = app
            .oneshot(request(
                "POST",
                "/nodes",
                Some(r#"{"label":"src","inputs":0,"outputs":1}"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        let parsed: CreateNodeResponse = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(parsed.id, 1);
    }

    // --- preset: export/import ---

    /// Builds a two-node graph (eq with params → gain) plus one link on the
    /// given controller; returns the created node ids.
    fn build_preset_graph(ctrl: &GraphController) -> (NodeId, NodeId) {
        let eq = ctrl
            .create_node(NodeSpec {
                label: "eq1".into(),
                inputs: 1,
                outputs: 1,
                kind: Some("eq".into()),
                params: Some(NodeParams::Eq {
                    filter_type: "peaking".into(),
                    freq: 2000.0,
                    gain_db: 6.0,
                    q: 1.0,
                }),
            })
            .expect("create eq");
        let gain = ctrl
            .create_node(NodeSpec {
                label: "g".into(),
                inputs: 1,
                outputs: 1,
                kind: Some("gain".into()),
                params: Some(NodeParams::Gain { gain: 0.5 }),
            })
            .expect("create gain");
        ctrl.link_ports(eq, 0, gain, 0).expect("link");
        (eq, gain)
    }

    #[test]
    fn export_preset_roundtrips() {
        let ctrl = fake_controller();
        let (eq, gain) = build_preset_graph(&ctrl);

        let preset = ctrl.export_preset();
        assert_eq!(preset.version, PRESET_VERSION);
        assert_eq!(preset.nodes.len(), 2);
        assert_eq!(preset.links.len(), 1);

        let eq_node = &preset.nodes[0];
        assert_eq!(eq_node.id, eq);
        assert_eq!(eq_node.label, "eq1");
        assert_eq!(eq_node.inputs, 1);
        assert_eq!(eq_node.outputs, 1);
        assert_eq!(eq_node.kind.as_deref(), Some("eq"));
        assert_eq!(
            eq_node.params,
            Some(NodeParams::Eq {
                filter_type: "peaking".into(),
                freq: 2000.0,
                gain_db: 6.0,
                q: 1.0,
            })
        );

        let gain_node = &preset.nodes[1];
        assert_eq!(gain_node.id, gain);
        assert_eq!(gain_node.label, "g");
        assert_eq!(gain_node.kind.as_deref(), Some("gain"));
        assert_eq!(gain_node.params, Some(NodeParams::Gain { gain: 0.5 }));

        let link = &preset.links[0];
        assert_eq!(link.from, eq);
        assert_eq!(link.from_port, 0);
        assert_eq!(link.to, gain);
        assert_eq!(link.to_port, 0);
    }

    #[test]
    fn import_preset_restores_graph() {
        let src = fake_controller();
        let (eq, gain) = build_preset_graph(&src);
        let preset = src.export_preset();

        // Fresh controller: empty store AND separate registries.
        let dst = fake_controller();
        assert!(dst.list_nodes().is_empty());
        dst.import_preset(&preset).expect("import_preset");

        let nodes = dst.list_nodes();
        assert_eq!(nodes.len(), 2, "both nodes restored");
        assert_eq!(nodes[0].id, eq);
        assert_eq!(nodes[0].label, "eq1");
        assert_eq!(nodes[0].inputs, 1);
        assert_eq!(nodes[0].outputs, 1);
        assert_eq!(nodes[0].kind.as_deref(), Some("eq"));
        assert_eq!(nodes[0].params, preset.nodes[0].params);
        assert_eq!(nodes[1].id, gain);
        assert_eq!(nodes[1].label, "g");
        assert_eq!(nodes[1].kind.as_deref(), Some("gain"));
        assert_eq!(nodes[1].params, preset.nodes[1].params);

        let links = dst.list_links();
        assert_eq!(links.len(), 1, "link restored");
        assert_eq!(links[0].from, eq);
        assert_eq!(links[0].from_port, 0);
        assert_eq!(links[0].to, gain);
        assert_eq!(links[0].to_port, 0);
    }

    #[test]
    fn import_preset_replaces_existing() {
        // Target starts with two nodes + a link that must all disappear.
        let ctrl = fake_controller();
        let a = ctrl
            .create_node(NodeSpec {
                label: "old-a".into(),
                inputs: 0,
                outputs: 1,
                kind: Some("gain".into()),
                params: None,
            })
            .unwrap();
        let b = ctrl
            .create_node(NodeSpec {
                label: "old-b".into(),
                inputs: 1,
                outputs: 0,
                kind: None,
                params: None,
            })
            .unwrap();
        ctrl.link_ports(a, 0, b, 0).unwrap();

        let src = fake_controller();
        build_preset_graph(&src);
        let preset = src.export_preset();

        ctrl.import_preset(&preset).expect("import_preset");

        let nodes = ctrl.list_nodes();
        assert_eq!(nodes.len(), 2, "only the preset's nodes remain");
        assert_eq!(nodes[0].label, "eq1");
        assert_eq!(nodes[0].kind.as_deref(), Some("eq"));
        assert_eq!(nodes[1].label, "g");
        assert!(
            !nodes
                .iter()
                .any(|n| n.label == "old-a" || n.label == "old-b"),
            "pre-existing nodes must not survive the import"
        );
        let links = ctrl.list_links();
        assert_eq!(
            links.len(),
            1,
            "old link cascaded away; only the preset's link remains"
        );
        assert_eq!(links[0].from, nodes[0].id);
        assert_eq!(links[0].to, nodes[1].id);
    }

    #[test]
    fn preset_json_file_roundtrip() {
        let ctrl = fake_controller();
        build_preset_graph(&ctrl);
        let preset = ctrl.export_preset();

        let path = std::env::temp_dir().join(format!(
            "sonicbrew-preset-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        ));
        preset.to_json_file(&path).expect("to_json_file");
        let loaded = Preset::from_json_file(&path).expect("from_json_file");
        std::fs::remove_file(&path).ok();

        assert_eq!(loaded, preset, "file roundtrip preserves the preset");
    }

    #[tokio::test]
    async fn rest_get_preset_returns_200() {
        let ctrl = Arc::new(fake_controller());
        build_preset_graph(&ctrl);
        let app = build_router(ctrl);
        let resp = app
            .oneshot(request("GET", "/preset", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 8192)
            .await
            .expect("body");
        let preset: Preset = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(preset.version, PRESET_VERSION);
        assert_eq!(preset.nodes.len(), 2);
        assert_eq!(preset.links.len(), 1);
    }

    #[tokio::test]
    async fn rest_post_preset_imports() {
        // Build a graph, export it through GET /preset...
        let src = Arc::new(fake_controller());
        build_preset_graph(&src);
        let app = build_router(Arc::clone(&src));
        let resp = app
            .oneshot(request("GET", "/preset", None))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 8192)
            .await
            .expect("body");
        let body = String::from_utf8(bytes.to_vec()).expect("utf8");

        // ...then POST it into a fresh controller's router.
        let dst = Arc::new(fake_controller());
        let app2 = build_router(Arc::clone(&dst));
        let resp2 = app2
            .oneshot(request("POST", "/preset", Some(&body)))
            .await
            .expect("oneshot");
        assert_eq!(resp2.status(), StatusCode::NO_CONTENT);

        let nodes = dst.list_nodes();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].label, "eq1");
        assert_eq!(nodes[0].kind.as_deref(), Some("eq"));
        assert!(matches!(nodes[0].params, Some(NodeParams::Eq { .. })));
        assert_eq!(nodes[1].label, "g");
        assert_eq!(nodes[1].kind.as_deref(), Some("gain"));
        assert_eq!(dst.list_links().len(), 1);
    }
}
