# mgd — Architecture & Design Spec

`mgd` (Memory Guardian Daemon, packaged as `lmkd-linux`) is a userspace memory
pressure manager for the Linux desktop. This document is the authoritative,
end-to-end description of *what* it does, *how* it is built, and *why* the design
choices were made. It is written to be read top-to-bottom by someone new to the
codebase.

Companion documents:
- [`PRIVILEGE_DESIGN.md`](PRIVILEGE_DESIGN.md) — how mgd gains the small amount of
  OS privilege a few features need, without running as root. Authoritative for
  anything capability-related.
- `README.md` — install/usage/quick reference.

---

## 1. The problem

On a memory-constrained Linux desktop (the reference machine: 16 GB, Intel Iris
Xe UMA, Fedora 44 / kernel 7.0, KDE Plasma 6 Wayland) three conditions combine to
make memory pressure painful in ways the stock kernel does not handle well:

1. **UMA GPU memory.** On Intel Iris Xe the GPU and CPU share one physical RAM
   pool. Every compositor buffer, browser GPU process, and desktop effect
   allocates from the same 15 GB that applications use. Standard tools
   (`free`, `/proc/meminfo`) do not attribute this to the GPU.

2. **MGLRU swap behavior.** Kernel 7.0's Multi-Gen LRU pushes pages into zram
   aggressively on a spike, but **never proactively pulls them back** once RAM
   frees up. Pages sit compressed indefinitely, so the system stays sluggish
   between pressure events.

3. **Unbounded process growth.** plasmashell leaks GPU buffers over days;
   Firefox RSS grows from ~400 MB to multiple GB over a long session. Not
   classic leaks — they retain memory they no longer need.

The kernel's own defenses are too late and too blunt: the OOM killer only fires
when the system is already unresponsive, and it has no notion of *priority* — a
compositor and a background file indexer are equal candidates.

**mgd's thesis:** monitor pressure continuously, and manage the reclaim cycle the
kernel skips — freezing, checkpointing, or killing processes *in priority order*
before stall time reaches the point of no return, then restoring them when
pressure clears.

---

## 2. Design principles

1. **Unprivileged by default.** The daemon runs as a systemd *user* service and
   manages only the user's own session processes. Every privileged feature is
   opt-in and degrades gracefully when its grant is absent. (See §9 and
   `PRIVILEGE_DESIGN.md`.)
2. **Priority before force.** Always act on the least-important process first,
   and never escalate force beyond what the current pressure level warrants.
   Hard-protect the compositor, audio, and session core.
3. **Reversible before destructive.** Prefer freeze (instant-reversible) →
   checkpoint (reversible via disk) → terminate → kill, in that order.
4. **Never free more than needed.** Stop acting the moment the RAM deficit is
   covered.
5. **Measure, don't assume.** Re-read PSI and meminfo every cycle; let the next
   cycle self-correct any over/under-estimate.
6. **Stop the bleeding cheaply first.** Before touching any process, run
   system-level reclaim (zram compaction, page-cache drop) that frees RAM with
   zero application impact.
7. **Fail safe.** A missing capability, a vanished PID, or a parse error logs
   once and continues — never a hard crash mid-cycle.

---

## 3. Process & threading model

The system is built as a Cargo workspace with a core daemon (`mgd`) and standalone plugins (`mgd-kde`, `mgd-gpu-intel`, etc.).

The core daemon (`mgd`) runs four cooperating threads ("actors"), spawned in
`main.rs` and joined on shutdown. They share three things via `Arc`:
`Arc<Mutex<FrozenRegistry>>`, `Arc<Mutex<CheckpointRegistry>>`, and an
`Arc<Logger>`.

```
                        ┌──────────────────────────────────────────┐
                        │                 main.rs                   │
                        │  spawns 4 threads, installs signal handlers│
                        └──────────────────────────────────────────┘
                                          │
        ┌──────────────────┬──────────────┴───────┬──────────────────────┐
        ▼                  ▼                       ▼                      ▼
┌───────────────┐  ┌───────────────┐     ┌─────────────────┐   ┌──────────────────┐
│ PressureRespon│  │ RecoveryManager│    │  IPC server      │   │ MaintenanceManager│
│   (evictor.rs)│  │ (recovery.rs)  │    │   (ipc.rs)       │   │ (maintenance.rs)  │
│  5s poll      │  │  3s poll       │    │  unix socket     │   │  60s poll         │
│  freeze/kill/ │  │  unfreeze/     │    │  status/list/    │   │  proactive swap   │
│  checkpoint + │  │  restore at    │    │  unfreeze/reload │   │  reclaim          │
│  system pre-  │  │  Normal only   │    │  & plugin comms  │   │                   │
│  actions      │  │                │    │                  │   │                   │
└───────────────┘  └───────────────┘     └─────────────────┘   └──────────────────┘
        │                  │                       │                      │
        └──────────────────┴───────────────────────┴──────────────────────┘
                    shared: FrozenRegistry, CheckpointRegistry, Logger
```

