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
kernel skips — proactively reclaiming background cgroup memory, freezing, checkpointing,
or killing processes *in priority order* before stall time reaches the point of no
return, then restoring them when pressure clears.

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

- **PressureResponder (evictor, 5 s):** the hot loop. Reads PSI, updates the
  continuous pressure controller, runs CPU throttling and idle cgroup reclaim at
  Normal pressure, executes freeze/terminate/kill/checkpoint at Elevated+, and runs
  the two *quick* system pre-actions (zram compact, page-cache drop). Must stay
  responsive, so nothing that blocks for long lives here.
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

### Pressure levels & Continuous Controller (`psi.rs`, `evictor.rs`)

`PressureLevel` is an ordered enum (`Normal < Elevated < High < Critical <
Emergency`), derived `PartialOrd`/`Ord` so callers can gate on
`level >= Elevated`. Instead of mapping raw PSI directly, the core daemon uses a **damped feedback controller** to determine the active level:

1. **Pressure Score ($P$):** A normalized score ($0.0$ to $1.0$) representing overall memory stress:
   $$P = w_{\text{PSI}} \cdot S_{\text{PSI}} + w_{\text{swap}} \cdot S_{\text{swap}} + w_{\text{GPU}} \cdot S_{\text{GPU}}$$
   where the weights are $w_{\text{PSI}} = 0.60$, $w_{\text{swap}} = 0.25$, and $w_{\text{GPU}} = 0.15$. The sub-scores are normalized between $0.0$ and $1.0$:
   *   $S_{\text{PSI}} = \text{clamp}\left(\frac{\text{PSI}_{\text{some\_avg10}}}{100.0},\, 0.0,\, 1.0\right)$
   *   $S_{\text{swap}} = \text{clamp}\left(\frac{\text{Swap}_{\text{used\_pct}}}{100.0},\, 0.0,\, 1.0\right)$ (defaults to $0.0$ if no swap exists)
   *   $S_{\text{GPU}} = \text{clamp}\left(\frac{\text{GPU}_{\text{UMA\_kb}}}{\text{RAM}_{\text{total\_kb}}},\, 0.0,\, 1.0\right)$ (total GPU residency reported by driver plugins)
2. **Trend ($T$):** The derivative ($dP/dt$) calculated dynamically across cycles to measure pressure velocity (change per second):
   $$T = \frac{dP}{dt} \approx \frac{P_t - P_{t-1}}{\Delta t}$$
   where $\Delta t$ is the precise time elapsed (in seconds) since the last cycle.
3. **State Machine with Hysteresis:** An internal state machine determines the target pressure tier (`Calm` -> `Warning` -> `Evicting` -> `Critical` -> `Emergency`).
   * **Escalation Delay:** To transition up the severity ladder, the target state must persist for at least **2 consecutive cycles** (ticks), preventing reactions to instantaneous spikes.
   * **Instant Escalation:** If a massive spike is detected (`Critical`/`Emergency`) with a high positive trend ($T > 0.08$), the delay is bypassed to protect the system.
   * **Recovery Cooldown:** Transitioning down the ladder requires the calmer state to persist longer (e.g., 1 minute of Calm before restoring/unfreezing).

The resulting state is mapped 1-to-1 to the `PressureLevel` enum.

**Active Foreground Protection:** If a desktop plugin reports the active window's PID via IPC (`PluginMessage::ActiveWindow`), its config priority is temporarily offset by `-25` inside `plan()`, shielding it from eviction actions unless no other candidates exist.


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
2. Compute the **RAM deficit** ($D$):
   $$D = \text{Target}_{\text{avail}} - \text{MemAvailable}$$
   $$\text{Target}_{\text{avail}} = 0.15 \cdot \text{RAM}_{\text{total}}$$
   If $D \le 0$, there is no deficit and no action is taken.
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

`HealthBaseline` tracks a rolling baseline of available RAM during healthy (Normal pressure) periods using an Exponential Moving Average (EMA) with $\alpha = 0.05$:
$$B_t = \alpha \cdot \text{MemAvailable}_t + (1 - \alpha) \cdot B_{t-1}$$

