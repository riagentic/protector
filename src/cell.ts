// The monitor cell — one cell drives the whole dashboard.
//
// The UI never talks to the Rust daemon itself: this cell's async methods run
// on the aio server, fetch a snapshot from protectord over loopback, and write
// it into state. Every component that reads `monitor.snapshot` re-renders when
// it changes. `persist: "none"` because every field is a live measurement — a
// snapshot restored from five minutes ago would be a lie.
import { cell, schedule, self } from "aio";
import type { MethodDraftMeta } from "aio";

export type Sev = "info" | "warn" | "danger" | "crit";

export type Signal = { severity: Sev; key: string; text: string };

export type Proc = {
  pid: number;
  name: string;
  cmd: string;
  uid: number;
  user: string;
  protected: boolean;
  killable: boolean;
  rss: number;
  rss_pct: number;
  cpu_core: number;
  cpu_machine: number;
  grow_bps: number;
  threads: number;
  d_state: number;
  oom_score: number;
  age_s: number;
  nproc: number;
  members: number;
  harm: number;
  signals: Signal[];
};

export type Event = {
  seq: number;
  t: number;
  kind: string;
  severity: "info" | "warn" | "crit";
  text: string;
  pid: number | null;
  name: string | null;
};

export type Sample = {
  t: number;
  mem_available_pct: number;
  swap_used_pct: number;
  cpu_busy_pct: number;
  risk: number;
};

export type Psi = {
  mem_some10: number;
  mem_some60: number;
  mem_full10: number;
  mem_full60: number;
  cpu_some10: number;
  cpu_some60: number;
  io_some10: number;
  io_some60: number;
  io_full10: number;
  io_full60: number;
};

export type System = {
  mem_total: number;
  mem_avail: number;
  mem_available_pct: number;
  mem_used_pct: number;
  swap_total: number;
  swap_used: number;
  swap_used_pct: number;
  cpu_busy_pct: number;
  nproc: number;
  load1: number;
  load5: number;
  load15: number;
  psi: Psi;
  d_state: number;
  procs: number;
  trees: number;
  threads: number;
  largest_tree_pct: number;
};

export type Risk = { score: number; status: string; reasons: string[] };

export type Trigger = {
  active: boolean;
  reason: string | null;
  panic: boolean;
  victim_pid: number | null;
  victim_name: string | null;
};

export type KillInfo = {
  t: number;
  pid: number;
  name: string;
  reason: string;
  dry_run: boolean;
  members: number;
  escalated: boolean;
};

export type Thresholds = {
  mem_avail_warn: number;
  mem_avail_kill: number;
  swap_warn: number;
  swap_kill: number;
  swap_panic: number;
  psi_mem_some_kill: number;
  psi_mem_full_kill: number;
  rss_tree_warn: number;
  rss_tree_kill: number;
  cpu_sys_warn: number;
  cpu_sys_kill: number;
  growth_warn_mbps: number;
  growth_danger_mbps: number;
};

/** The wire shape published by protectord. Field names are the daemon's. */
export type Snapshot = {
  t: number;
  seq: number;
  version: string;
  pid: number;
  mode: string;
  armed: boolean;
  uptime_s: number;
  interval_ms: number;
  config_path: string;
  system: System;
  risk: Risk;
  signals: Signal[];
  trigger: Trigger;
  cooldown_s: number;
  last_kill: KillInfo | null;
  events: Event[];
  history: Sample[];
  processes: Proc[];
  protected_prefixes: string[];
  thresholds: Thresholds;
};

export type State = {
  online: boolean;
  error: string | null;
  starting: boolean;
  service_binary: string | null;
  auto_refresh: boolean;
  registered: boolean;
  busy: boolean;
  last_poll_ms: number;
  snapshot: Snapshot | null;
};

/** Write a daemon snapshot into state. Pure — the unit under test. */
export function applySnapshot(s: State, snap: Snapshot): void {
  s.snapshot = snap;
  s.online = true;
  s.error = null;
  s.last_poll_ms = Date.now();
}