### Why these specific threads

- **PressureResponder (evictor, 5 s):** the hot loop. Reads PSI, decides and
  executes freeze/terminate/kill/checkpoint, and runs the two *quick* system
  pre-actions (zram compact, page-cache drop). Must stay responsive, so nothing
  that blocks for long lives here.
- **RecoveryManager (3 s):** acts **only at Normal pressure**. Unfreezes
  processes that have been frozen long enough, and restores checkpointed
  processes one at a time — both gated by a learned RAM baseline so the system
  doesn't bounce straight back into pressure.
- **IPC server:** a Unix-domain-socket request/response server for `mgctl` and **plugins**.
- **MaintenanceManager (60 s):** houses the *blocking* / slow housekeeping that
  must not stall the 5 s loop (e.g., proactive swap reclaim which blocks on `swapoff`/`swapon`).
  Acts only when the system is calm.

### The threading rule (load-bearing)

> Anything that **blocks for a long time** runs on the MaintenanceManager.
> Anything whose result must be **visible to the evictor's `plan()` on the same
> cycle** runs **inline in the evictor before `plan()`**.

This is why zram *compact* and page-cache *drop* are inline pre-actions (their
freed RAM must shrink the deficit `plan()` computes this cycle) while swap *reclaim*
is on the maintenance thread (it blocks, and has no same-cycle ordering dependency).

### Shutdown

SIGINT/SIGTERM set an atomic `SHUTDOWN` flag (the handler is async-signal-safe —
just a relaxed atomic store). Each actor checks `should_shutdown()` each
iteration and returns. After all join, `main` does a final **unfreeze sweep** so
no process is left stopped. systemd sends SIGTERM on `stop`, with
`TimeoutStopSec=10` to allow the sweep.

---

## 4. The sense → decide → act pipeline

Every evictor cycle is a pure pipeline:

```
   monitor/                    engine/decision.rs              executor/
 ┌──────────┐   pressure   ┌────────────────────┐  Decisions ┌──────────────┐
 │ PSI read │──level──────▶│   plan() DRY RUN    │───────────▶│ freeze/kill/ │
 │ meminfo  │   +meminfo   │  (no side effects)  │            │ terminate/   │
 │ process  │──procs──────▶│                     │            │ checkpoint   │
 └──────────┘              └────────────────────┘            └──────────────┘
```

`plan()` is a **dry run**: it produces a `Vec<Decision>` and touches nothing.
`evictor.rs` executes those decisions. This separation makes the entire
decision policy unit-testable with no processes and no syscalls — see the tests
in `engine/decision.rs`.

---

## 5. Sensing (`src/monitor/`)

| Module | Reads | Provides |
|--------|-------|----------|
| `psi.rs` | `/proc/pressure/memory` | `MemoryPressure` (some/full avg10/60/300) → `PressureLevel` |
| `meminfo.rs` | `/proc/meminfo` | `MemInfo { available, total, swap_free, swap_total }`, `swap_used_pct()` |
| `process.rs` | `/proc/<pid>/{status,stat,oom_score,exe,cgroup}` | `Process` list, `cpu_jiffies()` for idle detection |
| `zram.rs` | `/proc/swaps`, `/sys/block/zramN/mm_stat` | device list, compressed + decompressed sizes, `compact()` |
| `gpu.rs` | `/proc/<pid>/fdinfo/*` (DRM) | per-process GPU residency (UMA), plasmashell restart |
| `cache.rs` | configured dir trees | `posix_fadvise(DONTNEED)` page-cache drop |

### Pressure levels (`psi.rs`)

`PressureLevel` is an ordered enum (`Normal < Elevated < High < Critical <
Emergency`), derived `PartialOrd`/`Ord` so callers can gate on
`level >= Elevated`. Mapping from PSI `some avg10`:

