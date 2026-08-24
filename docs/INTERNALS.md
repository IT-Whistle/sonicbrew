# sonicbrew Internals

Mechanics deep-dive for the runtime core. Where [ARCHITECTURE.md](./ARCHITECTURE.md)
says *what* each crate is, this document explains *how* the moving parts actually
work. Format per mechanism: **WHY** it exists → **HOW** it runs, step by step →
**invariants** it maintains → the **proving tests**.

All paths are relative to `crates/`; every symbol name below is the real one.

## 1. The live-reload pipeline

**WHY.** REST callers mutate the topology at arbitrary moments, but the RT thread
must never block, allocate, or panic inside a cycle. The pipeline therefore splits
the work across three execution contexts — async control-api (mutation), a plain
`std::thread` (rebuild), and the engine thread (activation) — connected by a
WAL-backed store and a single-slot handoff. Nothing in the audio path ever waits
on an HTTP request.

### Sequence

```
  REST client      control-api            session-store        rebuild thread        engine thread      gateway worker
      │               │                        │             ("sonicbrew-rebuild")  ("sonicbrew-rt")         │
      │ POST /nodes   │                        │                   │                     │                   │
      ├──────────────►│ create_node()          │                   │                     │                   │
      │               │ validate spec          │                   │                     │                   │
      │               │ registries (deferred)  │                   │                     │                   │
      │               ├──apply_mutation()─────►│ WAL: redb insert  │                     │                   │
      │               │                        │ + txn.commit()    │                     │                   │
      │               │                        │ snapshot.apply()  │                     │                   │
      │               │                        ├──TopologyEvent───►│ broadcast           │                   │
      │               │ 201 {id}               │ (send err benign) │ try_recv() → Ok     │                   │
      │◄──────────────┤                        │                   │ get_topology()      │                   │
      │               │                        │◄──────────────────┤                     │                   │
      │               │                        │                   │ build_graph()       │                   │
      │               │                        │                   │  factory.build(id): │                   │
      │               │                        │                   │   make_source_node ─┼──────────────────►│ fresh rtrb pair;
      │               │                        │                   │   make_sink_node    │ bridge ends swap; │
      │               │                        │                   │  compile()          │ next push/pop      │
      │               │                        │                   │                     │ uses new ring     │
      │               │                        │                   ├──Some(graph)───┐    │                   │
      │               │                        │                   │  (latest-wins) │    │                   │
      │               │                        │                   │                ▼    │                   │
      │               │                        │                   │         RebuildSlot │                   │
      │               │                        │                   │         Arc<Mutex<  │                   │
      │               │                        │                   │           Option<  │                   │
      │               │                        │                   │            Graph>>> │                   │
      │               │                        │                   │                lock().take()           │
      │               │                        │                   │                ┌────┴──────────────────►│
      │               │                        │                   │                │ step():              │
      │               │                        │                   │                │  process_cycle (RT)  │
      │               │                        │                   │                │  flush_sinks         │
      │               │                        │                   │                │  swap graph; drop    │
      │               │                        │                   │                │  old one HERE        │
```

### How, step by step

1. **REST entry.** `POST /nodes` reaches the axum handler `create_node`
   (control-api/src/lib.rs), which delegates to `GraphController::create_node`.
   It validates the spec (label non-empty; kind/params agreement — section 4),
   assigns `new_id = max(existing ids) + 1`, and calls
   `store.apply_mutation(Mutation::AddNode(snapshot))`.

2. **WAL-first mutation.** `RaftEngine::apply_mutation` (session-store/src/lib.rs)
   serializes the mutation, inserts it into the redb `MUTATIONS` table keyed by
   `MutationId` (a monotonically increasing `u64`; redb orders integer keys
   numerically, so replay order = append order) and **commits before** touching
   the in-memory snapshot — a persistence failure can never leave the snapshot
   ahead of the log. Only then does it `snapshot.apply(&mutation)` and
   `tx.send(mutation_to_event(&mutation))` on a tokio `broadcast` channel. A send
   error (no subscribers) is deliberately benign.

3. **Rebuild thread.** `spawn_rebuild_task` (audio-engine/src/lib.rs) spawns the
   `sonicbrew-rebuild` `std::thread`. It performs **one initial rebuild on
   startup** (so a topology that pre-dates the spawn is reflected), then loops on
   `try_recv()` — no tokio runtime needed, since `subscribe()`/`try_recv()` are
   plain sync operations on the broadcast channel:
   - `Ok(_event)` → `rebuild_once(...)`;
   - `Empty` → `sleep(5ms)` and poll again;
   - `Closed` → the store was dropped; exit;
   - `Lagged(n)` → events were missed under a burst; **force a fresh rebuild
     from the latest snapshot** instead of replaying — the snapshot is the source
     of truth, so missing intermediate events is harmless.