async function doPoll(
  s: State & MethodDraftMeta,
  force: boolean,
): Promise<void> {
  if (!force && !s.auto_refresh) return;
  const io = await import("./protectord.server.ts");
  if (s.$signal?.aborted) return;
  s.busy = true;
  try {
    if (!s.registered) {
      try {
        await io.registerSelf();
        s.registered = true;
      } catch { /* daemon may be offline; the snapshot fetch reports it */ }
    }
    const snap = (await io.fetchSnapshot()) as Snapshot;
    if (s.$signal?.aborted) return;
    applySnapshot(s, snap);
  } catch (e) {
    s.online = false;
    s.error = e instanceof Error ? e.message : String(e);
  } finally {
    if (!s.$signal?.aborted) s.busy = false;
  }
}

export const monitor = cell("monitor", {
  // Live measurements — nothing here is worth restoring from disk.
  persist: "none",
  // A poll overlapping the previous one is fine to drop; newest wins.
  concurrency: { poll: "newest", refresh: "newest" },

  state: {
    online: false,
    error: null,
    starting: false,
    service_binary: null,
    auto_refresh: true,
    registered: false,
    busy: false,
    last_poll_ms: 0,
    snapshot: null,
  } as State,

  methods: {
    /** Pure ingest — the shape tests drive. */
    ingest(s: State, snap: Snapshot) {
      applySnapshot(s, snap);
    },

    /** Scheduled poll. Respects the auto-refresh toggle. */
    async poll(s: State & MethodDraftMeta) {
      await doPoll(s, false);
    },

    /** Manual refresh, even when auto-refresh is off. */
    async refresh(s: State & MethodDraftMeta) {
      await doPoll(s, true);
    },

    setAutoRefresh(s: State, on: boolean) {
      s.auto_refresh = on;
    },

    /** Observe (dry-run) ↔ armed. Arming is a deliberate act. */
    async setMode(s: State & MethodDraftMeta, mode: string) {
      const io = await import("./protectord.server.ts");
      try {
        await io.postJson("/api/mode", { mode });
      } catch (e) {
        s.error = e instanceof Error ? e.message : String(e);
        return;
      }
      await doPoll(s, true);
    },

    /** Manually kill a process tree. Rejects with the daemon's reason. */
    async kill(s: State & MethodDraftMeta, pid: number): Promise<string> {
      const io = await import("./protectord.server.ts");
      const r = (await io.postJson("/api/kill", { pid })) as {
        message?: string;
      };
      await doPoll(s, true);
      return r?.message ?? "killed";
    },

    /** Add a pid to the daemon's protected set. */
    async protectPid(s: State & MethodDraftMeta, pid: number): Promise<string> {
      const io = await import("./protectord.server.ts");
      const r = (await io.postJson("/api/protect", { pid })) as {
        message?: string;
      };
      await doPoll(s, true);
      return r?.message ?? "protected";
    },

    /** Find and launch the daemon, then connect. */
    async startService(s: State & MethodDraftMeta) {
      s.starting = true;
      try {
        const io = await import("./protectord.server.ts");
        s.service_binary = await io.findBinary();
        await io.startService();
        s.registered = false;
        await doPoll(s, true);
      } catch (e) {
        s.online = false;
        s.error = e instanceof Error ? e.message : String(e);
      } finally {
        s.starting = false;
      }
    },

    /** Stop the daemon. The UI is only a view; the daemon can run without it. */
    async stopService(s: State & MethodDraftMeta) {
      const io = await import("./protectord.server.ts");
      await io.stopService();
      s.online = false;
      s.snapshot = null;
    },

    /** Re-arm the recurring poll (used after a restart). */
    arm(s: State & MethodDraftMeta) {
      s.$do(schedule.every("monitor:poll", 1500, self("poll")));
      s.auto_refresh = true;
    },
  },

  selectors: {
    top: (s: State, n = 12) => (s.snapshot?.processes ?? []).slice(0, n),
    killableCount: (s: State) =>
      (s.snapshot?.processes ?? []).filter((p) => p.killable).length,
    worst: (s: State) => s.snapshot?.processes[0] ?? null,
    status: (s: State) => s.snapshot?.risk.status ?? "OFFLINE",
    activeSignals: (s: State) => s.snapshot?.signals ?? [],
  },
});