| `some_avg10` | Level |
|--------------|-------|
| ≥ 70 % | Emergency |
| ≥ 50 % | Critical |
| ≥ 25 % | High |
| ≥ 5 % | Elevated |
| else | Normal |

**Full-stall accelerator:** if `full_avg10 ≥ 20 %` (every task stalled), the
level jumps straight to **Critical** (or Emergency if `some_avg10 ≥ 70`),
regardless of the table above.

**Swap escalation (`evictor::escalate_for_swap`):** PSI can miss a slow pre-OOM
where swap fills gradually. When `swap_used_pct ≥ 85 %` (and swap ≥ 256 MB), the
computed level is bumped one tier. This *effective level* is what drives `plan()`
and the pre-actions.

### Process selection (`process.rs`)

`list_processes()` walks `/proc` and keeps only processes that are
**`is_user_managed`** — owned by our euid **and** living in a `user.slice` /
`user@` cgroup. This is the scoping guarantee: mgd never targets root daemons or
other users (it couldn't signal them anyway, and it shouldn't try). It also
excludes its own PID.

`exe_basename` (from `/proc/<pid>/exe`) is captured untruncated because
`/proc/<pid>/status` `Name:` is truncated to 15 chars — the basename is what the
`.desktop` category fallback matches on.

---

## 6. Deciding (`src/engine/`)

### Priority tiers

Every process gets a 0–100 priority (higher = sacrifice sooner), resolved by
`config.priority_for(name, exe_basename)`:

1. First `[[apps]]` regex whose pattern matches the process name wins.
2. Else, the `.desktop` category index keyed by `exe_basename`.
3. Else, the default (`[defaults] priority`, 50).

| Range | Tier | Treatment |
|-------|------|-----------|
| 0–19 | CRITICAL / SYSTEM | **never touched** (hard rule) — compositor, audio, session core |
| 20–39 | HIGH | last resort only (IDEs, DBs, video calls) |
| 40–59 | NORMAL | standard apps |
| 60–79 | LOW | background services — frozen/checkpointed early |
| 80–100 | EXPENDABLE | killed first (renderer tabs, AI inference) |

### `plan()` algorithm

1. If `Normal`, return no decisions.
2. Compute the **RAM deficit**: `target = 15 % of total`; `deficit = target −
   available`. If `≤ 0`, nothing to do.
3. Build candidate list: processes using **> 10 MB** (RSS+swap), each tagged with
   its priority. Sort least-important first (priority desc, then RSS desc, then
   oom_score desc).