4. **`rebuild_once` → `build_graph`.** The rebuild reads
   `store.get_topology()` and calls `build_graph(&topo, config, factory)`.
   Build errors are `tracing::warn!`-ed and swallowed — a bad topology never
   kills the rebuild thread; the previously deposited graph stays in the slot.

5. **`build_graph` internals** (audio-engine/src/lib.rs):
   - **NodeId remap.** Snapshot ids can be non-contiguous (e.g. after
     `Mutation::RemoveNode`), while a fresh `Graph` assigns contiguous ids. Each
     added node's new id is recorded in `id_map: HashMap<NodeId, NodeId>`, and
     every `snapshot.edges` entry is relinked through the map. An edge pointing
     at an unknown node is a hard `EngineError::Build` (caught in step 4).
   - **`Sink` vs `Plain` registration.** The snapshot is iterated manually
     instead of using `Graph::from_snapshot`, because `from_snapshot` registers
     *every* node via `add_node`. A `RingSink` registered as a plain node is
     stored in a `NodeSlot::Plain` slot that `flush_sinks` never drains — the
     rebuilt graph would process audio but ship nothing outbound. The factory's
     `BuiltNode::Sink` variant therefore routes through `Graph::add_sink`
     (a `Flushable` `SinkNode`), and `BuiltNode::Plain` through `add_node`.
   - Finally `g.compile(config)` validates port counts/links.

6. **The slot.** `RebuildSlot = Arc<Mutex<Option<Graph>>>`. `rebuild_once`
   overwrites whatever is inside — **latest-wins**: if two mutations land before
   the engine looks, only the graph built from the newest snapshot matters, and
   intermediate graphs are silently discarded.

7. **Activation.** `GraphEngine::step` runs `process_cycle` → `flush_sinks` →
   `slot.lock().take()`. The swap is **after process+flush**, so a graph
   deposited mid-step is processed on the *next* `step` (the previous graph
   finishes its cycle in flight). The old `Graph` is dropped at the swap point,
   on the engine thread — never inside `process_cycle`.

### The gateway tension and `GatewayBridge`

A rebuild constructs *new* nodes, so the old graph's `RingSource`/`RingSink` — and
the rtrb handles inside them — die with it. A gateway worker holding its own
`Producer`/`Consumer` would be severed from the graph on every rebuild.

