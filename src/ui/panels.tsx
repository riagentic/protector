import type { JSX } from "aio";
import type { VChild } from "aio/air";
import { monitor } from "../cell.ts";
import { bytes, clock, pct, statusBand } from "./format.ts";
import { Spark } from "./Spark.tsx";

function Panel(props: {
  title: string;
  right?: VChild;
  class?: string;
  children: VChild;
}): JSX.Element {
  return (
    <section class={`card ${props.class ?? ""}`}>
      <header class="card-hd">
        <span class="card-title">{props.title}</span>
        {props.right ? <span class="card-right">{props.right}</span> : null}
      </header>
      <div class="card-bd">{props.children}</div>
    </section>
  );
}

function Meter(props: {
  label: string;
  value: number;
  max: number;
  band?: string;
  text?: string;
  sub?: string;
}): JSX.Element {
  const p = Math.max(0, Math.min(100, (props.value / props.max) * 100));
  return (
    <div class="meter">
      <div class="meter-top">
        <span class="meter-label">{props.label}</span>
        <span class={`meter-val ${props.band ?? "ok"}`}>
          {props.text ?? ""}
        </span>
      </div>
      <div class="bar">
        <div
          class={`bar-fill ${props.band ?? "ok"}`}
          style={{ width: `${p}%` }}
        />
      </div>
      {props.sub ? <div class="meter-sub">{props.sub}</div> : null}
    </div>
  );
}

function availBand(avail: number, warn: number, kill: number): string {
  if (avail < kill) return "crit";
  if (avail < warn) return "watch";
  return "ok";
}

export function SystemCard(): JSX.Element {
  const snap = monitor.snapshot;
  const sys = snap?.system;
  if (!snap || !sys) {
    return (
      <Panel title="System">
        <div class="empty">waiting for the service…</div>
      </Panel>
    );
  }
  const th = snap.thresholds;
  const memBand = availBand(
    sys.mem_available_pct,
    th.mem_avail_warn,
    th.mem_avail_kill,
  );
  // A full swap is only a problem while RAM is scarce too.
  const scarce = sys.mem_available_pct < th.mem_avail_warn;
  const swapBand = scarce && sys.swap_used_pct > th.swap_panic
    ? "crit"
    : scarce && sys.swap_used_pct > th.swap_kill
    ? "watch"
    : "ok";
  const cpuBand = sys.cpu_busy_pct > th.cpu_sys_warn ? "watch" : "ok";
  const loadBand = sys.load1 > sys.nproc ? "watch" : "ok";
  const dBand = sys.d_state >= 10 ? "crit" : sys.d_state >= 3 ? "watch" : "ok";
  const hist = snap.history;

  return (
    <Panel
      title="System"
      right={`${sys.procs} procs · ${sys.trees} trees`}
    >
      <Meter
        label="RAM available"
        value={sys.mem_used_pct}
        max={100}
        band={memBand}
        text={pct(sys.mem_available_pct)}
        sub={`${bytes(sys.mem_avail)} free of ${bytes(sys.mem_total)}`}
      />
      <Meter
        label="Swap"
        value={sys.swap_used_pct}
        max={100}
        band={swapBand}
        text={pct(sys.swap_used_pct)}
        sub={`${bytes(sys.swap_used)} of ${bytes(sys.swap_total)}`}
      />
      <Meter
        label="CPU busy"
        value={sys.cpu_busy_pct}
        max={100}
        band={cpuBand}
        text={pct(sys.cpu_busy_pct, 0)}
        sub={`${sys.nproc} cores · load ${sys.load1.toFixed(1)} / ${
          sys.load5.toFixed(1)
        } / ${sys.load15.toFixed(1)}`}
      />
      <div class="mini-grid">
        <div class="mini">
          <span class="mini-k">D-state</span>
          <span class={`mini-v ${dBand}`}>{sys.d_state}</span>
        </div>
        <div class="mini">
          <span class="mini-k">Largest tree</span>
          <span class="mini-v">{pct(sys.largest_tree_pct)}</span>
        </div>
        <div class="mini">
          <span class="mini-k">Threads</span>
          <span class="mini-v">{sys.threads}</span>
        </div>
        <div class="mini">
          <span class="mini-k">Load/core</span>
          <span class={`mini-v ${loadBand}`}>
            {(sys.load1 / sys.nproc).toFixed(2)}
          </span>
        </div>
      </div>
      <div class="psi">
        <div class="psi-hd">Pressure (PSI)</div>
        <PsiRow
          label="memory some60"
          v={sys.psi.mem_some60}
          warn={10}
          kill={40}
        />
        <PsiRow
          label="memory full10"
          v={sys.psi.mem_full10}
          warn={5}
          kill={15}
        />
        <PsiRow label="cpu some60" v={sys.psi.cpu_some60} warn={25} kill={60} />
        <PsiRow label="io full10" v={sys.psi.io_full10} warn={5} kill={20} />
      </div>
      <div class="spark-row">
        <div class="spark-cell">
          <span class="spark-k">RAM free</span>
          <Spark
            values={hist.map((h) => h.mem_available_pct)}
            class="ok"
            max={100}
            height={22}
          />
        </div>
        <div class="spark-cell">
          <span class="spark-k">CPU</span>
          <Spark
            values={hist.map((h) => h.cpu_busy_pct)}
            class="watch"
            max={100}
            height={22}
          />
        </div>
        <div class="spark-cell">
          <span class="spark-k">Swap</span>
          <Spark
            values={hist.map((h) => h.swap_used_pct)}
            class="danger"
            max={100}
            height={22}
          />
        </div>
      </div>
    </Panel>
  );
}

