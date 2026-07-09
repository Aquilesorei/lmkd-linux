# mgd — Privilege Design

How mgd gains the small amount of OS privilege some features need, without
running as root and without arming the long-lived daemon with broad
capabilities.

---

## The core constraint: mgd is a *user* service

`config/mgd.service` is a **systemd user unit** (`ExecStart=%h/.local/bin/mgd`,
`WantedBy=graphical-session.target`). It runs under the systemd *user* manager
as the unprivileged login user.

This matters because:

> A user service **cannot** be granted capabilities via
> `AmbientCapabilities=` / `CapabilityBoundingSet=`. The user manager has no
> privilege to hand out capabilities it does not itself hold. Those directives
> are silently ineffective (or fail) in `--user` units.

So the plan in `NEXT_FIXES.md` (add `AmbientCapabilities=CAP_SYS_ADMIN` to the
service) **does not work** for this service as written. Privilege has to be
attached to a **file on disk** (`setcap` on a binary, or a permission grant on
a device/sysfs node) — those are honored no matter who launches the process.

This single fact is *why* the helper-binary pattern below is the right design
rather than just adding capabilities to the daemon.

---

## Principles

Whenever mgd needs a privileged operation:

1. **Narrowest capability, never root.** Find the single capability the exact
   syscall requires. Never full root, never `CAP_SYS_ADMIN` when something
   smaller exists.
2. **Smallest possible carrier.** Attach the privilege to the smallest unit of
   code — ideally a fixed-function helper that takes **no parameters**, or, even
   better, to no binary at all (a device/sysfs permission grant).
3. **Policy stays in mgd.** All decision logic — thresholds, cooldowns, safety
   gates — lives in the unprivileged daemon. The privileged carrier is *dumb*:
   it performs one mechanical action and exits. Dumb = auditable.
4. **No untrusted input into privileged code.** No argv, no env reliance, no
   subprocess exec unless unavoidable. If a target *must* be passed (CRIU's
   PID), the carrier validates it (owner + cgroup) before acting.
5. **Group-gate execution.** Privileged helpers are `root:mgd`, mode `0750` —
   only members of the `mgd` group can run them, not every local user.
6. **Optional + graceful degradation.** Every privileged feature is opt-in. If
   the helper/grant is absent, mgd detects it, logs once, and skips the feature.
   Root is always optional.

A consequence of principle 2: **never build one general "do-root-thing"
helper.** A multi-action helper that takes a command argument is just root with
extra steps. Prefer several tiny single-purpose carriers, each holding only the
capability its one action needs.

---

## Two tiers of privileged operation

### Tier A — zero-input, fixed action (safe, easy)

No target, no parameter. The carrier (or grant) does exactly one hardcoded
thing. Nothing to inject. This is the safe default; push operations into this
tier whenever possible.

### Tier B — operation needs a target (higher risk)

The operation acts on something chosen at runtime (e.g. CRIU on a PID). Input
exists, so it must be validated inside the privileged carrier. The validating
carrier is the audit-critical code and gets extra scrutiny.

---

## Operation catalog

### 1. zram compact — Tier A, **no capability, no binary**

Operation: write `1` to `/sys/block/zram0/compact`.

The sysfs node is `--w------- root root` (mode `0200`, owner root) — the login
user **cannot** write it by default. But this needs no capability and no helper
binary; it only needs write permission on that one node. Grant it declaratively:

`/etc/tmpfiles.d/mgd-zram.conf`
```
# type path                       mode uid  gid mode age arg
z     /sys/block/zram0/compact    0220 root mgd  -   -
```

(Or an equivalent udev rule keyed on the zram device.) After
`systemd-tmpfiles --create`, the `mgd` group can write the node and mgd compacts
zram directly — **zero privileged code**.

- Carrier: none.
- Capability: none.
- Input: none.
- Graceful degrade: if the node is not group-writable, the write fails with
  `EACCES`; mgd logs "zram compact unavailable" once and disables the feature.

