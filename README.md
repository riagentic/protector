<img src="assets/icon.svg" width="96" alt="Protector icon" align="right">

# Protector

Keeps a workstation responsive. It watches memory, swap and **pressure stall
information (PSI)** — the kernel's own measure of "the machine cannot make
progress" — scores every process tree by how much it threatens stability, and
kills one runaway job only when the OS is about to lock up.

> **Cloned this repo?** The framework link and `node_modules` are gitignored —
> run `am fix` once, then `deno task service` and `am start --client=electron`.

It is deliberately **not** a "heavy app killer". A busy CPU, a full swap or a
huge process means nothing and never triggers a kill. What freezes a machine is
memory running out with nowhere left to go, so that is what it acts on — plus,
under much stricter rules, a CPU hog that starves the OS itself.

```
protector/
├─ assets/icon.svg          icon source (→ src/icon.png, 512 px)
├─ src/                     aio + Electron UI (this app) — the window
│  ├─ cell.ts               monitor cell: polls the daemon, holds the snapshot
│  ├─ protectord.server.ts  server-only bridge to the daemon (loopback HTTP)
│  ├─ App.tsx, ui/, style.css
│  └─ app.ts
└─ service/                 protectord — the Rust daemon (the brains + the armor)
   ├─ src/proc.rs           /proc + /proc/pressure readers
   ├─ src/policy.rs         trees, harm score, freeze policy, victim choice
   ├─ src/model.rs          the JSON snapshot
   ├─ src/api.rs            loopback HTTP API (bearer token)
   ├─ src/config.rs         config, defaults, persisted token
   └─ src/main.rs           sampler loop, signals, runtime file
```

The **daemon protects; the app shows.** The daemon runs independently of the UI
(it survives the window closing), which is the point: closing the dashboard must
not stop protection.

## Quick start

```sh
# 1. Build the daemon (once)
cd service && cargo build --release && cd ..

# 2. Run the desktop app (auto-installs Electron on first run)
am start --client=electron        # or: am start --client=server-only

# 3. Arm it (from the app, or persistently in the config)
#    The daemon starts in OBSERVE mode: it logs what it *would* kill and kills
#    nothing. Click "observe — arm" in the header, or run:
#      ~/.protector/config.json  ->  "mode": "armed"
#    or start it armed:
#      ./service/target/release/protectord --armed
```

The app will also **start the daemon for you** if it is not running: the offline
banner has a _start service_ button, and the daemon is spawned detached so it
outlives the app.

## What the UI shows

- **Freeze risk** — 0–100, the closest the machine is to a freeze, plus the
  reasons and any active trigger (with the chosen victim).
- **System** — RAM available, swap, CPU busy, load, D-state tasks (processes
  stuck in uninterruptible sleep — a strong freeze signal), and PSI for memory,
  CPU and I/O. Sparklines for the last ~3 minutes.
- **Signals** — every active negative signal, from system-wide (`swap 63%`,
  `memory PSI full10 18`) to per-tree.
- **Processes** — the top process trees by **harm**, each with its harm bar, RSS
  and share of RAM, CPU (cores), memory **growth rate**, negative-signal chips,
  and a kill button. Protected (system/other-user) trees are locked. Filter by
  name/cmd/pid, sort by harm/RSS/CPU/growth/name, "killable only".
- **Activity** — the daemon's event log: kills, would-kills, mode changes,
  holds, protections.

## Policy

Two stages. **WARN** is broad and only shows signals. **KILL** is narrow and
fires only near a genuine freeze; the victim must also be big enough to help.

A memory kill needs tasks **stalled on machine-wide reclaim, right now**: memory
PSI `some10` > 10 or `full10` > 5, **and** a task had to reclaim memory itself
in the last 10 s (`allocstall_*` in `/proc/vmstat` — a cgroup at its own limit
does not move it). On top of that, one of:

| Condition                                                        | Held for | Action            |
| ---------------------------------------------------------------- | -------- | ----------------- |
| **Exhausted**: available RAM < 5 % **and** swap > 85 % (or none) | 10 s     | kill the job      |
| **PANIC**: the same with available RAM < 2 %                     | 3 s      | kill, no cooldown |
| **Gridlock**: available RAM < 15 % **and** PSI `full10` > 30     | 30 s     | kill the job      |

| Otherwise                                                                                                                        | Action                                                        |
| -------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------- |
| OS starved of CPU for `cpu_sys_kill_sustain`: CPU ≥ 98 %, CPU PSI `some60` > 40 **and** the daemon's own sleep overruns by > 2 s | kill the job burning it, if its tree uses ≥ 50 % of all cores |
| Swap full · CPU busy · one process very large · RAM low with swap left · a big load swapping out                                 | never a kill                                                  |
| Kernel log: device inaccessible, hung task, lockup, stall                                                                        | alert — never a kill                                          |

**A level is not a freeze.** Swap does not drain when its cause goes away, so
"swap 60 % used" is idle pages parked on disk (2026-10-08: that rule killed a
79-process agent session and then the browser, with 48 % and 80 % of RAM
available). A 76 GB model loading pushes swap to 72 % and PSI `full10` to 17
with machine-wide reclaim running — and is healthy. Only _no way out_ counts:
RAM and swap both spent, or the machine at a standstill a third of the time. A
pinned CPU with the daemon still waking on time is heavy work, not starvation.

