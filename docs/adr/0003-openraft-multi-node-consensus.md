# ADR 0003 — openraft multi-node consensus for session-store (M07)

- **Status:** Accepted
- **Date:** 2026-08-04
- **Author:** sonicbrew contributors
- **Supersedes (for this repo):** `BUILD-PLAN.md` §2.1 single-node deferral comment (`session-store/src/lib.rs` lines 6–7, 92–93)
- **Related:** `audio-graph-bsd` 0.4.0 `topology_pub.rs` (serde derives for `Mutation` / `TopologySnapshot`)

## Context

`session-store` (M07) is currently a **single-node** topology store: a `redb`
WAL of mutations (`MUTATIONS` table, `u64 → Vec<u8>`), an in-memory
`TopologySnapshot`, and a `tokio::sync::broadcast` fan-out for
`TopologyEvent`. It is 405 lines with 8 tests. Its own module doc states the
openraft single-node self-leader is **deferred to P1** (see `BUILD-PLAN.md`
§2.1; comment in `session-store/src/lib.rs:6` and `:92`). `control-api` (M13)
consumes it synchronously via `Arc<dyn SessionStore>` — the `SessionStore`
trait is object-safe and its three methods (`get_topology`, `apply_mutation`,
`subscribe`) are all **synchronous**.

Three facts make lifting the deferral a GOVERNANCE-gated decision rather than
a code-only change:

1. **`sonicbrew/GOVERNANCE.md` §2.2** triggers an ADR for: openraft mode
   transition (single-node → multi-node), WAL/table schema change, and
   `SessionStore` trait contract change — all three (lines 51 and 55). The
   umbrella `GOVERNANCE.md` §2.2 independently triggers an ADR for storage
   schema changes (line 61). This ADR must therefore address **all three**
   explicitly, not just "consensus".

2. **`audio-graph-bsd` 0.4.0 already supplies the log/snapshot types with
   serde derives.** `Mutation` is
   `#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]`
   and `TopologySnapshot` is
   `#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]`
   (`topology_pub.rs:260` and `:165`). `TopologySnapshot::apply(&mutation)`
   already implements the state-machine apply step (`:200`). A Raft log entry
   can be a `Mutation` directly; a Raft snapshot is a serialized
   `TopologySnapshot` plus its last-applied `LogId`.

3. **`BUILD-PLAN.md` §1 already declares `openraft = "0.9"`** (line 109), but
   the actual `Cargo.toml` does **not** reflect it. The single-node engine
   (`RaftEngine`) currently uses a plain `redb` WAL without openraft at all.
   Lifting the deferral therefore also reconciles the manifest with the plan.

The dev host is Linux x86_64; FreeBSD is verified only by `cargo check
--target x86_64-unknown-freebsd` (full cross-build/run needs a bhyve VM, see
`CONTRIBUTING.md` §4.1). Multi-node Raft tests run on Linux; the FreeBSD gate
remains a compile check.

## Decision

1. **Consensus mode.** Keep the current single-node `RaftEngine` (redb WAL)
   as-is for backward compatibility, and add a new
   **`DistributedRaftEngine`** based on **openraft 0.9.x** (target the latest
   0.9.21 patch). Use **static bootstrap** via `Raft::initialize(members)`
   with a configurable node count (default 3). Dynamic join/leave
   (`add_learner`/`change_membership`) is explicitly **out of scope** for this
   ADR and tracked as a separate P2 task.

2. **redb schema change (openraft tables).** Add four new tables on a
   **separate redb database file** owned by `DistributedRaftEngine`:

   | Table             | Key → Value                         | Purpose                           |
   |---|---|---|
   | `logs`            | `LogId(u64,u64)` → serialized entry | Raft log entries                  |
   | `hard_state`      | `()` → `HardState` (single row)     | committed term/vote               |
   | `membership`      | `()` → `Membership` (single row)    | current cluster config            |
   | `current_snapshot`| `()` → `SnapshotMeta` (single row)  | pointer to installed snapshot     |

   The existing single-node `MUTATIONS` table (u64 → Vec<u8>) is **preserved
   unchanged** on the original DB file. The two engines never share a file,
   so the single-node engine stays byte-compatible.

3. **`SessionStore` trait contract.** The public trait
   (`get_topology`/`apply_mutation`/`subscribe`, synchronous, object-safe)
   is **unchanged**. `DistributedRaftEngine` implements the same trait; it
   bridges openraft's async core to the sync trait via a dedicated tokio
   `Handle` (the binary already runs a multi-thread runtime). Specifically:
   `apply_mutation` builds a `ClientWriteRequest(Mutation)`, submits it to the
   Raft leader, and blocks the caller on the result via
   `Handle::block_on`/a oneshot channel. This keeps `control-api`'s
   `Arc<dyn SessionStore>` consumption working without an ABI break. Because
   the trait signature does not change, the contract is formally invariant;
   this ADR records the **semantic** change (multi-node: `apply` returns only
   after leader replication, not after a local WAL append).