To decide if it is safe to unfreeze or restore a process, `safe_to_restore` verifies:
$$\text{MemAvailable} - \text{RSS}_{\text{process}} > 0.85 \cdot B_t$$

If the baseline has fewer than 10 samples, it falls back to a conservative threshold:
$$\text{MemAvailable} - \text{RSS}_{\text{process}} > 0.10 \cdot \text{RAM}_{\text{total}}$$

This prevents recovery actions from immediately re-triggering pressure.

---

## 7. Acting (`src/executor/`)

| Module | Responsibility |
|--------|----------------|
| `freezer.rs` | `freeze`/`unfreeze` (SIGSTOP/SIGCONT), plus `*_checked` variants |
| `killer.rs` | `terminate` (SIGTERM → 5 s wait → SIGKILL), `kill` (SIGKILL) |
| `checkpoint.rs` | CRIU dump/restore; absolute-path criu resolution |
| `registry.rs` | `FrozenRegistry`, `CheckpointRegistry` (persisted state) |
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

Both are **persisted to disk** in `~/.local/share/mgd/state/*.json`. A daemon restart restores the registry state so previously frozen processes can still be unfrozen and checkpointed processes restored. `cleanup_orphaned_snapshots()` is run at startup to remove any snapshot directories on disk that are not tracked in the recovered `CheckpointRegistry`.
`CheckpointRegistry` tracks a per-entry restore-attempt count; after
`MAX_RESTORE_ATTEMPTS = 3` the snapshot is abandoned and removed.

### CRIU specifics

Checkpoint/restore is mediated by the `mgd-checkpoint` wrapper binary (see §9).
`checkpoint.rs` resolves `mgd-checkpoint` by absolute path (sibling of the `mgd`
binary, or `~/.local/bin/mgd-checkpoint`), then execs:

Dump: `mgd-checkpoint dump <pid> <images-dir>` → wrapper validates and execs
`criu dump --tree <pid> --images-dir <dir> --shell-job --leave-stopped --ext-unix-sk
--tcp-established --file-locks`. SIGKILL on success. Snapshots live in
`~/.local/share/mgd/snapshots/<pid>_<name>/`.

Restore: `mgd-checkpoint restore <images-dir>` → wrapper execs
`criu restore --images-dir <dir> --shell-job --restore-detached`.

If `mgd-checkpoint` is absent, `checkpoint.rs` falls back to direct criu invocation
(legacy path); if criu itself is missing or exits non-zero, falls back to SIGKILL
and logs once. See §9 and `PRIVILEGE_DESIGN.md` §3.

---

## 8. Non-destructive Reclaim (System-level & Early Process Reclaim)

These act on the *system* or specific *background processes* before `plan()`, freeing RAM or pushing memory to swap/zram without destroying application state, reducing the need for destructive evictions.

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

### early background process cgroup reclaim (`evictor.rs`, inline pre-action, Elevated)

To proactively free RAM before pressure escalates, `mgd` implements early background cgroup reclaim. When pressure is `Elevated`, it selects up to 3 background processes that:
- Have a priority $\ge 50$ (expendable/user applications).
- Have an RSS $> 20$ MB.
- Are not the currently active foreground process (reported by plugins).

It reclaims $50\%$ of their RSS by writing the target bytes to their cgroup's `memory.reclaim` node (e.g., `/sys/fs/cgroup/user.slice/.../memory.reclaim`). Since the cgroup file hierarchy under the user's slice is owned by the user, this is fully unprivileged and does not require elevated privileges (`CAP_SYS_ADMIN`). A $30$ s cooldown is enforced between runs.

### idle background process cgroup reclaim (`evictor.rs`, Normal pressure only)

At Normal pressure, `check_idle_process_reclaim()` identifies background processes that:
- Are not the active foreground process (reported by plugins via `ActiveWindow`).
- Have priority ≥ 50 and RSS ≥ `idle_reclaim_rss_min_mb` (default 50 MB).
- Have been continuously in the background for ≥ `idle_sec` (default 180 s).

