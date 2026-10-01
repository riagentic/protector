import type { JSX } from "aio";
import { useLocal } from "aio/air";
import { monitor } from "./cell.ts";
import { ago, pct, statusBand } from "./ui/format.ts";
import { EventsCard, RiskCard, SignalsCard, SystemCard } from "./ui/panels.tsx";
import { Processes } from "./ui/Processes.tsx";

function Header(): JSX.Element {
  const snap = monitor.snapshot;
  const status = snap?.risk.status ?? "OFFLINE";
  const b = statusBand(status);
  const armed = snap?.armed ?? false;
  const confirmArm = useLocal(false);

  async function arm() {
    await monitor.setMode("armed");
    confirmArm.set(false);
  }

  return (
    <header class="hdr">
      <div class="brand">
        <span class="logo">◈</span>
        <span class="brand-name">PROTECTOR</span>
        <span class="brand-sub">stability guard</span>
      </div>

      <div class="hdr-status">
        <span class={`pill ${b}`}>{status}</span>
        <span class="risk-chip">risk {snap?.risk.score ?? "–"}</span>
        {snap && snap.trigger.active
          ? <span class="pill danger">signal</span>
          : null}
      </div>

      <div class="hdr-right">
        <span
          class={`dot ${monitor.online ? "on" : "off"}`}
          title={monitor.online ? "service online" : "service offline"}
        />
        <span class="svc">{snap ? `v${snap.version}` : "offline"}</span>
        <span class="svc dim">
          {monitor.online ? `polled ${ago(monitor.last_poll_ms / 1000)}` : ""}
        </span>

        <label class="toggle" title="auto refresh">
          <input
            type="checkbox"
            checked={monitor.auto_refresh}
            onChange={() => monitor.setAutoRefresh(!monitor.auto_refresh)}
            aria-label="Auto refresh"
          />
          live
        </label>
        <button
          type="button"
          class="btn ghost"
          disabled={monitor.busy}
          onClick={() => monitor.refresh()}
          aria-label="Refresh now"
        >
          refresh
        </button>

        {armed
          ? (
            <button
              type="button"
              class="btn danger"
              onClick={() => monitor.setMode("observe")}
              aria-label="Disarm"
              title="stop killing — back to observe"
            >
              ARMED — disarm
            </button>
          )
          : confirmArm.local
          ? (
            <button
              type="button"
              class="btn danger"
              onClick={arm}
              aria-label="Confirm arm"
              title="Protector will now kill runaway trees"
            >
              confirm arm
            </button>
          )
          : (
            <button
              type="button"
              class="btn armed-off"
              onClick={() => confirmArm.set(true)}
              aria-label="Arm"
              title="let Protector kill one runaway job when the PC is about to freeze"
            >
              observe — arm
            </button>
          )}
      </div>
    </header>
  );
}

function OfflineBanner(): JSX.Element | null {
  // Hooks must run unconditionally before any early return.
  const starting = useLocal(false);
  if (monitor.online) return null;
  async function start() {
    starting.set(true);
    await monitor.startService();
    starting.set(false);
  }
  return (
    <div class="banner">
      <div class="banner-txt">
        <b>protectord is not running.</b>{" "}
        {monitor.error ?? "No runtime file found."}
        {monitor.service_binary
          ? <span class="dim">· {monitor.service_binary}</span>
          : null}
      </div>
      <button
        type="button"
        class="btn"
        disabled={starting.local || monitor.starting}
        onClick={start}
        aria-label="Start service"
      >
        {starting.local || monitor.starting ? "starting…" : "start service"}
      </button>
    </div>
  );
}

function Footer(): JSX.Element {
  const snap = monitor.snapshot;
  return (
    <footer class="ftr">
      <span>
        {snap
          ? `protectord pid ${snap.pid} · up ${
            Math.floor(snap.uptime_s)
          }s · mode ${snap.mode} · ${snap.interval_ms}ms`
          : "protectord offline"}
      </span>
      <span class="dim">
        {snap?.config_path ?? ""}
      </span>
      <span class="spacer" />
      <span class="dim">
        kill policy: only stalled with RAM &lt;{" "}
        {snap ? pct(snap.thresholds.mem_avail_kill, 0) : "5%"}{" "}
        and swap spent, or gridlocked · a full swap or busy CPU never · one job,
        not its tree
      </span>
    </footer>
  );
}

export default function App(): JSX.Element {
  return (
    <div class="app">
      <Header />
      <OfflineBanner />
      <div class="body">
        <aside class="side">
          <RiskCard />
          <SystemCard />
          <SignalsCard />
          <EventsCard />
        </aside>
        <main class="main">
          <Processes />
        </main>
      </div>
      <Footer />
    </div>
  );
}
