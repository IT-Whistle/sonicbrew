# ADR 0005 — NodeSpec typed params (REST parameterised node creation)

- **Status:** Accepted
- **Date:** 2026-08-08
- **Author:** sonicbrew contributors
- **Related:** [ADR 0004](./0004-audio-processing-nodes.md); `sonicbrew/GOVERNANCE.md` §2.2 ("control API contract change"); `PROGRESS.md` §6 (NodeSpec typed params extension)

## Context

ADR 0004 introduced six audio-processing nodes (mixer / EQ / compressor / limiter / meter / channel-map) wired into the `EngineServerFactory` via a `kind`-string dispatch. However, `NodeSpec` carried only `kind: Option<String>` — no typed parameters. Every `POST /nodes {"kind":"eq"}` created an `EqNode` with hardcoded defaults (1 kHz peaking, +0 dB, Q=0.707), and users had no REST surface to specify EQ frequency, compressor threshold, mixer gains, etc.

This was called out as a follow-up in ADR 0004's Consequences: *"A future ADR will extend `NodeSpec` with an optional `params` sub-struct (serde-defaulted for backward compatibility), and the factory will read params per kind. This is a wire-protocol change requiring its own ADR per `sonicbrew/GOVERNANCE.md` §2.2."*

Without typed params, the six processing nodes are functionally inert for real use — an EQ you cannot tune is just a passthrough.

## Decision

Add an optional, kind-specific `NodeParams` enum to `NodeSpec` / `NodeInfo`, persisted in a shared `ParamsRegistry` (mirroring the existing `KindRegistry` pattern), and consumed by the factory to construct nodes with caller-supplied parameters.

### Wire format

`NodeParams` is an externally-tagged serde enum (one variant per kind):

```json
POST /nodes
{
  "label": "low-cut",
  "inputs": 1,
  "outputs": 1,
  "kind": "eq",
  "params": {
    "Eq": {
      "filter_type": "highpass",
      "freq": 80,
      "q": 0.707
    }
  }
}
```

Every field inside each variant uses `#[serde(default)]`, so clients can send **partial** params — omitted fields fall back to the same defaults ADR 0004 established:

| Variant | Fields (all `#[serde(default)]`) | Defaults |
|---------|----------------------------------|----------|
| `Gain` | `gain: f32` | 1.0 |
| `Mixer` | `inputs: usize`, `gains: Vec<f32>` | 2, `[0.5, 0.5]` |
| `Eq` | `filter_type: String`, `freq: f32`, `gain_db: f32`, `q: f32` | "peaking", 1000, 0.0, 0.707 |
| `Compressor` | `threshold_db`, `ratio`, `attack_ms`, `release_ms`, `makeup_db` | -12, 4, 1, 50, 0 |
| `Limiter` | `threshold_db: f32` | -1.0 |
| `Meter` | (none) | — |
| `ChannelMap` | `mode: String`, `pan: Option<f32>` | "passthrough", None |

`filter_type` and `mode` are REST-friendly strings parsed by the factory into the internal enums (`FilterType`, `ChannelMode`), keeping the wire protocol language-agnostic.

### Registry + controller plumbing

- `ParamsRegistry = Arc<RwLock<HashMap<NodeId, NodeParams>>>` — a new shared side registry, mirroring `KindRegistry`.
- `GraphController` gains a `params` field; `create_node` records params after the mutation succeeds; `list_nodes` echoes them back; `delete_node` prunes the entry (alongside labels and kinds).
- New constructor `GraphController::new_with_registries(store, kinds, params)` / `RestApi::new_with_registries(store, kinds, params)` — the binary's `EngineServerFactory` reads both registries.
- `params` is `#[serde(default)]` on both `NodeSpec` and `NodeInfo`, so old clients omitting it still work (backward-compatible).

### Factory integration

The `EngineServerFactory::build` match now reads `(kind, params)` and dispatches to a module-level `render_node` function:

```rust
fn render_node(kind, params, sample_rate, channels) -> BuiltNode {
    match (kind, params) {
        (Some("eq"), Some(NodeParams::Eq { filter_type, freq, gain_db, q })) => …,
        (Some("eq"), _) => /* defaults */,
        …
    }
}
```

When `params` is `None` or the variant does not match `kind`, the factory applies the same per-kind defaults as ADR 0004 — so existing clients that POST only `{"kind":"eq"}` see no behaviour change.

## Consequences

- **Positive:** Users can now parameterise every processing node via REST — EQ frequency, compressor threshold, mixer gains, channel-map mode, etc. The six nodes from ADR 0004 are now genuinely useful.
- **Positive:** Partial params are supported (send only `freq`, keep the rest at defaults) thanks to `#[serde(default)]` on every field.
- **Positive:** Fully backward-compatible — `params` is optional on both `NodeSpec` and `NodeInfo`; old clients are unaffected.
- **Negative:** `params` is ephemeral (in-memory `ParamsRegistry`, same limitation as `kind`). It is lost on restart. Persisting it needs a `params` field on `NodeSnapshot` upstream — the same follow-up that applies to `kind`.
- **Negative:** The variant/kind pairing is not enforced at the type level — a `POST {"kind":"eq","params":{"Gain":{"gain":0.5}}` compiles and serialises but the factory falls back to EQ defaults (variant mismatch). This is acceptable (defaults, not a crash) but could be validated in a future iteration.

## Compliance

This ADR changes the `NodeSpec` / `NodeInfo` wire format (a §2.2 "control API contract change" per `GOVERNANCE.md`). The change is **additive and backward-compatible**:

- New optional field `params` (`#[serde(default)]`).
- `KindRegistry` API unchanged (new `ParamsRegistry` + `new_with_registries` added alongside, not replacing).
- `AudioNode` trait, `SessionStore` trait, `SessionStore::apply_mutation` — all unchanged.
- No change to the `NodeSnapshot` topology type (params is a side registry, same as kind).

## Related

- `crates/control-api/src/lib.rs` — `NodeParams` enum, `ParamsRegistry`, controller + REST integration, 5 new tests
- `crates/sonicbrew/src/main.rs` — `render_node` factory dispatch, `EngineServerFactory` params field
- [ADR 0004](./0004-audio-processing-nodes.md) — the six nodes whose defaults this ADR makes configurable
