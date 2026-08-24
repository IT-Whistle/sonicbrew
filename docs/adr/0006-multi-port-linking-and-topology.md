# ADR 0006 — Multi-port linking + link listing + topology endpoint

- **Status:** Accepted
- **Date:** 2026-08-08
- **Author:** sonicbrew contributors
- **Related:** [ADR 0005](./0005-nodespec-typed-params.md); `sonicbrew/GOVERNANCE.md` §2.2 ("control API contract change")

## Context

ADR 0004 introduced six audio-processing nodes and ADR 0005 made them parameterisable. However, the control API had two limitations that prevented real audio routing:

1. **Port-0-only linking.** `ControlApi::link(from, to)` hardcoded `SnapshotEdge { from: (from, 0), to: (to, 0) }`. A 2-input `MixerNode` could only receive audio on input port 0 — the second input was unreachable. An `AuxSendNode` (1-in / 2-out) had its aux output port unreachable. This made multi-port nodes functionally single-port.

2. **No link introspection.** The REST surface offered `GET /nodes`, `POST /nodes`, `DELETE /nodes/:id`, `POST /links`, `DELETE /links/:id` — but **no way to list existing links**. A client that created links could not read them back, making it impossible to render the graph topology or verify routing state. There was also no single endpoint returning the complete graph (nodes + links) in one request.

## Decision

### Multi-port linking

Extend `LinkRequest` with optional port indices:

```json
POST /links
{
  "from": 3,
  "from_port": 0,
  "to": 5,
  "to_port": 1
}
```

- `from_port` / `to_port` are `Option<u16>` with `#[serde(default)]` → default to port 0 when omitted.
- The REST handler extracts the port indices (unwrap to 0) and calls `GraphController::link_ports(from, from_port, to, to_port)`.
- `link_ports` validates that both nodes exist and that the port indices are in range before applying the mutation. Returns `NotFound` for a missing node, `BadRequest` for an out-of-range port.
- The existing `ControlApi::link(from, to)` trait method delegates to `link_ports(from, 0, to, 0)` — fully backward-compatible.

### Link listing

Add `ControlApi::list_links() -> Vec<LinkInfo>` and expose it as `GET /links`:

```json
GET /links → 200
[
  {"id":0,"from":3,"from_port":0,"to":5,"to_port":1},
  {"id":1,"from":4,"from_port":0,"to":5,"to_port":0}
]
```

`LinkInfo` carries the positional `LinkId`, source/destination `NodeId`, and the port indices for each end.

### Topology snapshot

Add `GET /topology` returning both nodes and links in a single response:

```json
GET /topology → 200
{
  "nodes": [ {"id":1,"label":"src","inputs":0,"outputs":1,...}, ... ],
  "links": [ {"id":0,"from":1,"from_port":0,"to":2,"to_port":0}, ... ]
}
```

`TopologyInfo { nodes: Vec<NodeInfo>, links: Vec<LinkInfo> }` is the one-shot graph state — clients no longer need two round-trips to render the full topology.

## Consequences

- **Positive:** Multi-port nodes (mixer inputs, aux-send outputs) are now fully routable. The REST surface is CRUD-complete (GET for both nodes and links). A single `GET /topology` gives the complete graph state.
- **Positive:** Port-range validation prevents silent misconfiguration — an out-of-range `to_port` returns a precise `400 BadRequest` with a diagnostic message, not a graph compile error at rebuild time.
- **Positive:** Fully backward-compatible — old clients omitting `from_port`/`to_port` see no change.
- **Negative:** `LinkId` remains positional (index into the edge vector). Removing a link still shifts later ids. Clients must re-fetch topology after any mutation that changes edges. This is a pre-existing limitation, not introduced by this ADR.

## Compliance

This ADR changes the `LinkRequest` wire format and adds two new response types (`LinkInfo`, `TopologyInfo`) and two new endpoints (`GET /links`, `GET /topology`). All changes are **additive and backward-compatible**:

- `LinkRequest` new fields are `#[serde(default)]`.
- `ControlApi` gains `list_links()` with a default trait method (existing implementors unaffected).
- No change to `NodeSpec`, `NodeInfo`, `NodeSnapshot`, `SessionStore`, or `AudioNode`.

## Related

- `crates/control-api/src/lib.rs` — `LinkInfo`, `TopologyInfo`, `list_links`, `link_ports`, `GET /links`, `GET /topology`
- `crates/audio-engine/src/nodes/aux_send.rs` — `AuxSendNode` (1-in/2-out, enabled by multi-port linking)
- [ADR 0005](./0005-nodespec-typed-params.md) — typed params (the parameter system this routing builds upon)