4. **Type mapping.**

   | openraft role        | sonicbrew type                              | Note                                                  |
   |---|---|---|
   | Entry data `D`       | `audio_graph_bsd::Mutation`                 | already Serialize/Deserialize/Clone (0.4.0)           |
   | State machine data   | `audio_graph_bsd::TopologySnapshot`         | already Serialize/Deserialize/Default; `.apply()` = SM |
   | Snapshot payload     | serialized `TopologySnapshot` + last LogId  | `bincode` encoding; no wrapper type needed            |
   | `NodeId`             | `RaftNodeId = u64`                          | aliased to avoid collision with `audio_graph_bsd::NodeId` (audio-node id) |

   New workspace dependencies (exact pins, committed `Cargo.lock`):
   `openraft = "0.9"`, `async-trait = "0.1"` (the `RaftLogStore` /
   `RaftStateMachine` / `RaftNetwork` impls are `async_trait`-based),
   `bincode` (binary encode for log/snapshot payloads).

5. **`RaftNetwork`.** For tests only, an **in-process in-memory loopback**
   (a per-node channel map) ships in `session-store`. Production transport
   (TCP + mTLS) is a separate ADR; this ADR explicitly fences it out.

6. **Test gates (per `TESTING-STANDARDS.md`).**
   - Unit: `RaftLogStore` append/get/purge/truncate; `RaftStateMachine`
     apply/snapshot install/build.
   - Integration (`tests/`, `#[tokio::test]`): 3-node cluster leader
     election, log replication, and leader failover with **deterministic
     election timeouts** (fixed seed, no retry).
   - M07 coverage target: **80%**.
   - FreeBSD gate: `cargo check --target x86_64-unknown-freebsd` (compile
     only; live-cluster runs on Linux only, same as the rest of M07).

## Consequences

- **Positive:** the distributed fault-tolerance axis of `ARCHITECTURE.md` §7.2
  is realized — a single-node failure no longer loses the session/topology
  state. The single-node `RaftEngine` remains available for single-host
  deployments and for tests that do not need consensus, so no existing
  consumer regresses.

- **Negative:** openraft 0.9 is a moving target across patch releases
  (`RaftTypeConfig` associated types, `RaftLogStore`/`RaftStateMachine`/
  `RaftNetwork` `async_trait` signatures, `RPCOption`, `storage::vocab`).
  Patch-level API drift is possible; `cargo deny` may flag
  multiple-versions for transitive crates (e.g. rand/byteorder/maplit).

- **Negative:** leader election and failover integration tests are
  timing-sensitive and a CI flake risk if timeouts are not deterministic.

- **Mitigation:** exact patch pin of openraft 0.9.x + committed `Cargo.lock`;
  run `cargo deny check` early (Task 2 of any future implementation) to catch
  multiple-versions regressions; election timeouts fixed to a deterministic
  seed with no retry, single test run.

- **Mitigation:** the current `EngineState` is guarded by a `std::sync::Mutex`
  (`src/lib.rs:99`). The openraft storage traits are `async` and must never
  hold a `std::Mutex` guard across an `.await` boundary (deadlock / cancel
  hazard). Implementations will either keep the redb synchronous I/O short
  (sub-millisecond, no await under the guard) or split state into a
  consensus-meta mutex separate from the topology snapshot (apply path).

- **Reversibility:** `DistributedRaftEngine` is a distinct type from the
  single-node `RaftEngine`. Reverting to single-node operation is removal of
  the distributed type plus its `openraft`/`async-trait`/`bincode` deps; the
  single-node engine and its `MUTATIONS` table are untouched.

## Compliance

This ADR is the required artifact for **three** `GOVERNANCE.md §2.2` triggers
in a single decision:

1. **openraft mode transition (single-node → multi-node)** — sonicbrew
   `GOVERNANCE.md §2.2` line 51.
2. **redb WAL/table schema change** (new `logs`/`hard_state`/`membership`/
   `current_snapshot` tables) — sonicbrew `GOVERNANCE.md §2.2` line 55 and
   umbrella `GOVERNANCE.md §2.2` line 61.
3. **`SessionStore` trait contract** — sonicbrew `GOVERNANCE.md §2.2` line 51.
   The trait signature is invariant in this decision; only the multi-node
   replication semantics change, which this ADR records explicitly so the
   semantic shift is not smuggled in as a "no contract change".

## Related

- `sonicbrew/GOVERNANCE.md` §2.2 (lines 51, 55) — ADR triggers
- umbrella `GOVERNANCE.md` §2.2 (line 61) — storage schema trigger
- `ARCHITECTURE.md` §6, §7.2 — distributed fault-tolerance axis
- `ROADMAP.md` §5 decision point #1 — Raft engine choice
- `BUILD-PLAN.md` §1 (line 109, `openraft = "0.9"`), §2.1 (single-node
  deferral comment superseded here)
- `TESTING-STANDARDS.md` §2, §4 — unit/integration tier, deterministic seeds
- `audio-graph-bsd` 0.4.0 `topology_pub.rs` (`Mutation`/`TopologySnapshot`
  serde derives, `TopologySnapshot::apply`)
- `session-store/src/lib.rs` lines 6–7, 92–93, 99 — openraft-deferral comment
  and current `EngineState` mutex (superseded; mitigation applies)
