# sonicbrew server — Contribution Guide (CONTRIBUTING)

> **Document status:** This guide defines contribution procedures, development environment, testing, and code-quality rules **specific** to the sonicbrew subproject (M07–M14 + the unified `sonicbrew` binary). The org-wide common contribution guide is [umbrella CONTRIBUTING](../CONTRIBUTING.md), the top-level source of truth; this document stands in a **delegation** relationship to it — anything not stated here follows the umbrella guide.
>
> **Relationship (dependency direction):** `sonicbrew` (server) → `audio-toolkit` via relative path dependencies (`../audio-toolkit/crates/<name>`). sonicbrew and audio-toolkit are **sibling independent git repos**, and the umbrella is a conceptual workspace (not a repo). sonicbrew's gateways (`gw-*`) and `net-rtp-aes67` implement audio-toolkit's `AudioNode` trait (defined in `audio-core-bsd`) so they plug into the core graph (`audio-graph-bsd`) as nodes.
>
> **Targets:** FreeBSD `14.2-RELEASE` (minimum patch p3) · Rust 2021 ed. (MSRV 1.85+). This subproject is at the **pre-code stage** (docs only) — the `Cargo.toml`/pseudocode below are a **contract (specification)**, not executable code.

---

## 1. Development Environment Setup

### 1.1 Required tools

```bash
# Rust (rustup, stable toolchain)
rustup default stable
rustc --version    # 1.85+ (MSRV)

# FreeBSD cross-compile target (build verification from a Linux dev host)
rustup target add x86_64-unknown-freebsd

cargo --version
```

> For FreeBSD VM preparation steps (ISO/snapshots, etc.), see [umbrella CONTRIBUTING](../CONTRIBUTING.md) §1.1.

### 1.2 audio-toolkit dependency setup (crates.io by default · path deps = for local co-development)

> **📌 Default = crates.io:** the ten audio-toolkit crates are pinned to their **crates.io** versions. **`cargo build` no longer requires the `../audio-toolkit/` sibling** — the sonicbrew repo builds standalone. The path-dep instructions below are an **alternative** for **local co-development**, where you keep the audio-toolkit sources side by side and modify both together.

**Default (crates.io) — no extra setup needed:** just `cargo build`. Pinned versions are declared in the root `Cargo.toml`'s `[workspace.dependencies]` (10 crates including `audio-bluetooth-bsd` 0.1.0).

**Alternative (path deps, local co-development):** only when the sibling repo is present. Declare audio-toolkit's 10 crates (exact names/paths) in `[workspace.dependencies]` (same crate names/paths as the table above):

```toml
# sonicbrew/Cargo.toml (workspace root)
[workspace.dependencies]
# === audio-toolkit path dependencies (sibling independent repo, crates/<name>/ convention) ===
# LHS = crate name, RHS = path. Always the form ../audio-toolkit/crates/audio-*.
audio-core-bsd     = { path = "../audio-toolkit/crates/audio-core-bsd" }      # shared AudioFrame/ProcessContext/AudioNode traits
audio-graph-bsd    = { path = "../audio-toolkit/crates/audio-graph-bsd" }     # M02 Graph (add_node/link/process_cycle)
audio-dsp-bsd      = { path = "../audio-toolkit/crates/audio-dsp-bsd" }       # M03 mixer/DSP
audio-resample-bsd = { path = "../audio-toolkit/crates/audio-resample-bsd" }  # M04 rubato (RT-safe)
audio-opus-bsd     = { path = "../audio-toolkit/crates/audio-opus-bsd" }      # M05 Opus encode/decode
audio-codec-bsd    = { path = "../audio-toolkit/crates/audio-codec-bsd" }     # M06 symphonia containers
audio-io-bsd       = { path = "../audio-toolkit/crates/audio-io-bsd" }        # M01 cpal backend
audio-plugin-bsd   = { path = "../audio-toolkit/crates/audio-plugin-bsd" }    # M15 (P1)
audio-clock-bsd    = { path = "../audio-toolkit/crates/audio-clock-bsd" }     # M16 clock-ptp (P2)
audio-bluetooth-bsd = { path = "../audio-toolkit/crates/audio-bluetooth-bsd" } # BT A2DP input backend (audio-io-bsd AudioBackend impl, not an M0x module)
```

> **Note:** `AudioNode`/`AudioFrame`/`ProcessContext`/`PortDescriptor` live in **`audio-core-bsd`**, while `Graph`/`add_node`/`link`/`process_cycle` live in **`audio-graph-bsd`**. They are separate crates — take care when importing ([docs/ARCHITECTURE](./docs/ARCHITECTURE.md)).

```bash
# 1. Check that both sibling repos sit side by side (under the umbrella/ directory)
ls ../audio-toolkit/   # must exist

# 2. Build sonicbrew (audio-toolkit compiles along automatically)
cargo build
```