**The victim is one job, not its tree.** From the root of a tree the daemon
walks down into the heaviest child for as long as that child outweighs the
process it stands on and still holds ≥ 5 % of RAM, and signals only what it
lands on: the model server, not the agent, terminal and shell above it. A
fan-out of small workers is one job at their parent. A shell or multiplexer
(`bash`, `tmux`, …) and anything above a protected process is never where the
walk ends — it moves on to the heaviest child, even one too small to kill.
Memory is anonymous RSS + swap, plus the largest shared segment counted once.

**No bystanders.** If one process the daemon may not touch (another user's, a
protected one) holds more RAM than the biggest job, the cause is elsewhere: it
_holds_ and alerts instead — except in a panic, when any big job goes. When RAM
and swap are spent by many small jobs (twenty agents each running a test), the
size floor drops to 1 % of RAM so the biggest of them can still be removed, one
per cooldown.

**Known limits (0.1).** The victim is the biggest job, not provably the one that
grew: a steady 70 GB model goes before a 30 GB leak. Thrashing on file-backed
pages (a memory-mapped model larger than RAM) is not acted on — those pages
count as available. Both are left to the kernel's own OOM killer.

A **stuck kernel** (a device that will not wake, a hung task, a CPU lockup)
freezes the machine with RAM to spare and no process to blame, so killing cannot
help. The daemon follows `/dev/kmsg`, raises the risk to CRITICAL, runs `sync`,
and shows a **desktop alert** (`notify-send`). Any danger/crit signal that holds
for 10 s alerts the same way; a busy CPU alone never does. Opt-in last resort:
with `stall_reboot_s` > 0 (armed mode), a stall that lasts that long with tasks
still stuck gets a 30 s warning, then `sync` + `systemctl
reboot`. logind
refuses that while something blocks shutdown (a package install, say); the
daemon then alerts instead.

Escalation is always **SIGTERM the job → 5 s grace → SIGKILL survivors**, with a
**45 s cooldown** between kills (10 s in a panic, so freed memory is seen before
a second job is chosen). A memory-driven trigger only fires if that job holds at
least **5 % of RAM** — otherwise the daemon logs the signal and _holds_ rather
than killing a small job that would not help. Kill windows and the cooldown run
on the monotonic clock, so a suspend or clock step neither fires nor blocks
them, and an unreadable `/proc/meminfo` never triggers. The daemon locks itself
in RAM (`mlockall`).

**Harm score (per tree, 0–100)** is the worst of: absolute memory danger (share
of RAM vs the kill threshold), relative hogging (share of the biggest tree),
memory growth (MB/s), CPU, attributed memory pressure, OOM score and D-state
tasks. On a machine with plenty of RAM the biggest app scores around 45 — "top
hog, not yet dangerous" — and only real pressure pushes it to 100.

## Safety

- PID 1, kernel threads and core desktop/system daemons are **never** touched
  (`systemd`, `Xorg`, `gnome-shell`, `dbus`, `pipewire`, `NetworkManager`, …).
- The daemon protects **itself, its ancestry, and any pid the app registers**
  (the aio server registers itself on first poll). Extra prefixes can be added
  in the config.
- Only processes owned by **your user** are ever signalled. Other users' trees
  are shown but locked.
- Trees are grouped by their top-most same-user ancestor, so a job's whole
  sub-process fan-out counts — and dies — together.
- **Observe mode** (the default) kills nothing; it records what it would do.

## Configuration

`~/.protector/config.json` (created on first run; override the directory with
`PROTECTOR_HOME`). Every threshold above is a key. `~/.protector/runtime.json`
holds the live pid/port/token; the log is `~/.protector/protector.log`.

```jsonc
{
  "mode": "observe", // "armed" to actually kill
  "interval_ms": 1000,
  "port": 7801,
  "cooldown_s": 45,
  "min_victim_rss_pct": 5,
  "stall_reboot_s": 0, // e.g. 180: clean reboot after a 3 min kernel stall
  "extra_protected": ["my-batch-runner"]
}
```

## Running it as a real service

`service/protector.service` is a systemd **user** unit. Build, install, enable
and (re)start it — armed, at every login:

```sh
deno task service:install     # = sh service/install.sh; re-run after changes
```

The UI connects to whatever daemon is running via `~/.protector/runtime.json`.

## Develop

```sh
deno task check && deno task lint && deno task test   # the aio app
cd service && cargo test                               # the policy unit tests
```

`am` drives the running app: `am state`, `am surface`,
`am dispatch
monitor:setMode armed`, `am expect monitor.online eq true`,
`am logs`.

## Design notes

- The UI never talks to the daemon directly — the aio **cell** polls it on the
  server and pushes the snapshot into state, so every component that reads
  `monitor.snapshot` re-renders on change, and `persist: "none"` keeps live
  measurements out of SQLite.
- The daemon's HTTP API is loopback-only and bearer-token protected; the token
  is generated on first run and shared through `runtime.json`.
- The policy is data-driven (`config.rs`), and its decision layer (`policy.rs`)
  is unit-tested: ramps, sustain windows, the victim guard, the "CPU alone never
  kills" rule, and harm ranking.
