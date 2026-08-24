# sonicbrew Operations Guide (RUNBOOK)

> Build · tests · running the server · persistence · troubleshooting.

## 1. Build / Quality Gates

```bash
# Linux dev host (no audio-toolkit sibling needed — consumes crates.io)
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                    # 556 passed
cargo check --workspace --target x86_64-unknown-freebsd   # cross typecheck
```

Per-crate: `cargo test -p control-api` (94) · `-p audio-engine` (100) · `-p session-store` (34) etc — full list in [TEST-LAYERS.md](./TEST-LAYERS.md).

## 2. Running the Server

### Default mode
```bash
cargo run -p sonicbrew -- --ws-addr 127.0.0.1:9001 \
                           --api-addr 127.0.0.1:9002 \
                           --metrics-addr 127.0.0.1:9003
```
RT loop + WebSocket gateway + REST + `/metrics`.

### server-engine mode (live reload, recommended)
```bash
cargo run -p sonicbrew -- --server-engine [--load-file song.wav] [address options]
```
- REST topology change → `TopologyEvent` → graph rebuild → swap between cycles → **gateways survive**
- `--load-file <PATH>`: loads a FLAC/WAV/PCM file as a FileSource node. If the file's channel count matches the graph (2ch), it is auto-connected to the sink; on mismatch it is added as an unconnected node (warning log) and connected via REST. Note that every reboot adds a new file node (when the existing topology is preserved).

### Default addresses
WS `127.0.0.1:9001` · REST `127.0.0.1:9002` · metrics `127.0.0.1:9003`

## 3. Deterministic self-tests (no server)

```bash
cargo run -p sonicbrew -- --self-test                 # bidirectional flush + monitor (peak-match verification)
cargo run -p sonicbrew -- --hot-reload-test           # RtHandle arc-swap live replacement
cargo run -p sonicbrew -- --live-rebuild-test         # from_snapshot topology rebuild
cargo run -p sonicbrew -- --engine-live-rebuild-test  # full store→event→rebuild→swap chain
cargo run -p sonicbrew -- --gateway-live-reload-test  # gateway bridge survival verification
```

## 4. Persistence

| File (temp_dir) | Contents |
|------------------|------|
| `sonicbrew-dev.redb` | session store WAL — topology (nodes/edges) |
| `sonicbrew-dev.preset.json` | preset sidecar — full state + **kind/params/labels** |

server-engine mode auto-saves the preset every 2s (writes only on change detection, atomic tmp+rename). At boot, if the sidecar exists the graph is restored — **parameters such as EQ frequency survive restarts**. On forced termination up to 2s of changes can be lost.

To reset, delete both files and boot.

## 5. Typical Workflow

```bash
# 1) Boot the server
cargo run -p sonicbrew -- --server-engine &

# 2) Build an effect chain: bridge_src(0) → EQ → Reverb → bridge_sink(1)
curl -X POST :9002/nodes -H "content-type: application/json" \
  -d '{"label":"eq","inputs":1,"outputs":1,"kind":"eq","params":{"Eq":{"freq":12000,"filter_type":"lowpass"}}}'
#   → {"id":2}
curl -X POST :9002/nodes -d '{"label":"verb","inputs":1,"outputs":1,"kind":"reverb","params":{"Reverb":{"room_size":0.7,"wet":0.4}}}'
#   → {"id":3}
curl -X POST :9002/links -d '{"from":0,"to":2}'
curl -X POST :9002/links -d '{"from":2,"to":3}'
curl -X POST :9002/links -d '{"from":3,"to":1}'

# 3) Verify — live reload is automatic (no server interruption)
curl :9002/topology

# 4) Metrics
curl :9003/metrics
```

The browser exchanges binary PCM (6-byte-header wire format) over WS `:9001`.

## 6. Diagnostics

- **RT processing/flush errors**: `engine process_cycle failed` / `flush_sinks reported` in the logs — suspect xruns; check `process_latency_us_*` · `xrun_total` on `/metrics`
- **Rebuild failure**: `rebuild: build_graph failed` — a port channel-count mismatch is the main cause (e.g. mono file node → 2ch sink). Check ports with `GET /topology` and rewire the links
- **"(2,1) rebuild never landed" at server boot**: port mismatch in the stored topology — reset by deleting the preset/redb
- **`--load-file` file accumulation**: booting with an existing topology preserved adds a new file node every run — clean up with `DELETE /nodes/:id`

## 7. Optional features

```bash
cargo check -p sonicbrew --features bluetooth   # BT A2DP input (FreeBSD runtime)
cargo check -p gw-browser --features opus       # Opus sub-path (links libopus)
cargo check -p sonicbrew --features diagnose    # signal waveform TUI
```

## 8. FreeBSD (VM tier)

On a Linux dev host, `cargo check --target x86_64-unknown-freebsd` is as far as you can go. Full build/run regression (xrun/alloc), netmap RTP, and libpulse/libasound FFI run on a bhyve FreeBSD 14.2 VM (TESTING-STANDARDS Layer 4 — see the §5 gap in [TEST-LAYERS.md](./TEST-LAYERS.md)).