### 1.3 FreeBSD 14.2 VM (bhyve) — build & integration verification

bhyve is recommended (VirtualBox also works). After installing FreeBSD 14.2-RELEASE in the VM, install the following ports:

| Port | Purpose | Related sonicbrew module |
|------|------|------------------|
| `audio/alsa-lib` | via the cpal ALSA backend (shared with audio-toolkit `audio-io-bsd` M01) | `gw-browser` (final output of the browser path) |
| `audio/opus` | Opus encode/decode (audio-toolkit `audio-opus-bsd` M05, BSD-3) | `gw-browser` (codec subpath) |
| `audio/libsndfile` | IR/sample loading (audio-toolkit `audio-codec-bsd` M06 file I/O) | (no sonicbrew module) |
| `multimedia/libsrtp2` | SRTP (WebRTC/AES67 security) | `net-rtp-aes67`, `gw-browser` |
| `security/openssl` | mTLS / DTLS-SRTP | `net-rtp-aes67`, `gw-browser`, `control-api` |
| `net/libnetmap` | netmap zero-copy (RTP data plane) | `net-rtp-aes67` |

### 1.4 Local environment variables

```bash
# .env — never commit to git! (gitignored, §6.2)
cp ../.env.example .env   # copy the umbrella template
# sonicbrew-related local settings:
SONICBREW_RAFT_SEED=local-dev-seed
SONICBREW_ZPOOL_PATH=zpool/sonicbrew-dev
SONICBREW_CONTROL_API_KEY=local-dev-key
```

---

## 2. Development Workflow

### 2.1 Gateway insertion principle (sonicbrew ↔ audio-toolkit integration) ★

This is the **single touchpoint** where sonicbrew and audio-toolkit meet. All of sonicbrew's gateways (`gw-pulse`/`gw-alsa`/`gw-browser`) and `net-rtp-aes67` implement audio-toolkit's **`AudioNode` trait** (defined in `audio-core-bsd`) so they are inserted into the core graph (`audio-graph-bsd` M02) as Source/Sink nodes ([p10 §6.2](../notes/p10-architecture-design.md)).

- **What varies:** only the protocol parsers (WebSocket binary / PulseAudio / ALSA PCM / RTP packets).
- **What stays fixed:** after conversion to `AudioFrame`, nodes are inserted via `audio-graph-bsd`'s `Graph::add_node` as `AudioNode`s.
- **Contract changes = ADR required:** changing the `AudioNode`/`Gateway`/`SessionStore`/`NetworkAudioTransport` trait signatures or the path-dep structure **affects both subprojects** → an ADR plus joint review by both owners is mandatory ([GOVERNANCE §2.2](./docs/GOVERNANCE.md), [docs/ARCHITECTURE](./docs/ARCHITECTURE.md)).

### 2.2 Issue → branch

Branch naming ([GOVERNANCE §4.2](./docs/GOVERNANCE.md)):

```
{type}/{module-name}-{short-description}

module-name = crate name (session-store, net-rtp-aes67,
                            gw-pulse, gw-alsa, gw-browser, control-api, monitor, sonicbrew)

e.g.: feature/gw-browser-ws-reconnect
      fix/session-store-wal-corruption
      hotfix/gw-pulse-protocol-regression
      perf/net-rtp-aes67-zero-copy
```

### 2.3 Conventional Commits

```text
{type}({scope}): {description}

e.g.:
feat(gw-browser): add WebSocket reconnect backoff
fix(session-store): fix missing WAL checksum verification
perf(net-rtp-aes67): apply netmap zero-copy path
refactor(control-api): tidy up REST router structure
docs(monitor): add metrics collection guide
```

| type | Meaning |
|------|------|
| `feat` | New feature |
| `fix` | Bug fix |
| `perf` | Performance improvement (RT path, zero-copy) |
| `refactor` | Refactoring (no behavior change) |
| `docs` | Documentation |
| `test` | Adding/updating tests |
| `chore` | Build, dependencies, CI |

> `{scope}` = module crate name (`gw-browser`, `session-store`, `net-rtp-aes67`, etc.).

### 2.4 Merge rules

- **Squash & Merge** principle (inherited from umbrella policy)
- Keep changes within **500 lines** per PR where possible
- RT path, core graph, gateway protocol, or `audio-toolkit` dependency changes = **2-reviewer review mandatory** (→ [GOVERNANCE §5.1](./docs/GOVERNANCE.md))

---

## 3. Testing

> For org-wide test standards, see [umbrella TESTING-STANDARDS](../TESTING-STANDARDS.md). Below are sonicbrew-specific rules.

### 3.1 Test layers