It writes `reclaim_pct`% of their RSS (default 20%) to `memory.reclaim` for their cgroup.
Skipped if swap occupancy exceeds `max_swap_occupancy_pct` (default 60%) — pushing more
into an already-full swap would be counterproductive. A global cooldown of
`global_cooldown_sec` (default 30 s) prevents back-to-back sweeps. Fully unprivileged.
Gated by `[idle_reclaim] enabled` (on by default).

### proactive swap reclaim (`maintenance.rs`, maintenance thread, Normal only)

The headline fix for the MGLRU lazy-swap-in problem. When calm, cycle the zram
swap device (`swapoff`/`swapon`) to pull all compressed pages back into RAM.
Blocks, so it runs on the maintenance thread. **Privileged** (`CAP_SYS_ADMIN`) —
carried by the `mgd-zram-reclaim` helper (see §9), **off by default**. All policy
gates live in the unprivileged daemon:

- Normal pressure **and** `some_avg60 < 5 %` (calm, not a just-subsided spike)
- cooldown elapsed (`reclaim_cooldown_min`, default 10 min)
- `zram_used ≥ min_zram_used_mb` (default 2048)
- **OOM headroom gate (critical):** Restricts swap reclaim unless:
  $$\text{MemAvailable} > \text{Footprint}_{\text{decompressed}} \cdot m_{\text{headroom}}$$
  where $m_{\text{headroom}} = 1.5$ (default) and $\text{Footprint}_{\text{decompressed}}$ is the sum of `orig_data_size` (field 0 of `/sys/block/zramN/mm_stat`) across all zram devices. Because zram stores compressed pages that expand 2–3× on decompression, this gate prevents the reclaim operation itself from triggering a system OOM.

---

## 8.5 CPU throttling / App Nap (`evictor.rs`, per-cycle)

Background processes that have been out of the foreground for ≥ 10 s (debounce) have their
cgroup CPU weight reduced via `cpu.weight` and `cpu.max`. `ThrottledState` is a two-tier
enum (Light / Heavy) mapped by priority tier and background duration. Hard rules:

- The active foreground cgroup (from `ACTIVE_FOREGROUND_PID`) is restored to full CPU
  weight instantly whenever the active window changes.
- Priority < 60 processes are never throttled (IDEs, DBs — low-priority but interactive).
- All throttled cgroups are restored on daemon shutdown.

This is unprivileged: `cpu.weight` and `cpu.max` under `user.slice` are user-writable.
The evictor reads the active foreground PID from `plugin_server::get_active_foreground_pid()`
(updated by DE plugins via `PluginMessage::ActiveWindow`).

---

## 8.6 Plugins (`mgd-kde`, `mgd-gpu-intel`, etc.)

Historically, environment-specific watchers lived inside `mgd`. They are now decoupled into
standalone plugin processes. `plugin_server::init_plugins()` (called at daemon startup)
auto-detects and spawns the right binaries:
- **DE detection**: reads `$XDG_CURRENT_DESKTOP` → spawns `mgd-kde`, `mgd-gnome`, or `mgd-cosmic`.
- **GPU detection**: reads `/sys/class/drm/*/device/driver` symlinks → `i915`/`xe` spawns
  `mgd-gpu-intel`; `amdgpu` spawns `mgd-gpu-amd`.

Active plugins:
- **`mgd-kde`**: Handles restarting the KDE `plasmashell` on GPU leaks, and reaping
  `plasma-discover` on CPU idle. Reports active window PID via `ActiveWindow`.
- **`mgd-gpu-intel`**: Scans `/proc/<pid>/fdinfo/` (DRM) to account for UMA graphics memory.

Plugins observe the system and send `PluginMessage` to core; core routes via
`plugin_server::serve_plugin_connection()`. Core broadcasts `CoreMessage::PressureChanged`
each evictor cycle so plugins can adapt their polling rate.

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
| CRIU dump/restore | `mgd-checkpoint` wrapper → execs capped `criu` | `CAP_CHECKPOINT_RESTORE` + `CAP_SYS_PTRACE` (+`CAP_NET_ADMIN` for live TCP) | on if criu present |

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

