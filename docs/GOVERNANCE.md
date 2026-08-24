# sonicbrew · Unified Server Governance

> **Document role:** This document defines governance **specific** to the sonicbrew subproject (M07–M14 + the binary). Organization-wide governance has the [umbrella GOVERNANCE](../../GOVERNANCE.md) as its top-level source, and this document stands in a **delegation** relationship to it — matters not explicitly stated in this document follow the umbrella policy.
>
> **Related documents:** [CONTRIBUTING.md](../CONTRIBUTING.md) · [ARCHITECTURE.md](./ARCHITECTURE.md) · [CODEOWNERS](../CODEOWNERS) · [umbrella GOVERNANCE](../../GOVERNANCE.md)

---

## Delegation relationship (Umbrella ↔ Server)

```
umbrella GOVERNANCE.md (organization-wide — top-level policy for roles, decision-making, environments, security)
        │
        │  delegation: sonicbrew-specific rules (module crates, gateway protocols, session consensus, audio-toolkit dependencies)
        ▼
sonicbrew GOVERNANCE.md (this document — sonicbrew-subproject-only augmentation)
```

- **On conflict:** umbrella policy takes precedence.
- **Augmentation scope:** this document defines only rules specific to the development, review, and deployment of the sonicbrew modules (M07–M14).

---

## 1. Roles

> Role definitions and assignment criteria are inherited from [umbrella GOVERNANCE](../../GOVERNANCE.md) §1. sonicbrew-specific responsibilities:

| Role | sonicbrew-specific responsibilities |
|------|-----------------|
| **Project Lead** | sonicbrew roadmap priorities, final approval of production deployments |
| **Tech Lead** | Approval of RT audio paths, `session-store` (M07) consensus design, approval of audio-toolkit dependency contracts |
| **Gateway Tech Lead** | Compatibility and security decisions for gateway protocols (M10/M11/M12), protocol regression gates |
| **Net Audio Maintainer** | `net-rtp-aes67` (M09) zero-copy/netmap paths, AES67 standards compliance |
| **DevOps Lead** | `monitor` (M14) observability, environment (jail/bhyve/carp) infrastructure |
| **Security Officer** | Gateway security (mTLS/SRTP/Capsicum), audits of session secret management, veto |

---

## 2. Decision-Making

### 2.1 Routine decisions: PR review

- Merge with approval from at least 1 Maintainer (inherited from umbrella policy)
- **A single objection = blocking**

### 2.2 Mandatory ADR items (sonicbrew-specific)

If any of the following applies, an [ADR](../../docs/adr/) is mandatory (sonicbrew-specific additions to umbrella §2.2):

- **Gateway protocol compatibility change** — wire protocol changes to the PulseAudio protocol (M10) / ALSA PCM plugin (M11) / WebSocket frame format (M12). Breaks existing client compatibility.
- **Session consensus design change** — `session-store` (M07) `openraft` mode transition (single-node → multi-node), WAL format/checksum structure, `SessionStore` trait contract change.
- **RT audio path change** — thread-separation principles for gateway reception (RT boundary) and net-rtp transmit/receive, changes to the lock-free ring buffer (`rtrb`) boundary.
- **audio-toolkit dependency contract change** — changes to the signatures of the audio-toolkit traits sonicbrew consumes (`AudioNode`/`Gateway`/`SessionStore`/`NetworkAudioTransport`), changes to the path dep structure. ⚠ Affects both subprojects.
- **netmap/Capsicum/jail kernel feature dependency change** — inherited from umbrella §2.2 (netmap(4)/Capsicum/jails(8)).
- **Session store schema change** — `session-store` (M07) redb/sled table structure.
- **Control API contract change** — `control-api` (M13) gRPC/REST contract (backward compatibility).

### 2.3 Strategic decisions: RFC

- New gateway sub-paths (WebRTC/HLS), adoption of distributed multi-node, promotion to AES67 compatibility
- RFC document → 5 business days of review → Project Lead decision (inherited from umbrella §2.3)

### 2.4 Urgent decisions: Hotfix (sonicbrew-specific)

In a production incident, the Tech Lead / Gateway Tech Lead acts immediately under their authority:

| Incident type | Authority | Post-incident obligation |
|----------|------|----------|
| **xrun storm** (audio dropouts) | Tech Lead | Postmortem within 48 hours |
| **Raft leader loss** (M07 session disruption) | Tech Lead | Postmortem within 48 hours |
| **Gateway outage** (M10/M11/M12 client disruption) | Gateway Tech Lead | Postmortem within 48 hours |
| **netmap NIC failure** (M09 RTP disruption) | Net Audio Maintainer | Verify fallback (standard socket) switchover |

---

## 3. Environments

> The environment hierarchy is inherited from [umbrella GOVERNANCE](../../GOVERNANCE.md) §3. sonicbrew-specific rules:

```
local (dev PC — Linux or FreeBSD host)
  ↓  push / cargo build (including the audio-toolkit path dep)
vm (bhyve / VirtualBox — FreeBSD 14.2-RELEASE VM)
  ↓  approved PR
staging (jail-isolated or a separate host — production mirror)
  ↓  Project Lead/Tech Lead approval
production (FreeBSD host — carp(4) HA)
```

| Rule | local | vm | staging | production |
|------|-------|-----|---------|------------|
| Data | test fixtures | test fixtures + fixtures | production replica (masked) | real sessions/audio |
| Secrets | `.env` (excluded from git) | `.env` (inside the VM) | per-host env (jail) | per-host env + `carp(4)` VIP |
| Deploy permission | anyone | anyone (local VM) | Maintainer+ | Project Lead/Tech Lead |
| WAL/schema migration | free | free (VM) | Maintainer approval | Tech Lead approval + prior backup mandatory |
| **session-store(M07) Raft mode** | single-node (self-leader) | single-node | single-node | single-node (MVP) → multi-node (later) |
| **audio-toolkit path dep** | local `../audio-toolkit/` | mounted into the VM | built artifacts | static build |

### 3.1 Production data access

- Access to the production `session-store` (redb) is limited to a service account managed exclusively by the Security Officer
- Lookups of session credentials and client connection information require two-person approval (dual control) (inherited from umbrella §3.1)

---

## 4. Branch & merge strategy

### 4.1 Trunk-Based Development

```
main (protected, direct push forbidden)
  ├── feature/gw-browser-ws-reconnect       (lifetime of 3 days or less recommended)
  ├── fix/session-store-wal-corruption
  ├── perf/net-rtp-aes67-zero-copy
  └── hotfix/control-api-auth-regression
```

### 4.2 Branch naming

```
{type}/{module-name}-{short-description}

module-name = crate name (session-store, net-rtp-aes67,
                          gw-pulse, gw-alsa, gw-browser, control-api, monitor, sonicbrew)
```

| type | meaning |
|------|------|
| `feature` | new functionality |
| `fix` | bug fix |
| `hotfix` | urgent production fix |
| `perf` | performance (RT paths, zero-copy) |
| `refactor` | refactoring |
| `chore` | build, dependencies, CI |

### 4.3 Merge rules

- **Squash & Merge** principle (inherited from umbrella §4.3)
- PRs should stay within 500 lines
- If a PR exceeds 500 lines, split the PR or obtain prior agreement from the Tech Lead

---

## 5. Code review

### 5.1 Required reviewers (sonicbrew-specific)

| Change area | Minimum reviewers | Must include |
|-----------|------------|----------|
| General sonicbrew code | 1 | Maintainer or above |
| **RT audio paths/core** (M09 transmit/receive, gateway RT boundary) | 2 | **Tech Lead** required |
| **Gateway protocol / security** (M09/M10/M11/M12) | 2 | **Security Officer** required |
| `Cargo.toml` / audio-toolkit dependencies | 2 | **Tech Lead** required |
| `session-store` (M07) consensus/WAL | 2 | **Tech Lead** required |

### 5.2 Review checklist

> For the full checklist, see [CONTRIBUTING.md](../CONTRIBUTING.md) §5. sonicbrew-specific additions:

- [ ] Compliance with the audio-toolkit `AudioNode`/`Gateway` trait contracts (gateways, net-rtp)
- [ ] No alloc/lock on RT threads (reception runs on worker threads)
- [ ] Gateway protocol backward compatibility preserved
- [ ] FreeBSD VM regression pass (xrun/alloc/netmap)
- [ ] session-store WAL checksum verification (M07)

### 5.3 Review turnaround

- First review: **within 24 business hours** (inherited from umbrella)
- Urgent hotfix: **within 2 hours**

---

## 6. Separation of Duties

> Inherited from umbrella §1.2. sonicbrew-specific:

```
Gateway protocol authoring  →  Protocol compatibility review  →  Merge  →  Deploy
       (Author)                    (Gateway Tech Lead)          (Maintainer)  (Project Lead/Tech Lead)
```

- You may not merge your own PR
- **Deploying a gateway protocol change requires approval from someone other than the author**
- **audio-toolkit trait contract changes require approval from both the Security Officer and the Tech Lead** (affects both subprojects)
- Secret/session credential changes require prior approval from the Security Officer

---

## 7. Policy updates

Changes to this document require approval from the Tech Lead or above + notification to all sonicbrew contributors (inherited from umbrella §7). Changes that conflict with the umbrella GOVERNANCE are not permitted.

---

## Related documents

- [umbrella GOVERNANCE](../../GOVERNANCE.md) — organization-wide top-level governance
- [sonicbrew CONTRIBUTING](../CONTRIBUTING.md) — development environment and testing rules
- [sonicbrew ARCHITECTURE](./ARCHITECTURE.md) — 5-layer distribution + dependency diagram
- [P10 design.md](../../notes/p10-architecture-design.md) · [P11 MVP](../../notes/p11-mvp-scope-design.md) — design sources