| Layer | Tool | Scope |
|------|------|------|
| Module unit | `cargo test -p <crate-name>` | Each sonicbrew module crate |
| Integration | `cargo test --features integration` | Cross-crate and graph-insertion paths |
| FreeBSD VM regression | `cargo test` inside bhyve VM | xrun, alloc, and netmap-dependent paths |
| Gateway protocol | Protocol fuzzers / compatible clients | `gw-pulse`/`gw-alsa`/`gw-browser` (WS frames) |
| RT path validation | `cargo bench` + custom harness | `session-store` Raft path, gateway RT boundaries |

### 3.2 Module unit tests

Crate names have **no** `mNN-` prefix. The `-p` argument takes the directory name (= crate name) as-is:

```bash
cargo test -p session-store          # M07
cargo test -p control-api            # M13
cargo test -p gw-browser             # M12 ★ MVP core
cargo test -p net-rtp-aes67          # M09 (P1)
```

### 3.3 FreeBSD VM regression (mandatory)

```bash
# Run inside the VM — xrun/alloc verification
cargo test --target x86_64-unknown-freebsd -- --nocapture

# RT-path alloc verification: no alloc/locks on RT threads (docs/KNOWLEDGE §9.2, docs/ARCHITECTURE)
# Gateway receive (deserialize/decode) runs on worker threads; RT threads call only rubato (audio-resample-bsd) directly
cargo test -p gw-browser --features rt-check
```

### 3.4 Gateway protocol tests

Each gateway implements a compatible protocol, so real-client interoperability must be verified:

| Gateway | Test method |
|-----------|------------|
| `gw-browser` (M12) | WebSocket clients (binary PCM/Opus frame send/receive) — including per-browser-version (Chromium/Firefox) regressions |
| `gw-pulse` (M10) | PulseAudio clients such as `pactl`/`paplay` — protocol message deserialization verification |
| `gw-alsa` (M11) | `aplay`/`arecord` ALSA clients — PCM plugin interface emulation verification |

> **WebSocket regression (MVP core):** the WebSocket subpath of `gw-browser` is the only browser entry point of the P12 prototype, so regression tests for connection, reconnection, and frame loss across browser versions are mandatory.

### 3.5 Session consensus regression (session-store)

```bash
# openraft single-node mode — WAL persistence + self-leader verification
cargo test -p session-store
# Distributed consensus (leader election/log replication) is out of MVP scope — a dedicated harness comes with the next phase (P1)
```

---

## 4. Code Quality

### 4.1 Verification checklist (before submitting a PR)

- [ ] `cargo test` passes (include tests for new features/bug fixes)
- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] FreeBSD target build: `cargo check --target x86_64-unknown-freebsd`
- [ ] **audio-toolkit dependency sanity check** — default is a crates.io build; with path deps (local co-development) the `../audio-toolkit/` build is included
- [ ] No xrun/performance regressions (FreeBSD VM `cargo bench` regression)
- [ ] **No alloc/locks on RT threads** (gateway receive on worker threads; RT uses only rubato)
- [ ] Gateway protocol compatibility regressions pass (`gw-pulse`/`gw-alsa`/`gw-browser`)
- [ ] No secrets/environment variable exposure
- [ ] When adding dependencies, `cargo deny check` passes
- [ ] On API/protocol changes, confirm docs are updated + an ADR is written

### 4.2 unsafe justification

- `unsafe` blocks **must** carry a SAFETY comment stating the preconditions (invariants, lifetimes, aliasing rules).
- When using FreeBSD kernel FFI (`sendfile(2)`, `copy_file_range(2)`, netmap, Capsicum `cap_rights_limit(2)`), encode return-type/semantic differences into backend traits (see the pitfalls in [docs/KNOWLEDGE §2](./docs/KNOWLEDGE.md)).
- Community gateway protocol FFI (`libpulse`/`libasound`) is dynamically linked and isolated — protecting core license simplicity ([docs/KNOWLEDGE §9.4](./docs/KNOWLEDGE.md)).

### 4.3 No alloc/panic on RT paths

- Allocation (`Vec`/`Box`), locks, and panicking branches are **forbidden** in the `process()` path of RT (real-time) audio threads.
- Gateway and net-rtp receive threads are never RT threads → they always hand off to RT threads via the `rtrb` lock-free ring buffer ([p10 §7.2](../notes/p10-architecture-design.md), [docs/KNOWLEDGE §9.2](./docs/KNOWLEDGE.md)).
- RT path changes = mandatory review by two Tech Leads ([GOVERNANCE §5.1](./docs/GOVERNANCE.md)).

### 4.4 Adding dependencies (cargo deny, BSD-compatible)