`GatewayBridge` (audio-engine/src/bridge.rs) inverts the ownership: the *bridge*
owns the gateway-side ends (`inbound: Mutex<Option<Producer<AudioFrame>>>`,
`outbound: Mutex<Option<Consumer<AudioFrame>>>`), locked only by the off-RT
worker. On every rebuild the factory calls `make_source_node()` /
`make_sink_node()`, which allocate a **fresh rtrb pair** (capacity 128, matching
gw-browser's `RING_CAPACITY`), box the graph-side node, and store the new
gateway-side end — the worker's next locked `push_inbound`/`pop_outbound`
transparently picks up the new ring. Migration is implicit; the worker never
learns a rebuild happened.

**Proof:** `--gateway-live-reload-test` runs `run_gateway_live_reload_test`
(sonicbrew/src/main.rs): topology bridge-source(0) → Gain(1) → bridge-sink(2);
phase A pushes a 0.5-amp sine and pops peak ≈ 0.5 (gain 1.0); phase B flips the
factory's gain to 0.5, triggers a rebuild via `Mutation::AddNode`, waits for the
slot, swaps, and pops peak ≈ 0.25 — same bridge API, new rings, audio never
stopped. Timing subtlety proven there: the bridge points at the NEW rings the
moment the factory runs, but the engine still runs the OLD graph until the next
swap — so the test must deposit, swap, then push/step/pop.

### Invariants

- WAL is written before the snapshot changes; broadcast happens after both.
- Registry entries (kind/params/label) are inserted only after `apply_mutation`
  succeeds — a failed create leaves no dangling metadata.
- The engine only ever observes complete, compiled graphs (the slot is never
  handed a half-built one).
- `Lagged` never loses data: rebuilds read the snapshot, not the event stream.
- A failed rebuild degrades to "keep running the previous graph", never silence.

### Proving tests

`restart_restores_from_wal`, `subscribe_receives_event` (session-store);
`build_graph_from_snapshot`, `engine_swaps_rebuilt_graph_between_cycles`,
`rebuild_task_rebuilds_on_topology_event`, `rebuilt_sink_is_flushed_via_add_sink`
(the Plain-sink regression guard, builds twice) (audio-engine);
`bridge_survives_rebuild` (audio-engine/src/bridge.rs);
`run_gateway_live_reload_test` (binary self-test).

## 2. The RT loop contract

**WHY.** One thread owns the live `Graph` by value and paces itself at the block
rate, keeping every allocation, lock, and drop *between* cycles.

**HOW.** `run_server_engine` (sonicbrew/src/main.rs) spawns the `sonicbrew-rt`
thread around `GraphEngine::step`:

```text
loop {
    ctx = ProcessContext::new(NUM_FRAMES, position, SAMPLE_RATE);
    t0 = Instant::now();
    eng.step(&mut ctx);                          // process_cycle + flush_sinks + swap
    recorder.record_cycle(t0.elapsed().as_micros() as u64);
    position += NUM_FRAMES as u64;
    std::thread::sleep(FRAME_DURATION);          // 256/48000 ≈ 5.33 ms
}
```

Constants: `CHANNELS = 2`, `SAMPLE_RATE = 48_000`, `NUM_FRAMES = 256`,
`FRAME_DURATION = NUM_FRAMES / SAMPLE_RATE` computed in nanoseconds.

- **`process_cycle` is alloc-free.** Nodes allocate only in constructors;
  `process` does bounded arithmetic (TESTING-STANDARDS §3.2: no alloc, no lock,
  no panic inside the cycle).
- **`flush_sinks` runs between cycles** and *does* push frames into rtrb rings;
  likewise the rebuild swap takes a brief `Mutex` lock and drops the old `Graph`
  on the engine thread. Both are acceptable because…
- **…the loop is a polling `std::thread`, not SCHED_FIFO.** Pacing is
  `sleep`-based soft-RT: no privileges, no callback deadline, and the between-
  cycle costs above are already part of the model. A hard-RT deployment would
  need a lock-free swap slot plus off-thread drop (documented as future work in
  audio-engine's `# RT-safety model` docs).
- **Metrics.** Each cycle's wall time feeds `MetricsRecorder::record_cycle`
  (monitor crate); p50/p99/xruns are exported via Prometheus `/metrics`
  (`serve_metrics`) and a 1 s log summary.

**The `flush(&mut)` vs `RtHandle(&self)` tension.** `Graph::flush_sinks` takes
`&mut self` (it mutates sink nodes to drain them). Upstream audio-graph-bsd's
`RtHandle` shares the graph behind `&self` (arc-swap `install`) — you cannot
soundly call a `&mut` flush through a shared handle. sonicbrew resolves this by
having `GraphEngine` own the `Graph` **by value**, making `&mut` natural. The
default mode's `RtHandle::install` hot path (proven by `--hot-reload-test`) was
never unified with flushing; `--server-engine` is the unified replacement. A
flush-capable shared handle remains upstream work.

**Invariants:** one cycle = exactly one `process_cycle` + one `flush_sinks` +
at most one swap; swap never interleaves with processing; metrics measure the
full step.

**Proving tests:** `engine_step_processes_and_flushes` (audio-engine);
`--gateway-live-reload-test` end-to-end; default-mode `--self-test` /
`--hot-reload-test` (binary).

## 3. The bidirectional audio path

**WHY.** A browser peer is both sender and receiver: captured frames must enter
the graph and the processed mix must come back. The two directions have
asymmetric RT constraints — inbound may be consumed *inside* a cycle, outbound
must not be pushed inside one — so they take different routes through the same
pair of rtrb rings (capacity 128 blocks ≈ 0.68 s of slack at 48 kHz/256).

### Data path

```
   WS client       gw-browser worker          RT engine thread
      │            (off-RT, locked)               │
      │ binary     decode → AudioFrame            │
      ├───────────►│ bridge.push_inbound(frame)   │
      │            │  rtrb Producer ──────────────┼────────────┐
      │            │                              │            ▼
      │            │                              │ RingSource::process (in cycle)
      │            │                              │  consumer.pop() → output port
      │            │                              │  … links: gain, eq, …
      │            │                              │ RingSink::process (in cycle)
      │            │                              │  bounded copy input → stash
      │            │                              │  (ring NOT touched here)
      │            │                              │ flush_sinks (between cycles)
      │            │                              │  RingSink::flush:
      │            │                              │   producer.push(stash.clone())
      │            │ bridge.pop_outbound()        │            │
      │            │  rtrb Consumer ◄─────────────┼────────────┘
      │ binary     encode                         │
      │◄───────────┤                              │
```

### How, step by step

1. **Inbound (client → graph).** In server-engine mode the worker is
   `BrowserGateway::serve_with_io`, whose push closure forwards each decoded
   frame to `GatewayBridge::push_inbound` — a `Mutex`-guarded `rtrb::Producer`
   locked only on the worker thread. On the next `process_cycle`,
   `RingSource::process` pops one frame and writes it to its output port;
   graph links carry it into the chain. A full ring returns
   `PushError::Full(frame)` — treated as an xrun/drop, never a block.

2. **Outbound (graph → client).** `RingSink::process` only copies its input
   port into a pre-allocated `stash` (bounded copy, RT-safe). Pushing inside
   the cycle is impossible by design: `RingSink::flush` **clones** the stash —
   an allocation — so it runs in `Graph::flush_sinks`, *between* cycles on the
   engine thread (the cost model of section 2). The worker then pops the frame
   via `bridge.pop_outbound` (locked `rtrb::Consumer`) and encodes it back to
   the client.

3. **The peak-parity proof (`--self-test`).** `run_self_test` pushes a 440 Hz
   sine at amplitude 0.5, runs 10 cycles, taps the sink's *consumed* audio via
   `Graph::read_input`, then calls `flush_sinks` and pops the outbound frame.
   Both taps print the same peak ≈ 0.4999: the frame that entered through the
   inbound ring is the frame that left through the outbound ring, unattenuated
   end-to-end. Two API subtleties the test pins down:
   - `read_input`, not `read_output` — `RingSink` declares zero output ports,
     so its output scratch is empty; the input port is the only tap.
   - the inbound ring, not `Graph::feed` — `RingSource::process` overwrites
     its single output every cycle by popping, so a seeded frame would be
     clobbered; the rtrb producer is the honest way in.

### Invariants

- `push_inbound`/`pop_outbound` are worker-side only; the RT path never
  touches the bridge `Mutex`es.
- Zero allocation inside `process_cycle`; the one per-block allocation
  (`stash.clone()`) lives in `flush_sinks`, between cycles.
- Both directions survive rebuilds untouched (section 1's `GatewayBridge`).

### Proving tests

`bridge_roundtrip`, `bridge_survives_rebuild` (audio-engine/src/bridge.rs);
`run_self_test` (binary, `--self-test`); `--gateway-live-reload-test` runs the
same loop across a gain flip and a rebuild.

## 4. Kind/params rendering

**WHY.** The store persists topology only — `NodeSnapshot` has no label, kind,
or params fields — yet rendering a REST-created node into a real DSP node needs
exactly that metadata. It therefore travels in side registries kept next to
the store and shared with the rebuild factory.

### The three registries

`GraphController` (control-api/src/lib.rs) owns three maps with deliberately
different sharing:

- `labels: RwLock<HashMap<NodeId, String>>` — **private**, per-instance;
  nodes without a recorded label fall back to `node-{id}`.
- `kinds: KindRegistry = Arc<RwLock<HashMap<NodeId, String>>>` — **shared**.
- `params: ParamsRegistry = Arc<RwLock<HashMap<NodeId, NodeParams>>>` —
  **shared**.

`run_server_engine` creates the two `Arc`s and hands the *same* clones to
`RestApi::new_with_registries` and to `EngineServerFactory` — that Arc
identity is the entire channel: `create_node` writes, the rebuild factory
reads. Entries are inserted only *after* `apply_mutation` succeeds (a failed
create leaves no dangling metadata), and `delete_node` prunes all three (the
store itself cascades incident links).

### Variant/kind agreement

`create_node` validates before mutating:

- kind present + params present + **mismatch** → `BadRequest` (HTTP 400):
  `params variant '{actual}' does not match kind '{expected}'`. Without this
  the factory would silently render per-kind defaults and the caller's
  parameters would vanish.
- kind absent + params present → the kind is **inferred** from
  `NodeParams::kind_name()` and recorded as if declared, so the kind registry,
  `list_nodes`, and the factory all agree.

### `render_node`: the 20-kind dispatch table

`render_node(kind, params, sample_rate, channels)` (binary main.rs) is one
`match (kind, params)` over 20 kinds — `gain`, `eq`, `compressor`, `limiter`,
`meter`, `mixer`, `channel_map`, `delay`, `noise_gate`, `noise`, `tone`,
`reverb`, `chorus`, `distortion`, `flanger`, `aux_send`, `phaser`,
`bitcrusher`, `tremolo`, `stereo_widener`:

- parameterized kinds match `(Some(kind), Some(NodeParams::Kind { .. }))` to
  build from the caller's parameters;
- every kind also has a `(Some(kind), _)` arm building from per-kind defaults
  (ADR 0004's default-parameter policy; e.g. bare `"gain"` renders
  `Gain(0.5)`).

The catch-all `_` renders `Gain::new(1.0)` passthrough, so an unknown or
missing kind can never hard-fail a rebuild — the node simply passes audio
through.

### The `file` exception

A decoded audio file is far too large for `NodeParams`, so kind `"file"` never
reaches `render_node`. The factory intercepts it first and consults the
`FileBufferRegistry` (`Arc<RwLock<HashMap<NodeId, FileBuffer>>>`;
`FileBuffer { samples: Vec<f32>, channels, sample_rate, looping }`, populated
once by `--load-file` seeding). Every rebuild constructs a fresh
`FileSource::new(f.samples.clone(), …)` — a full `Vec<f32>` copy per build, a
documented cost accepted at single-file scale (Arc-sharing is out of scope).
A `"file"` node whose buffer is missing degrades to `Gain::new(0.0)` silence.
One MVP quirk follows from the max-id+1 strategy: booting an existing store
with `--load-file` adds a *new* unlinked file node on every run.

### The port-channel mismatch failure mode

The seeded file `NodeSnapshot` declares the file's *real* channel count. A
mono file node linked to the stereo bridge sink therefore fails at
`build_graph`/`compile` (`PortIncompatible`) — the error is `tracing::warn!`-ed
by the rebuild task and swallowed; the server keeps running the previous
graph. This is exactly why `--load-file` seeds a channel-mismatched file node
**unlinked** (keeping the 0→1 bridge baseline) and logs a warning pointing at
REST / a `channel_map` node as the wiring path.

### Invariants

- Registries never hold entries for ids absent from the topology
  (insert-after-success, prune-on-delete).
- The factory returns `Some(BuiltNode)` for every node id — the fallback
  ladder is caller params → per-kind defaults → `Gain(1.0)` → `Gain(0.0)`
  (missing file buffer). Build failures come only from graph-level checks
  (port counts/links), never from rendering.
- kind/params are ephemeral in-memory state: a restart restores them solely
  from the preset sidecar (autosave thread, 2 s cadence, temp-file + rename),
  never from the store.

### Proving tests

`params_variant_mismatch_rejected`, `params_without_kind_infers_kind`,
`create_node_with_eq_params_roundtrips`, `create_node_applies_mutation`
(control-api); the `--load-file` seeding paths (binary).

## 5. Persistence

**WHY.** Two artifacts with different jobs: the redb WAL owns *topology*
durability (crash-safe, replay-on-open), while the preset sidecar owns
*metadata* durability (kind/params — fields `NodeSnapshot` does not carry,
section 4). One persists inside every mutation; the other is polled, diffed,
and atomically swapped by a background thread.

### The redb WAL (mutation replay)

`RaftEngine::open` (session-store/src/lib.rs) opens (or creates) the database
at `DEV_STORE_PATH` (`sonicbrew-dev.redb` under the OS temp dir), creates the
`MUTATIONS` table idempotently, and **replays** it: `u64` keys in ascending
numeric order = append order, each value a JSON-serialized `Mutation` fed to a
fresh `TopologySnapshot::apply`. `next_id` resumes at the replayed count, so
post-restart mutations append rather than overwrite. There is no compaction —
the log grows by one entry per mutation forever, accepted at MVP scale.

### The preset sidecar autosave

`run_server_engine` spawns the `sonicbrew-autosave` `std::thread`
(sonicbrew/src/main.rs), which loops forever:

1. `sleep(2s)`;
2. `ctrl.export_preset()` → `serde_json::to_string_pretty`;
3. **change detection by string compare** — if the JSON equals
   `last_written`, skip (no disk write for an idle graph);
4. write `sonicbrew-dev.preset.json.tmp`, then `fs::rename` over
   `sonicbrew-dev.preset.json` — a crash can never leave a half-written
   preset; readers see either the old or the new file, never a torn one.

Crash semantics are **latest-wins with ≤ 2 s of loss**: an abrupt `kill`
between autosaves loses at most the last two seconds of mutations (the
regression suite deliberately `sleep 3` after its REST POST before killing so
the autosave wins the race). Serialize/write failures are `warn`-ed and
retried on the next tick; the thread is detached for the process lifetime.

### Boot restore ordering

`run_server_engine` restores in a fixed order:

1. `RaftEngine::open` replays the WAL (topology only).
2. **Preset import, if the sidecar exists**: `Preset::from_json_file` →
   `GraphController::import_preset`, which *replaces* the replayed topology
   with the exported state — the preset is strictly fresher-shaped than the
   store because it also carries kind/params. A failed import is a `warn`,
   and boot continues with the WAL-restored topology.
3. **Empty-topology guard**: only if `topo.nodes.is_empty()` after steps 1–2
   does the bridge seeding run (`AddNode` ids 0 and 1 + the baseline
   `AddLink 0→1`, or the file node taking the baseline's place). A restored
   graph is never re-seeded, so id 0/1 reservation applies only to a virgin
   store.

Order matters: seeding *before* the preset import would be wiped by the
import's replace; importing *after* seeding would duplicate nodes under
fresh ids.

### The `--load-file` accumulation caveat

`--load-file` seeds *one* file node per boot, but the seeded id is
`max(existing ids) + 1`. On a fresh store the file lands at id 2 (0/1
reserved by the bridge), linked to the sink only when its channel count
matches `CHANNELS` (otherwise unlinked + warned, section 4). On an *existing*
store — which includes "booted with a preset" — the `else` branch adds a
**new unlinked file node on every run**: restart with the same flag N times,
collect N unlinked file nodes at ids max+1, max+2, … Wiring them is
REST-owned. This is documented MVP behavior, not a bug being hidden.

### Proving paths

`restart_restores_from_wal` (session-store); the server-engine smoke section
of `scripts/freebsd-regression.sh` (POST → sleep → kill → restart → GET
shows the eq node) exercises autosave + restore end-to-end on the FreeBSD
host.

## 6. Session consensus

**WHY.** One `SessionStore` trait, two engines: a single-node MVP that is a
glorified WAL, and a real Raft replica for multi-node. They are **distinct
types** — reverting to single-node is simply not constructing the distributed
one; no feature flag or config switch exists.

### Single-node: `RaftEngine`

What the server binary actually uses today. `EngineState` under one
`std::sync::Mutex` holds the snapshot, `next_id`, the `redb::Database`, and a
64-slot tokio `broadcast::Sender<TopologyEvent>`. `apply_mutation` is
WAL-first (insert + commit before `snapshot.apply`), then a best-effort
`tx.send` — a send error with no subscribers is benign. "Raft" in the name is
aspirational: a single node is trivially its own leader, and there is no
election, no log replication, no snapshot shipping.

### Multi-node: `DistributedRaftEngine` (P1, openraft 0.9.25)

The real consensus engine (session-store/src/distributed.rs + the four
`raft_*` modules, per ADR-0003):

- `raft_types.rs` — `TypeConfig` via openraft's `declare_raft_types!`, with
  `AppData = Mutation` and `AppDataResponse = ClientWriteResponse`
  (carrying `mutation_id`).
- `raft_log_store.rs` — `RaftLogStore` (ephemeral-per-node log storage).
- `raft_state_machine.rs` — `StateMachine` applies committed `Mutation`s into
  its own redb database, plus a `StateMachineReader` taken *before* the state
  machine moves into `Raft::new`, sharing that database.
- `raft_network.rs` — `LoopbackNetworkFactory`, an in-process network for
  tests.

Two bridges make it usable from the synchronous trait:

- **Sync↔async.** `apply_mutation` spawns `raft.client_write(mutation)` on
  the tokio `Handle` and blocks the caller on a `std::sync::mpsc`
  `sync_channel(1)` — blocking on a std channel is safe even when the caller
  is itself on the runtime (it never drives the reactor from a worker
  thread, which `Handle::block_on` would). The returned `mutation_id` is
  extracted from `ClientWriteResponse::data`; the topology event fans out
  locally after the write returns.
- **Reads.** `get_topology` reads the applied topology straight from the
  shared redb via `StateMachineReader` — no async round trip, always the
  latest locally committed+applied state.

`spawn_cluster(node_ids)` builds an N-node in-process cluster (per-node
ephemeral log store + state machine, shared loopback network, single
`Raft::initialize` on the first node with the full voter set; election
timings 80–120 ms so tests converge fast).

### When each is used

The `sonicbrew` binary constructs `RaftEngine::open` — full stop.
`DistributedRaftEngine` exists at the library level, proven by
`tests/cluster.rs` (`three_node_cluster_elects_a_leader`, replicated writes,
`cluster_survives_leader_loss_and_re_elects`). Promotion to the server is a
future decision point (ADR-0003 §6), not a runtime switch.

## 7. Monitor & metrics

**WHY.** The RT loop needs an O(1), allocation-free way to deposit one
number per cycle; operators need Prometheus text on a scrape port. Both
sides get exactly that and nothing heavier.

### `MetricsRecorder` (monitor/src/lib.rs)

- **Sliding window.** `LatencyHistogram` = `Mutex<VecDeque<u64>>`
  pre-allocated to `LATENCY_WINDOW = 1024` samples, so the first window-full
  of pushes never reallocates. `record` is a bounded `push_back` + optional
  `pop_front` — O(1) under the lock. (A lock-free sharded histogram is a
  documented later optimization; the `std::Mutex` is the accepted P1 cost.)
- **Two entry points.** `record_cycle(us)` (inherent, *not* on the trait) is
  what `sonicbrew-rt` calls once per cycle with the measured step time.
  `record_latency(p50, p99)` (the `MetricsSink` trait method) serves callers
  who already computed percentiles: it stores them into `last_p50`/`last_p99`
  `AtomicU64`s and feeds both values into the window.
- **Export.** `export_prometheus` snapshots the window (never called from
  the RT path), computes min/max/avg, and renders a `summary`
  (`sonicbrew_process_latency_us{quantile="0.5"|"0.99"}` from the last-
  recorded atomics, plus `_min`/`_max`/`_avg` gauges from the window) and a
  `sonicbrew_xrun_total` counter. The metric-name prefix is configurable
  (`with_prefix`) for multi-node deployments.
- **Xrun counting.** `record_xrun(count)` takes the *cumulative* total from
  the engine and keeps the stored value monotonically non-decreasing via a
  CAS max-loop — a Prometheus `counter` must never go backwards, so a stale
  or out-of-order report is ignored rather than subtracting.

### `serve_metrics`: raw HTTP on purpose

`serve_metrics` (monitor) is a hand-rolled TCP loop: accept, read the
request line best-effort into 256 bytes under a 500 ms timeout (the bytes
are intentionally *not inspected* — the response is identical for any
request), write `200 text/plain; version=0.0.4` + the exposition body,
close. No axum, no hyper, no routing: the endpoint has exactly one consumer
(a Prometheus scraper at ~15 s cadence), so a framework dependency would buy
nothing and drag a server-sized dep tree into every binary linking monitor.
The 1 s log summary in `run_server_engine` reads the same recorder.

`NoopSink` is the zero-cost stand-in for builds that want the trait without
the recorder; `spawn_kqueue_loop` is a `cfg(freebsd)` stub that returns an
error — the kqueue event loop is a later FreeBSD-only optimization and the
crate deliberately carries no `nix` dependency (Linux build compatibility).

### Proving tests

`record_latency_updates_window`, `export_prometheus_format`,
`xrun_counter_accumulates` (the monotonicity guard), `percentile_edge_cases`
(empty / single / window wrap at 1024+512 pushes), `custom_prefix_is_applied`
(monitor).

## 8. Gateway implementations compared

**WHY.** Three gateways attack the same problem — get external audio into
and out of the graph — from three different host ecosystems, so their
transports, process models, and insertion contracts differ sharply. The
table is the map; the notes under it are the territory.

| | gw-browser (M12) | gw-pulse (M10) | gw-alsa (M11/P2) |
|---|---|---|---|
| **Transport** | WebSocket (tokio-tungstenite), binary frames: 6-byte LE header (`codec_tag u8` 0=PCM/1=Opus, `channels u8`, `sample_rate u32`) + planar f32 payload | PulseAudio **native protocol v35** over a Unix socket (`$PULSE_SERVER` → `$XDG_RUNTIME_DIR/pulse/native` → `/var/run/pulse/native` …), pure Rust — **no libpulse** | libasound **ioplug `.so`** (`libasound_module_pcm_sonicbrew.so`) `dlopen`ed inside the ALSA client, bridging over **local TCP** with a 5-word handshake (`BRIDGE_MAGIC 0x5342_4E52`, proto version, stream dir, channels, rate) |
| **Process model** | async tokio task per server (`serve_with_io` closures over an `Arc<GatewayBridge>`); decode/encode off the RT thread | blocking I/O with a 5 s timeout; must be driven from a worker thread only | in-process inside the *client* application: libasound calls the plugin's `transfer` callback synchronously; eager connect (`snd_pcm_open` fails `-EIO` when no bridge answers) |
| **Insertion contract** | `RingSource`/`RingSink` (rtrb, capacity 128) handed out by `BrowserGateway::register`; in server-engine mode wired through `GatewayBridge::push_inbound`/`pop_outbound` (section 3) | `PulseGateway::register` wires the graph rings, then `Gateway::run` returns `Unimplemented("live daemon connection deferred")` — the daemon client (`daemon.rs`) is exercised by examples, not yet pumped by the server loop | plugin-side `BridgeStream` (pure-Rust bridge module) talks f32 interleaved LE over the socket; server side feeds the same ring pair the browser path uses |
| **Verified proof** | `--self-test` peak parity + `--gateway-live-reload-test` (bridge survives rebuild); `serve_with_io_listener` loopback tests | `examples/handshake` against **real PulseAudio 17.0**: AUTH (256-byte cookie) + `SET_CLIENT_NAME` + `SERVER_INFO`; `--play` pushed 144,000 frames (3 s sine) as pstream **memblocks** (20-byte descriptor, channel = stream index — *not* `0xFFFFFFFF`) → `PLAY_OK` | **aplay end-to-end** on FreeBSD: libasound dlopens the `.so` → `FLOAT_LE` negotiation → TCP handshake → 24,000 f32 frames received; `abi_layout_matches_alsa` tests pin every ioplug struct offset against alsa-lib 1.2.13 (frozen since protocol 1.0.2) |

Three cross-cutting notes:

- **gw-pulse's no-libpulse stance is a design decision, not a shortcut**: the
  native protocol is documented, the crate already owns a parser for it
  (`codec.rs`), and avoiding the LGPL library keeps the wire layer
  dependency-free and builds identical on Linux and FreeBSD. The cost is
  owning tagstruct details upstream C code normally hides (ASCII tags,
  NUL-terminated strings, proplist double-encoding, cvolume raw fields) —
  each was a real bug fixed against the C sources.
- **gw-alsa's ABI is mirrored by hand**, not bound: `snd_pcm_ioplug_t` and
  the callback table are `#[repr(C)]` copies with offset-pinning tests, so
  drift in either alsa-lib or this crate fails loudly at test time. In
  `no_alsa_link` builds (dev host without libasound2-dev) the extern
  declarations degrade to logging stubs — the `.so` still builds and still
  exports `_snd_pcm_sonicbrew_open` (verified with `nm -D`).
- **Only gw-browser is live in the server loop today.** gw-pulse and
  gw-alsa's proofs run through their examples/tests on the FreeBSD host; the
  production pump that would drive them from `run_server_engine` is future
  wiring, not hidden dead code.

## 9. Failure modes & diagnostics

The five failure modes that actually bit, what they look like from the
outside, and what the system does about each.

### Port-channel mismatch on rebuild

A node snapshot whose port channel count disagrees with its link target
(mono file node → stereo bridge sink) fails `build_graph`/`compile` with
`PortIncompatible`. The rebuild task `tracing::warn!`s the error and
**swallows it** — the previously deposited graph stays in the slot, the
server keeps processing audio, REST keeps answering. Diagnosis: the warn
line names the failing edge; the fix is REST wiring (e.g. a `channel_map`
node), not a restart. Section 4's `--load-file` seeding avoids the trap
proactively by leaving mismatched files unlinked.

### redb lock contention on fast restart

redb holds an exclusive file lock on the database. `kill` + immediate
restart races the OS releasing that lock, and the fresh process dies on
`open redb` with a lock error — *especially* right after a VM reboot when
teardown is slow. The regression suite's fix (§5 of
`scripts/freebsd-regression.sh`) is the operational contract: after
`kill`/`wait`, **poll `pgrep` until the process is truly gone (up to 10 s at
0.5 s intervals) plus one more second** before relaunching. Any restart
tooling must copy this wait, or treat a lock-error boot as "retry shortly",
not as data corruption.

### Ring overflow → xrun drop policy

When the inbound rtrb ring is full (the graph is not draining fast enough,
or a client is pushing faster than 48 kHz real-time),
`GatewayBridge::push_inbound` returns `PushError::Full(frame)` — the frame
is **dropped**, counted as an xrun, and the worker moves on. Nothing blocks,
nothing panics, no backpressure signal is propagated to the client. This is
the deliberate drop-over-delay choice for live audio: a late frame is
worthless, a blocked worker would cascade into the WS connection.

### "Machine down" false positives — the VPN postmortem

During VALE/netmap debugging on 2026-08-18 the FreeBSD host appeared to
go unreachable mid-experiment — the tempting conclusion was that the
zero-copy ring experiments had panicked the kernel. After the host returned
(new DHCP lease), forensics said otherwise: `/var/crash` was empty,
`/var/log/messages` contained no panic lines, and the full regression suite
passed unchanged. The outage was a **client-side VPN dropout**; the netmap
work was never the cause. The durable lesson (and the reason this section
exists): an "unreachable" window is not evidence of a crash — check the
crash artifacts before blaming the most recent scary experiment. One
hardening from the investigation was kept anyway (sync ioctls pass `0`,
not `ptr::null_mut()`).

### `LinkId` positional shift after deletes

`LinkId` is a **positional index** into `TopologySnapshot::edges`, not a
stable identifier. `Mutation::RemoveLink(id)` removes `edges[id]` and
shifts every later edge down by one — so a `LinkId` captured before a delete
silently refers to a *different* link afterwards. Two defenses exist:
`delete_link` range-checks against the current edge count (an out-of-range
id surfaces as 404 `NotFound` rather than the store's silent no-op), and the
REST layer returns fresh ids from every `POST /links`. There is **no
defense against use of a stale-but-in-range id** — a client holding link 2
across another client's delete of link 1 will delete what is now a
different edge. Long-lived clients must re-`GET /links` after any concurrent
mutation; a stable link id is upstream work (it needs an id field in
`SnapshotEdge`).

---

**Related documents:** [ARCHITECTURE](./ARCHITECTURE.md) ·
[REST-API](./REST-API.md) · [RUNBOOK](./RUNBOOK.md) ·
[PROGRESS](./PROGRESS.md) · [TEST-LAYERS](./TEST-LAYERS.md) ·
[adr/](./adr/)
