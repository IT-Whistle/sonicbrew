# sonicbrew REST API Reference

> Control plane of control-api (M13). Base address `127.0.0.1:9002` (change with `--api-addr`). Prometheus metrics are on a separate port, `9003` (`GET /metrics`).

## Endpoint Summary

| Method | Path | Function |
|--------|------|------|
| GET | `/nodes` | List nodes |
| POST | `/nodes` | Create node (kind + params) |
| DELETE | `/nodes/:id` | Delete node (links cascade) |
| GET | `/links` | List links |
| POST | `/links` | Create link (ports can be specified) |
| DELETE | `/links/:id` | Delete link |
| GET | `/topology` | Full graph snapshot (nodes+links in 1 request) |
| GET | `/preset` | Export preset (full state) as JSON |
| POST | `/preset` | Import preset — replaces the entire graph |

## Nodes

### `GET /nodes` → 200

```json
[{"id":0,"label":"node-0","inputs":0,"outputs":1,"kind":null,"params":null},
 {"id":2,"label":"my-eq","inputs":1,"outputs":1,"kind":"eq",
  "params":{"Eq":{"filter_type":"peaking","freq":2000.0,"gain_db":3.0,"q":0.9}}}]
```

### `POST /nodes` → 201 `{"id": N}`

Request body `NodeSpec`:

| Field | Type | Required | Description |
|------|------|:---:|------|
| `label` | string | ✅ | Empty string/whitespace → 400 |
| `inputs` / `outputs` | u16 | ✅ | Number of ports |
| `kind` | string | — | Node kind (20 kinds — [AUDIO-NODES.md](./AUDIO-NODES.md)) |
| `params` | [NodeParams](#nodeparams-variants) | — | Per-kind parameters. **Omitted/partial fields are both allowed** (`#[serde(default)]`) |

**Validation rules:**
- `kind` and `params` variant mismatch (`{"kind":"eq","params":{"Gain":{...}}}`) → **400**
- `kind` omitted + `params` provided → kind is **inferred automatically** from the params variant
- Node id is `max(existing id) + 1` (1 for an empty graph)

```bash
# Example: parameterized EQ node
curl -X POST http://127.0.0.1:9002/nodes \
  -H "content-type: application/json" \
  -d '{"label":"low-cut","inputs":1,"outputs":1,"kind":"eq",
       "params":{"Eq":{"filter_type":"highpass","freq":80}}}'
```

### `DELETE /nodes/:id` → 204 / 404

Deletes the node. Connected links are cascaded automatically by the store.

## Links

### `GET /links` → 200

```json
[{"id":0,"from":0,"from_port":0,"to":2,"to_port":1}]
```

### `POST /links` → 201 `{"id": N}`

Request body `LinkRequest`:

| Field | Type | Default | Description |
|------|------|:---:|------|
| `from` / `to` | u64 | ✅ | Source/target node id |
| `from_port` / `to_port` | u16 | 0 | **Port index** — targets an individual port of multi-port nodes (mixer input 1, AuxSend aux output, etc.) |

**Validation:** missing node → 404, port out of range → 400 (with a diagnostic message).

```bash
# Example: route a source to the mixer's second input (port 1)
curl -X POST http://127.0.0.1:9002/links \
  -H "content-type: application/json" \
  -d '{"from":3,"from_port":0,"to":5,"to_port":1}'
```

### `DELETE /links/:id` → 204 / 404

**Caution:** `LinkId`s are positional indices — deleting shifts down the ids of subsequent links. Clients must re-fetch the topology after changes.

## Topology / Preset

### `GET /topology` → 200

Nodes and links in one shot: `{"nodes":[NodeInfo...], "links":[LinkInfo...]}`

### `GET /preset` → 200

A `Preset` serializing the full graph state (including each node's kind/params/label). Saving to a file is also possible programmatically via `Preset::to_json_file`.

```json
{"version":1,
 "nodes":[{"id":2,"label":"my-eq","inputs":1,"outputs":1,"kind":"eq",
           "params":{"Eq":{"filter_type":"peaking","freq":2000.0,"gain_db":3.0,"q":0.9}}}],
 "links":[{"from":0,"from_port":0,"to":1,"to_port":0}]}
```

### `POST /preset` → 204

**Replaces the entire graph** with the body's `Preset` (existing nodes/links are removed then recreated, registries restored). server-engine mode auto-saves this preset every 2s and restores it on restart ([RUNBOOK.md](./RUNBOOK.md) §Persistence).

**Caution:** import is not transactional — a mid-way failure can leave a partially applied state.

## Error Mapping

| ControlError | HTTP |
|--------------|------|
| BadRequest (empty label, port out of range, kind/params mismatch) | 400 |
| NotFound (missing node/link) | 404 |
| Graph (store rejection) | 422 |
| Unimplemented (load_module etc.) | 501 |

## NodeParams Variants

Externally tagged serde representation: `"params":{"<Variant>":{...}}`. All fields are `#[serde(default)]` — omitted fields take their defaults when partially provided.

| kind | Variant | Fields (defaults) |
|------|------|---------------|
| `gain` | `Gain` | gain (1.0) |
| `mixer` | `Mixer` | inputs (2), gains ([0.5, 0.5]) |
| `eq` | `Eq` | filter_type ("peaking"), freq (1000), gain_db (0), q (0.707) |
| `compressor` | `Compressor` | threshold_db (-12), ratio (4), attack_ms (1), release_ms (50), makeup_db (0) |
| `limiter` | `Limiter` | threshold_db (-1) |
| `meter` | `Meter` | (none) |
| `channel_map` | `ChannelMap` | mode ("passthrough"), pan (null) |
| `delay` | `Delay` | max_delay_ms (500), delay_ms (250), feedback (0.3), mix (0.3) |
| `noise_gate` | `NoiseGate` | threshold_db (-50), attack_ms (1), hold_ms (50), release_ms (100) |
| `noise` | `Noise` | color ("white"), amp (0.5), seed (12345) |
| `tone` | `Tone` | waveform ("sine"), freq (440), amp (0.5) |
| `reverb` | `Reverb` | room_size (0.5), damping (0.5), wet (0.3), dry (0.7) |
| `chorus` | `Chorus` | rate_hz (1.5), depth_ms (3), center_delay_ms (20), mix (0.5) |
| `flanger` | `Flanger` | rate_hz (0.5), depth_ms (2), center_delay_ms (3), feedback (0.5), mix (0.5) |
| `phaser` | `Phaser` | rate_hz (0.5), base_freq (800), depth (0.5), feedback (0.3), mix (0.5), stages (4) |
| `distortion` | `Distortion` | mode ("soft_clip"), drive (3), threshold (0.7), output_level (1.0) |
| `bitcrusher` | `Bitcrusher` | bits (8), hold_factor (1) |
| `tremolo` | `Tremolo` | rate_hz (5), depth (0.5) |
| `stereo_widener` | `StereoWidener` | width (1.0) |
| `aux_send` | `AuxSend` | send_level (0.5) |

String enum values: `filter_type` = lowpass/highpass/bandpass/peaking/lowshelf/highshelf · `mode` = passthrough/swap/mute_left/mute_right/pan/mono_to_stereo/stereo_to_mono · `color` = white/pink · `waveform` = sine/square/saw/triangle · distortion `mode` = soft_clip/hard_clip/foldback/overdrive

`kind: "file"` (FileSource) cannot be created via REST params — only via the binary's `--load-file` or the programmatic API (`load_file_source`) ([RUNBOOK.md](./RUNBOOK.md)).