- The core license is **BSD-2-Clause**. Check license compatibility when adding dependencies.
- Pure Rust + permissive (MIT/Apache/BSD/MPL-2.0) is OK. **LGPL** (libpulse/libasound) requires dynamic linking plus separate isolation.
- Dependency-addition PRs are gated on passing `cargo deny check` (licenses, duplicates, vulnerabilities).

---

## 5. Module Crate Conventions

### 5.1 Crate naming & layout

Crate layout rule: `crates/<kebab-name>/` (aligned with umbrella §4).

| Crate path | ID | Layer |
|--------------|-----|------|
| `crates/session-store/` | M07 | L3 |
| `crates/net-rtp-aes67/` | M09 | L4 |
| `crates/gw-pulse/` | M10 | L5 |
| `crates/gw-alsa/` | M11 | L5 |
| `crates/gw-browser/` | M12 | L5 |
| `crates/control-api/` | M13 | L5 |
| `crates/monitor/` | M14 | L5 |
| `crates/sonicbrew/` | — | Binary (unified entry point) |

### 5.2 Trait implementation rules

sonicbrew modules must implement/consume the corresponding audio-toolkit traits ([p10 §8](../notes/p10-architecture-design.md), [docs/ARCHITECTURE](./docs/ARCHITECTURE.md)):

| sonicbrew module | audio-toolkit trait consumed/implemented | Insertion point |
|------------|--------------------------------|----------|
| `session-store` | `SessionStore` | Graph topology cache & change history |
| `net-rtp-aes67` | `NetworkAudioTransport` → **`AudioNode`** | Graph Source/Sink nodes |
| `gw-pulse` / `gw-alsa` / `gw-browser` | `Gateway` → **`AudioNode`** | Graph Source/Sink nodes |
| `control-api` | `ControlApi` | Graph manipulation API |
| `monitor` | `MetricsSink` | Observability (an observer, not a graph node) |

> **Key point:** by implementing the `AudioNode` trait (audio-core-bsd), gateways and net-rtp are inserted **transparently** into the audio-toolkit graph (audio-graph-bsd). Audio streams from Linux apps and browsers are thereby processed exactly like regular nodes in the graph.

### 5.3 Adding a new module

1. Create the `crates/<name>/` directory + `Cargo.toml` (registered as a workspace member).
2. Declare audio-toolkit path deps in `[workspace.dependencies]`, then consume them from each crate's `Cargo.toml` via `xxx.workspace = true`.
3. Add the responsible owner to [CODEOWNERS](./CODEOWNERS) (a `/crates/<name>/` glob).
4. For RT-path/protocol changes, check the [GOVERNANCE §2.2](./docs/GOVERNANCE.md) ADR gate.

---

## 6. Git Rules

### 6.1 .gitignore policy (tracked vs local)

This repo (sonicbrew) keeps **all project documents as regular (git-tracked) docs under `docs/`** (cleaned up 2026-08-08 — the former "planning docs local-only" policy was abolished, and ROADMAP/BUILD-PLAN were retired once all phases completed):

| Tracked (git) |
|------------|
| `README.md` · `CONTRIBUTING.md` · `CODEOWNERS` · all of `docs/` (ARCHITECTURE · REST-API · AUDIO-NODES · RUNBOOK · PROGRESS · TEST-LAYERS · PERFORMANCE · KNOWLEDGE · GOVERNANCE · adr/) |

### 6.2 Secrets / environment variables

- `.env`, `*.pem`, `*.key`, `secrets.toml`, `.dev.vars`, `credentials.json`, etc. are `.gitignored` and **must never be committed**.
- Production secrets are managed via ZFS-encrypted env + `carp(4)` VIP ([GOVERNANCE §3](./docs/GOVERNANCE.md)).

---

## Related Documentation

- [umbrella CONTRIBUTING](../CONTRIBUTING.md) — org-wide common contribution guide (top-level source of truth)
- [sonicbrew GOVERNANCE](./docs/GOVERNANCE.md) — server-tailored governance
- [sonicbrew KNOWLEDGE](./docs/KNOWLEDGE.md) — must-read knowledge before building each module
- [sonicbrew ARCHITECTURE](./docs/ARCHITECTURE.md) — architecture (as-implemented)
- [sonicbrew RUNBOOK](./docs/RUNBOOK.md) — build/run/operations guide
- [umbrella TESTING-STANDARDS](../TESTING-STANDARDS.md) — FreeBSD bhyve regression standards (local)
- [p10 unified architecture](../notes/p10-architecture-design.md) — single source of truth for trait contracts
- [p11 MVP scope](../notes/p11-mvp-scope-design.md) — MVP/P12 milestones + handover

---

**End of document.** This guide is specific to sonicbrew (server) contribution procedures and supplements anything the umbrella CONTRIBUTING leaves unstated. All crate names/paths are consistent with [CODEOWNERS](./CODEOWNERS).