function PsiRow(props: {
  label: string;
  v: number;
  warn: number;
  kill: number;
}): JSX.Element {
  const b = props.v >= props.kill
    ? "crit"
    : props.v >= props.warn
    ? "watch"
    : "ok";
  return (
    <div class="psi-row">
      <span class="psi-k">{props.label}</span>
      <div class="bar thin">
        <div
          class={`bar-fill ${b}`}
          style={{
            width: `${Math.min(100, (props.v / (props.kill * 1.2)) * 100)}%`,
          }}
        />
      </div>
      <span class={`psi-v ${b}`}>{props.v.toFixed(0)}</span>
    </div>
  );
}

export function RiskCard(): JSX.Element {
  const snap = monitor.snapshot;
  const risk = snap?.risk;
  const score = risk?.score ?? 0;
  const status = risk?.status ?? "OFFLINE";
  const b = statusBand(status);
  return (
    <Panel title="Freeze risk" right={status}>
      <div class="risk">
        <div class={`risk-score ${b}`}>{score}</div>
        <div class="risk-meter">
          <div class="bar big">
            <div class={`bar-fill ${b}`} style={{ width: `${score}%` }} />
          </div>
          <div class="risk-note">
            {status === "OK"
              ? "System responsive — nothing to do."
              : status === "OFFLINE"
              ? "Service not running."
              : "Approaching the kill threshold."}
          </div>
        </div>
      </div>
      {risk && risk.reasons.length > 0
        ? (
          <ul class="reasons">
            {risk.reasons.map((r) => <li key={r}>{r}</li>)}
          </ul>
        )
        : null}
      {snap && snap.cooldown_s > 0
        ? <div class="cooldown">cooldown · {snap.cooldown_s.toFixed(0)}s</div>
        : null}
      {snap && snap.trigger.active
        ? (
          <div class={`trigger ${snap.trigger.panic ? "crit" : "danger"}`}>
            <b>{snap.trigger.panic ? "PANIC" : "TRIGGER"}</b>{" "}
            {snap.trigger.reason}
            {snap.trigger.victim_pid
              ? ` → ${snap.trigger.victim_name} (${snap.trigger.victim_pid})`
              : " → holding (no tree large enough)"}
          </div>
        )
        : null}
    </Panel>
  );
}

export function SignalsCard(): JSX.Element {
  const signals = monitor.snapshot?.signals ?? [];
  return (
    <Panel
      title="Signals"
      right={signals.length ? `${signals.length}` : "clear"}
    >
      {signals.length === 0
        ? <div class="empty">no active negative signals</div>
        : (
          <div class="chips">
            {signals.map((s, i) => (
              <span
                key={`${s.key}-${i}`}
                class={`chip ${s.severity}`}
                title={s.key}
              >
                {s.text}
              </span>
            ))}
          </div>
        )}
    </Panel>
  );
}

export function EventsCard(): JSX.Element {
  const events = (monitor.snapshot?.events ?? []).slice(-40).reverse();
  return (
    <Panel title="Activity" right={`${events.length}`}>
      {events.length === 0
        ? <div class="empty">nothing yet</div>
        : (
          <ul class="feed">
            {events.map((e) => (
              <li key={e.seq} class={`feed-item ${e.severity}`}>
                <span class="feed-t">{clock(e.t)}</span>
                <span class={`feed-dot ${e.severity}`} />
                <span class="feed-x">{e.text}</span>
              </li>
            ))}
          </ul>
        )}
    </Panel>
  );
}