> Note: `/proc/swaps` device size and current usage vary; gate compaction on a
> minimum zram-used threshold so it is skipped when not worthwhile.

---

### 2. swap reclaim (swapoff/swapon zram) — Tier A, **`CAP_SYS_ADMIN`**

Operation: pull all compressed pages back to RAM by cycling the zram swap
device. `swapoff(2)` then `swapon(2)` are genuine `CAP_SYS_ADMIN` syscalls.
There is no narrower capability for them.

Carrier: a new third binary, **`mgd-zram-reclaim`**, separate from `mgd`/`mgctl`.

```
setcap cap_sys_admin+ep  /usr/local/bin/mgd-zram-reclaim
chown root:mgd           /usr/local/bin/mgd-zram-reclaim
chmod 0750               /usr/local/bin/mgd-zram-reclaim
```

`+ep` means the binary runs with **only** `CAP_SYS_ADMIN` effective — it is
**not** SUID root, never holds uid 0. Strictly smaller blast radius than a
`4755` SUID-root wrapper.

Carrier rules (all mandatory):

- **No parameters.** Takes no argv. One invocation = one full reclaim cycle.
- **No env trust.** Ignore all environment. (glibc secure-execution mode already
  neutralizes `LD_PRELOAD`/`LD_*` for `+ep` binaries — defense in depth, still
  don't read env.)
- **No subprocess.** Call `libc::swapoff` / `libc::swapon` directly. Never shell
  out to `/sbin/swapoff` (PATH hijack risk).
- **Validate the target is zram.** Read `/proc/swaps`, confirm the device path is
  a canonical `/dev/zram<N>` (anchored on the full path, *not* a basename
  `starts_with("zram")` — a swapfile named `zram-cache` must not match), before
  touching it. Prevents ever disabling a real disk-swap partition or a lookalike
  swapfile.
- **Self-enforce the OOM floor (the helper is group-executable).** The binary is
  `0750 root:mgd`, so *any* `mgd`-group process can run it directly — not only
  the daemon. It therefore cannot rely on the daemon's headroom gate. Before any
  `swapoff`, the helper itself reads `MemAvailable` (`/proc/meminfo`) and the
  decompressed footprint (`orig_data_size` from `/sys/block/zramN/mm_stat`, both
  world-readable, no extra privilege) and **refuses** (distinct exit code, no
  syscall made) unless available RAM strictly exceeds the decompressed total.
  This makes the privileged action safe *by construction* regardless of caller.
- **Atomic, never strand the system.** Perform `swapoff` then `swapon` in one
  invocation and never return with swap left off. Block terminating signals
  (SIGINT/TERM/HUP/QUIT) across the pair, and retry `swapon` on failure, so an
  interrupt between the two cannot leave the system swapless.
- **Distinct exit codes.** `0` ok/nothing-to-do, `2` swapoff EPERM (binary not
  capped — persistent, caller disables the feature), `3` refused for unsafe
  headroom and `4` meminfo-unreadable and `1` transient (all retried next
  cycle). Lets the daemon tell "uncapped" from "transient" without string-matching.

Policy stays in mgd (the daemon, unprivileged) — a *stricter* layer on top of the
helper's hard floor:

- `PressureLevel::Normal` and pressure calm (e.g. `some_avg60 < 5%`).
- Cooldown (e.g. ≥10 min between cycles).
- Minimum zram-used threshold (skip small amounts).
- **Decompressed-size headroom gate (critical).** zram stores *compressed*
  data; decompressed footprint can be 2–3×. The free-RAM check must compare the
  **decompressed estimate** (use `zramctl` original-data-size, not the
  compressed figure) against available RAM, or the reclaim itself can OOM the
  system at the moment all pages land back in RAM. Require a real margin
  (e.g. MemAvailable > decompressed-size × 1.5). The helper independently
  enforces a bare `> 1.0×` floor so a direct invocation that bypasses this
  policy still cannot self-OOM.

mgd evaluates all gates, then — and only then — execs `mgd-zram-reclaim`.

- Carrier: `mgd-zram-reclaim`, `cap_sys_admin+ep`.
- Capability: `CAP_SYS_ADMIN` (no narrower option exists).
- Input: none.
- Graceful degrade: if the binary is missing or lacks the cap, the
  exec/operation fails; mgd logs "swap reclaim unavailable" once and disables
  the feature.

---

### 3. CRIU checkpoint/restore — Tier B, **`CAP_CHECKPOINT_RESTORE` + `CAP_SYS_PTRACE`**

This is the existing optional feature (`src/executor/checkpoint.rs`). Today
CLAUDE.md says it needs "`CAP_SYS_PTRACE` or root." Under this design its
privilege requirement **shrinks** and stops needing root.

CRIU is fundamentally Tier B and breaks the safe no-input pattern, because:

- it acts on a **PID chosen at runtime** (`criu dump -t <pid>`), and
- mgd invokes it by **execing the external `criu` binary**
  (`Command::new("criu")` at `checkpoint.rs:56` and `:134`).

So the clean no-param carrier does not apply — input must be validated.

**Capability — not root, not `CAP_SYS_ADMIN`.** Kernel 5.9 added
`CAP_CHECKPOINT_RESTORE` specifically for unprivileged checkpoint/restore; this
system (kernel 7.0) has it. CRIU dump/restore wants:

```
CAP_CHECKPOINT_RESTORE + CAP_SYS_PTRACE
```

— far narrower than full root and narrower than `CAP_SYS_ADMIN`.

Two ways to grant it:

| Option | How | Trade-off |
|---|---|---|
| **A. setcap criu directly** | `setcap cap_checkpoint_restore,cap_sys_ptrace+ep $(command -v criu)` | Simplest, no new code. But any local user can then run criu near-privileged (limited to dumping their own-uid processes). Mild risk. |
| **B. validating wrapper** | `mgd-checkpoint <pid>`, capped, validates the PID, then execs criu | Safer (gates which PID), but new code + must sanitize the exec. |

**Option B (validating wrapper) is the chosen and implemented path** — `mgd-checkpoint` (`mgd-checkpoint/src/main.rs`) is built and ships as part of the workspace. It:

- Accepts only `dump <pid> <images-dir>` or `restore <images-dir>` — no other argv.
- Validates caller owns the target PID (`/proc/<pid>` uid check), the process lives in `user.slice` (cgroup path check), and the images dir exists under the caller's home directory.
- Raises ambient capabilities (`CAP_CHECKPOINT_RESTORE`, `CAP_SYS_PTRACE`, `CAP_NET_ADMIN` for live TCP restore) onto the inheritable set so they are inherited by the child `criu` process.
- Execs `criu` with a cleared environment (`env_clear()`).
- Exit codes: 0 = ok, 1 = bad args, 2 = security validation failed, 3 = criu failed.

`src/executor/checkpoint.rs` resolves `mgd-checkpoint` by **absolute path** (sibling of the `mgd` binary or `~/.local/bin/mgd-checkpoint`) — never a PATH search. If `mgd-checkpoint` is absent, it falls back to direct criu invocation (legacy path); if criu itself is missing or unprivileged, falls back to SIGKILL and logs once.

The caps are placed on `mgd-checkpoint` (not on `criu` itself), so `criu` remains an ordinary binary. Any local user running `criu` directly gets no elevated capabilities.

> **Option A** (setcap on `criu` directly) is simpler but extends near-privilege to all local users who can run `criu`. Option B adds a security validation layer — the wrapper confirms the PID is user-owned and in `user.slice` before acting. Option B is the implemented and preferred path.

- Carrier: `mgd-checkpoint` wrapper binary.
- Capability: `CAP_CHECKPOINT_RESTORE` + `CAP_SYS_PTRACE` + `CAP_NET_ADMIN`.
- Input: PID + images-dir (both validated inside the wrapper).
- Graceful degrade: `checkpoint.rs` falls back gracefully when `mgd-checkpoint` is missing or fails.

---

### 4. PSI kernel trigger — Tier A, **`cap_perfmon+ep` (retained for compat)**

Operation: arm a kernel PSI pressure trigger for zero-CPU idle waiting.

**Kernel 7.x breaking changes (discovered 2026-06-29):**
- `/proc/pressure/memory` trigger writes return `EINVAL` unconditionally — the global file no longer accepts trigger arming.
- Minimum trigger window raised from 500ms to **2s** (must be an exact multiple of 2s; `1000000`µs returns EINVAL).
- Cgroup PSI files (`/sys/fs/cgroup/.../memory.pressure`) owned by the user still work with window ≥ 2s.

**New behavior:** `mgd-psi-trigger` reads `/proc/self/cgroup`, walks the cgroup hierarchy upward, and arms the trigger on the highest writable `memory.pressure` file (typically `user@UID.service/app.slice/memory.pressure` — owned by the user, captures pressure across all user applications). Window `2000000`µs (2s) is used.

`cap_perfmon+ep` is retained on the binary for compatibility with kernels < 7.x where `/proc/pressure/memory` required that capability. On kernel 7.x it is not exercised.

- Carrier: `mgd-psi-trigger`.
- Capability: `cap_perfmon+ep` (compat; not required on kernel 7.x cgroup path).
- Input: none (cgroup path derived from `/proc/self/cgroup`; stall_us from argv, validated positive integer).
- Graceful degrade: if no writable PSI file found → exits with code 2 → daemon falls back to in-process `PsiTrigger` → then 5s polling.

---

### 5. Memory locking (mlockall) — Tier A, **`CAP_IPC_LOCK` on `mgd` itself**

Operation: `mlockall(MCL_CURRENT | MCL_FUTURE)` at daemon startup so mgd's own
pages can never be swapped to zram. Without it, a page fault in the eviction
hot path — while the system is already thrashing — is unbounded latency at
exactly the moment the daemon must act.

This is the one grant that lives on the `mgd` binary itself (alongside
`CAP_SYS_NICE` for the evictor's SCHED_RR), because memory locking is
process-wide and cannot be delegated to a helper. It slightly bends the
"user service holds no caps" rule; the cap is inert — it only removes the
`RLIMIT_MEMLOCK` bound on locking mgd's *own* pages and grants no access to
other processes or system state.

- Carrier: `mgd` (no helper possible — process-wide operation).
- Capability: `CAP_IPC_LOCK` (+ `CAP_SYS_NICE` on the same binary).
- Input: none.
- Graceful degrade: without the cap, `RLIMIT_MEMLOCK` (8MB default) bounds
  locking, and `MCL_FUTURE` under a finite rlimit would make future
  allocations *fail* — so mgd detects this and locks current pages only
  (`MCL_CURRENT`). If even that fails, it runs unlocked and logs once.

---

## Capability cheat-sheet

| Operation        | Carrier                        | Capability                                   | Input |
|------------------|--------------------------------|----------------------------------------------|-------|
| zram compact     | none (tmpfiles sysfs grant)    | none                                         | none  |
| swap reclaim     | `mgd-zram-reclaim`             | `CAP_SYS_ADMIN`                              | none  |
| CRIU dump/restore| `mgd-checkpoint` wrapper       | `CAP_CHECKPOINT_RESTORE` + `CAP_SYS_PTRACE` + `CAP_NET_ADMIN` | PID + images-dir (both validated) |
| PSI trigger      | `mgd-psi-trigger`              | `cap_perfmon+ep` (compat; not needed on 7.x) | stall_us from argv (validated) |
| RT scheduling    | `mgd` (daemon binary)          | `CAP_SYS_NICE`                               | none  |
| memory locking   | `mgd` (daemon binary)          | `CAP_IPC_LOCK`                               | none  |

SIGSTOP/SIGCONT (freezer), SIGTERM/SIGKILL (killer), SIGUSR1 (Firefox GC), and
fdinfo GPU reads all work on own-uid processes with **no** privilege and are not
in scope here.

---

## Install (opt-in)

Privilege grants are separate from the normal build/install so the daemon works
unprivileged out of the box. A user opts into each feature explicitly.

The `mgd` group gates who may run the capped helpers:

```bash
sudo groupadd -f mgd
sudo usermod -aG mgd "$USER"   # log out/in for membership to take effect
```

zram compact (Operation 1):

```bash
sudo install -m 0644 packaging/mgd-zram.conf /etc/tmpfiles.d/mgd-zram.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/mgd-zram.conf
```

swap reclaim (Operation 2):

```bash
sudo install -m 0750 -o root -g mgd target/release/mgd-zram-reclaim \
    /usr/local/bin/mgd-zram-reclaim
sudo setcap cap_sys_admin+ep /usr/local/bin/mgd-zram-reclaim
```

CRIU (Operation 3), Option B — `mgd-checkpoint` wrapper (implemented):

```bash
sudo install -m 0750 -o root -g mgd target/release/mgd-checkpoint \
    /usr/local/bin/mgd-checkpoint
sudo setcap cap_checkpoint_restore,cap_sys_ptrace,cap_net_admin+ep \
    /usr/local/bin/mgd-checkpoint
```

`install.sh --privileged` does this automatically. Caps on `mgd-checkpoint`, not on `criu` itself.

PSI trigger (Operation 4):

```bash
sudo install -m 0755 target/release/mgd-psi-trigger /usr/local/bin/mgd-psi-trigger
sudo setcap cap_perfmon+ep /usr/local/bin/mgd-psi-trigger
```

Note: on kernel 7.x the cap is not required (cgroup PSI files are user-owned). It is retained for compatibility.

RT scheduling + memory locking (Operation 5, caps on the daemon binary itself):

```bash
sudo install -m 0755 target/release/mgd /usr/local/bin/mgd
sudo setcap cap_sys_nice,cap_ipc_lock+ep /usr/local/bin/mgd
```

Each step is independent. Skipping one disables only that feature; mgd logs the
capability as unavailable at startup and continues.

---

## Detection / graceful degradation

At startup (and on reload) mgd probes each privileged feature once and records
availability, so a missing grant never causes a hard failure mid-cycle:

- **zram compact** — test-open `/sys/block/zram0/compact` for write
  (`O_WRONLY`); `EACCES` ⇒ unavailable.
- **swap reclaim** — `access(X_OK)` on `/usr/local/bin/mgd-zram-reclaim`; absent
  or non-executable ⇒ unavailable. (The cap itself can only be confirmed by
  attempting the op; treat the first `EPERM` as a disable signal.)
- **CRIU** — existing behavior: criu missing or returning a privilege error ⇒
  fall back, log once.

Each unavailable feature is logged exactly once at the level it would have run,
then silently skipped on subsequent cycles.

---

## Security summary

- Daemon stays unprivileged for its whole lifetime; no broad capability is held
  across the days-long process lifetime.
- Privilege lives on disk (file caps / sysfs grant), which is the only thing
  that works for a `--user` service anyway.
- Each privileged carrier holds the **one** capability its single action needs —
  `CAP_SYS_ADMIN` only for swap, the checkpoint/ptrace pair only for CRIU, and
  **nothing** for zram compact.
- Zero-input carriers have no injection surface; the one input-taking case
  (CRIU PID) is validated against owner + cgroup.
- Execution is group-gated (`root:mgd`, `0750`); not every local user can invoke
  the helpers.
- Every feature is optional and degrades gracefully — **root remains entirely
  optional.**