### `mgd-checkpoint` (CRIU validating wrapper)

`criu` is already its own external binary, so caps could go directly on it (Option A).
Instead, `mgd-checkpoint` (`mgd-checkpoint/src/main.rs`) is a thin validating wrapper
(Option B) that mgd execs in place of criu directly. It:
- Accepts only `dump <pid> <images-dir>` or `restore <images-dir>` — no other argv.
- Validates caller owns the target PID (`/proc/<pid>` uid check), the process lives in
  `user.slice` (cgroup path check), and the images dir exists under the caller's home.
- Raises ambient capabilities (`CAP_CHECKPOINT_RESTORE`, `CAP_SYS_PTRACE`,
  `CAP_NET_ADMIN`) onto the inheritable set so they are inherited by the child `criu`
  process, then execs criu with `env_clear()`.
- Exit codes: 0 = ok, 1 = bad args, 2 = security validation failed, 3 = criu failed.

This keeps the caps off the `criu` binary itself and adds a security validation layer
even in the single-user desktop case.

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
(`.desktop` fallback), `[zram]`, `[reclaim]`, `[cache_drop]`, `[psi]` (pressure-tier
boundaries, validated as a set), `[idle_reclaim]` (idle background cgroup reclaim
— `enabled`, `idle_sec`, `rss_min_mb`, `reclaim_pct`, `global_cooldown_sec`,
`max_swap_occupancy_pct`), `[thresholds]` (`target_available_pct` — explicit override
above the passive calibration file and RAM-scaling defaults).

Reload is triggered by SIGHUP or `mgctl reload`; the responder picks it up at the
top of its next cycle. `.desktop` files are re-scanned on each load to rebuild the
`exe_basename → priority` index.

---

## 11. IPC & control (`src/ipc.rs`, `mgctl/src/main.rs`)

The daemon serves a newline-delimited request/response protocol on a Unix socket
at `$XDG_RUNTIME_DIR/mgd.sock` (fallback `/tmp/mgd-<uid>.sock`). Responses are
`OK <data>` / `ERR <msg>`. Connections are short-lived threads, capped at
`MAX_CONNECTIONS = 8`; a stale socket from a crash is detected and rebound.

`mgctl` is the client and splits cleanly into three categories:
- **socket commands** (daemon must be alive): `status`, `list`, `unfreeze`,
  `reload`.
- **lifecycle commands** (wrap `systemctl --user` / `journalctl --user`, work
  even when the daemon is down — which the socket cannot): `restart`, `start`,
  `stop`, `service` (systemd unit state), `logs [-f]`.
- **standalone utility commands**: `doctor` (environment + feature report) and `calibrate` (derive per-machine PSI thresholds).

`status` is the daemon's live view (pressure, frozen counts); `service` is the
systemd unit view (active state, PID, uptime, memory) — deliberately distinct.

---

## 12. Logging (`src/logger.rs`)

One session log file per run at `~/memlogs/mgd_<YYYY-MM-DD_HH-MM-SS>.log` (local
time), shared via `Arc<Logger>` (so rotation runs once, not per-thread). `LogEntry { action, pid, name, rss_mb,
result }` is the structured line format. Rotation keeps the newest `log_keep`
files (default 10; 0 = unlimited). Actions seen in logs: `FREEZE`, `UNFREEZE`,
`TERMINATE`, `KILL`, `CHECKPOINT`, `RESTORE`/`RESTORE_FAIL`/`RESTORE_ABANDON`,
`ZRAM`, `RECLAIM`, `CACHE`, `REAP`, `RESTART`, `EARLY_RECLAIM`.

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
| `/sys/fs/cgroup/.../memory.reclaim` | trigger proactive cgroup memory reclaim (user write) |

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
| daemon restart | registry state recovered from disk; orphaned snapshots cleaned at startup |
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