4. Walk candidates until the deficit is covered:
   - **Hard rule 1:** skip priority ≤ 19 (system/critical tier).
   - **Hard rule 2:** skip anything matched by a `[[protect]]` entry.
   - Choose an action via `decide_action(level, prio, swap_ratio, checkpoint_override)`.
   - **Deficit accounting:** a *Freeze* frees no RAM directly (SIGSTOP only stops
     further allocation; reclaim is the kernel's to do later), so it does **not**
     reduce the deficit — otherwise mgd would stop short believing memory was
     freed. Terminate/Kill/Checkpoint credit the full RSS; the next 5 s cycle
     re-measures, so any lag self-corrects.

### `decide_action` matrix

| Level \ tier | prio ≥ 80 (expendable) | prio 60–79 (low) | prio 20–59 (normal/high) |
|---|---|---|---|
| **Elevated** | Freeze | Freeze | — |
| **High** | Terminate | Freeze | — |
| **Critical** | Terminate¹ | Terminate¹ | Checkpoint¹ |
| **Emergency** | Kill | Kill | Kill |

¹ At Critical, special cases apply in order:
- **Per-process `checkpoint` override** (config) wins: `true` → Checkpoint;
  `false` → Kill if mostly-swapped else Terminate.
- Else if **`swap_ratio > 0.5`** (already mostly in swap) → Kill (checkpointing
  on-disk-already data is pointless).
- Else normal tier with real RAM → Checkpoint to preserve state.

### Actions (`Action` enum)

- **Freeze** — SIGSTOP. Instantly reversible (SIGCONT). Stops the bleeding.
- **Checkpoint** — CRIU dump to disk, then SIGKILL. Reversible via restore.
- **Terminate** — SIGTERM, 5 s grace, then SIGKILL. Not reversible.
- **Kill** — SIGKILL now. Last resort.

### Restore safety (`health.rs`)

`HealthBaseline` keeps an EMA (α = 0.05, ~60 samples to converge) of available
RAM during Normal periods. `safe_to_restore(avail, total, rss)` requires
post-restore RAM to stay above 85 % of the learned baseline (or a conservative
10 % of total while fewer than 10 samples exist). This gates both unfreeze and
checkpoint-restore so recovery never re-triggers pressure.

---

## 7. Acting (`src/executor/`)

| Module | Responsibility |
|--------|----------------|
| `freezer.rs` | `freeze`/`unfreeze` (SIGSTOP/SIGCONT), plus `*_checked` variants |
| `killer.rs` | `terminate` (SIGTERM → 5 s wait → SIGKILL), `kill` (SIGKILL) |
| `checkpoint.rs` | CRIU dump/restore; absolute-path criu resolution |
| `registry.rs` | `FrozenRegistry`, `CheckpointRegistry` (in-memory state) |
| `mod.rs` | `OpResult`, `read_start_time()` |

### PID-recycle safety (a recurring theme)

A PID can be reused by a different process between when mgd records it and when it
acts. Every state-changing op guards against this by capturing the process
**start_time** (field 22 of `/proc/<pid>/stat`) at record time and re-checking it
before acting:

- `freeze_checked` / `unfreeze_checked` abort (or no-op) if start_time changed.
- `killer::terminate` treats a start_time change during the grace window as
  "original already exited — success".
- `FrozenRegistry.add()` captures start_time; the maintenance reaper re-checks it
  after its 60 s idle sample.

### Registries

Both are **in-memory only**, not persisted. A daemon restart orphans frozen /
checkpointed processes — which is why `main` runs `cleanup_orphaned_snapshots()`
at startup (snapshot dirs from a previous crash would otherwise leak hundreds of
MB). `CheckpointRegistry` tracks a per-entry restore-attempt count; after
`MAX_RESTORE_ATTEMPTS = 3` the snapshot is abandoned and removed.

### CRIU specifics

Dump: `criu dump --tree <pid> --images-dir <dir> --shell-job --leave-stopped
--ext-unix-sk --tcp-established --file-locks`, then SIGKILL on success. Snapshots
live in `~/.local/share/mgd/snapshots/<pid>_<name>/`. Restore:
`criu restore --images-dir <dir> --shell-job --restore-detached`.

criu is resolved to an **absolute path** from a fixed candidate list
(`/usr/sbin`, `/usr/bin`, `/sbin`, `/bin`, `/usr/local/{s}bin`) and never via a
`PATH` search — because the binary may be capped, and a `PATH`-search invocation
of a capped binary is a hijack vector. Privilege failures are inferred from
stderr (no libcap dependency) and annotated with the exact `setcap` command to
re-run. See §9 and `PRIVILEGE_DESIGN.md` §3.

---

## 8. System-level reclaim (free RAM without touching processes)

These act on the *system*, not a process, and run before `plan()` so their
savings reduce the work the evictor must do.

### zram compaction (`monitor/zram.rs`, inline pre-action, Elevated+)

zram's allocator fragments over time — freed slots aren't coalesced, so the pool
holds more RAM than its live data needs. Writing `1` to
`/sys/block/zramN/compact` repacks live objects and releases empty pages.
~100 ms, no process touched. Gated on `used_mb ≥ min_used_mb`. Needs the sysfs
group-write grant (see §9); `EACCES` ⇒ logged once, disabled for the session.

### page-cache drop (`monitor/cache.rs`, inline pre-action, High+)

Under pressure the kernel often evicts app pages to swap while keeping file cache
(build artifacts, `node_modules`, browser cache) that won't be re-read until the
next build. `posix_fadvise(POSIX_FADV_DONTNEED)` on configured directory trees
drops that cache. Surgical (only listed trees, never global `drop_caches`),
unprivileged (own files), non-destructive (clean pages drop immediately; dirty
pages are written back first). Hand-rolled `~` + single-`*`-per-segment glob
(no glob dependency). Bounded: `MAX_DEPTH = 8`, a **global** `MAX_FILES = 50_000`
budget across all patterns, symlinks skipped (so the walk can't escape the tree).
Cooldown-gated so a sustained High spell doesn't re-walk every 5 s.

### proactive swap reclaim (`maintenance.rs`, maintenance thread, Normal only)

The headline fix for the MGLRU lazy-swap-in problem. When calm, cycle the zram
swap device (`swapoff`/`swapon`) to pull all compressed pages back into RAM.
Blocks, so it runs on the maintenance thread. **Privileged** (`CAP_SYS_ADMIN`) —
carried by the `mgd-zram-reclaim` helper (see §9), **off by default**. All policy
gates live in the unprivileged daemon:

- Normal pressure **and** `some_avg60 < 5 %` (calm, not a just-subsided spike)
- cooldown elapsed (`reclaim_cooldown_min`, default 10 min)
- `zram_used ≥ min_zram_used_mb` (default 2048)
- **OOM headroom gate (critical):** `MemAvailable > decompressed_footprint ×
  decompressed_headroom_mult` (default 1.5). zram stores *compressed* pages that
  expand 2–3× on the way back into RAM; without this gate the reclaim itself
  could OOM the box.

---

## 8.5 Plugins (`mgd-kde`, `mgd-gpu-intel`, etc.)

Historically, environment-specific watchers lived inside `mgd`. They are now decoupled into standalone plugin processes that connect to the IPC server using the `mgd-common` library:
- **`mgd-kde`**: Handles restarting the KDE `plasmashell` on GPU leaks, and reaping `plasma-discover` on CPU idle.
- **`mgd-gpu-intel`**: Scans `/proc/<pid>/fdinfo/` (DRM) to account for UMA graphics memory as part of process footprint.

Plugins observe the system and request actions from the core daemon (`ActionRequest`), keeping the core portable.

---

## 9. Privilege model (summary)

Full treatment in [`PRIVILEGE_DESIGN.md`](PRIVILEGE_DESIGN.md). The core
constraint: mgd is a systemd **`--user`** service, so `AmbientCapabilities=`
**does not work** — the user manager has no privilege to hand out. Privilege must
therefore live **on a file on disk** (a `setcap` binary or a sysfs/device grant),
which is honored regardless of who launches the process.

| Operation | Carrier | Capability | Default |
|-----------|---------|-----------|---------|
| zram compact | none (tmpfiles sysfs grant on `compact` node) | none | on |
| swap reclaim | `mgd-zram-reclaim` (3rd binary) | `CAP_SYS_ADMIN` | **off** |
| CRIU dump/restore | the system `criu` binary, capped | `CAP_CHECKPOINT_RESTORE` + `CAP_SYS_PTRACE` (+`CAP_NET_ADMIN` for live TCP) | on if capped |

Principles: narrowest capability never root; smallest carrier (prefer a sysfs
grant over a binary, a fixed-function binary over a flexible one); **policy stays
in the unprivileged daemon**, the carrier is dumb; group-gated execution
(`root:mgd 0750`); opt-in with graceful degradation.

### `mgd-zram-reclaim` (the one custom privileged binary)

Self-contained, libc-only, **no argv, no env, no subprocess**. It:
- validates every target is a canonical `/dev/zram<N>` device (never a disk
  partition or a `zram-`named swapfile);
- **self-enforces the OOM floor** before any `swapoff` (reads `MemAvailable` +
  decompressed footprint from world-readable nodes and refuses if RAM wouldn't
  fit the pages) — because the binary is group-executable, this safety property
  cannot rely on the daemon being the caller;
- blocks SIGINT/TERM/HUP/QUIT across each `swapoff`→`swapon` pair (with retry) so
  an interrupt can never strand the system with swap off;
- restores each device's original priority;
- uses **distinct exit codes** (0 ok / 1 transient / 2 EPERM-uncapped / 3
  refused-unsafe / 4 no-meminfo) so the daemon can tell "uncapped" (disable for
  session) from "transient" (retry).

### Why CRIU has no custom helper

`criu` is already its own external binary mgd execs, so the caps go directly on
it (Option A). A validating wrapper (`mgd-checkpoint`, Option B) is documented as
the multi-user hardening upgrade but deliberately not built for the single-user
desktop threat model.

---

## 10. Configuration (`src/config.rs`)

Hot-reloadable TOML behind `Arc<RwLock<CompiledConfig>>`. Load order:
`~/.config/mgd/priorities.toml` → `/etc/mgd/priorities.toml` → the built-in
`config/priorities.toml` (embedded via `include_str!`). A parse error falls back
to the built-in defaults (logged) rather than crashing.

Each tunable section follows the **config triple** pattern:
1. a raw `#[derive(Deserialize)]` struct with `#[serde(default = "...")]` fns,
2. public fields on `CompiledConfig` (with minutes→seconds conversion at compile
   time),
3. a default block in `config/priorities.toml`.

Sections: `[defaults]`, `[[apps]]` (priority regexes + per-app `checkpoint`
override), `[[protect]]` (never-touch regexes), `[category_priorities]`
(`.desktop` fallback), `[plasma]`, `[plasma_discover]`, `[zram]`, `[reclaim]`,
`[cache_drop]`.

Reload is triggered by SIGHUP or `mgctl reload`; the responder picks it up at the
top of its next cycle. `.desktop` files are re-scanned on each load to rebuild the
`exe_basename → priority` index.

---

## 11. IPC & control (`src/ipc.rs`, `src/mgctl.rs`)

The daemon serves a newline-delimited request/response protocol on a Unix socket
at `$XDG_RUNTIME_DIR/mgd.sock` (fallback `/tmp/mgd-<uid>.sock`). Responses are
`OK <data>` / `ERR <msg>`. Connections are short-lived threads, capped at
`MAX_CONNECTIONS = 8`; a stale socket from a crash is detected and rebound.

`mgctl` is the client and splits cleanly in two:
- **socket commands** (daemon must be alive): `status`, `list`, `unfreeze`,
  `reload`.
- **lifecycle commands** (wrap `systemctl --user` / `journalctl --user`, work
  even when the daemon is down — which the socket cannot): `restart`, `start`,
  `stop`, `service` (systemd unit state), `logs [-f]`.

`status` is the daemon's live view (pressure, frozen counts); `service` is the
systemd unit view (active state, PID, uptime, memory) — deliberately distinct.

---

## 12. Logging (`src/logger.rs`)

One session log file per run at `~/memlogs/mgd_<YYYY-MM-DD_HH-MM-SS>.log` (local
time), shared via `Arc<Logger>` (so rotation runs once, not per-thread). `LogEntry { action, pid, name, rss_mb,
result }` is the structured line format. Rotation keeps the newest `log_keep`
files (default 10; 0 = unlimited). Actions seen in logs: `FREEZE`, `UNFREEZE`,
`TERMINATE`, `KILL`, `CHECKPOINT`, `RESTORE`/`RESTORE_FAIL`/`RESTORE_ABANDON`,
`ZRAM`, `RECLAIM`, `CACHE`, `REAP`, `RESTART`.

---

## 13. Data sources at a glance

Everything mgd senses comes from procfs/sysfs — no kernel module, no BPF:

| Path | Used for |
|------|----------|
| `/proc/pressure/memory` | PSI pressure levels |
| `/proc/meminfo` | available/total RAM, swap usage |
| `/proc/<pid>/status` | name, VmRSS, VmSwap |
| `/proc/<pid>/stat` | start_time (recycle guard), utime+stime (idle) |
| `/proc/<pid>/oom_score` | tiebreak in candidate sort |
| `/proc/<pid>/exe` | untruncated basename for `.desktop` lookup |
| `/proc/<pid>/cgroup` | user-session scoping |
| `/proc/<pid>/fdinfo/*` | per-process GPU residency (DRM) |
| `/proc/swaps` | zram device discovery |
| `/sys/block/zramN/mm_stat` | compressed + decompressed pool size |
| `/sys/block/zramN/compact` | trigger compaction (privileged write) |

---

## 14. Failure & degradation behavior

| Situation | Behavior |
|-----------|----------|
| PSI read error | treat as "not calm"; skip the cycle |
| config parse error | fall back to built-in defaults, log |
| missing capability/grant | log once, disable that feature for the session, continue |
| criu missing/unprivileged | fall back to SIGKILL; log the setcap command |
| restore fails 3× | abandon snapshot, remove dir, log |
| PID recycled | abort the op (or treat as success if target already gone) |
| daemon restart | orphaned snapshots cleaned at startup; registries reset |
| shutdown (SIGTERM) | unfreeze sweep before exit |

The invariant throughout: **no single failure aborts the daemon** — it logs and
keeps managing pressure.

---

## 15. Tested platform

Fedora 44 · kernel 7.0 · HP Spectre x360 · i7-13700H · 16 GB · Intel Iris Xe ·
KDE Plasma 6 Wayland. Logic (decision matrix, deficit math, parsing, glob,
headroom gate, path resolution) is covered by unit tests that run without root or
real processes; privileged round-trips (swap reclaim, CRIU without root) require
the opt-in grants and are verified manually.
