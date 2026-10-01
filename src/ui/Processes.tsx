import type { JSX } from "aio";
import { useLocal } from "aio/air";
import { monitor, type Proc } from "../cell.ts";
import { band, bytes, dur, pct, rate } from "./format.ts";

type SortKey = "harm" | "rss" | "cpu" | "grow" | "name";

function sortProcs(list: Proc[], key: SortKey): Proc[] {
  const out = [...list];
  switch (key) {
    case "rss":
      return out.sort((a, b) => b.rss - a.rss);
    case "cpu":
      return out.sort((a, b) => b.cpu_core - a.cpu_core);
    case "grow":
      return out.sort((a, b) => b.grow_bps - a.grow_bps);
    case "name":
      return out.sort((a, b) => a.name.localeCompare(b.name));
    default:
      return out.sort((a, b) => b.harm - a.harm || b.rss - a.rss);
  }
}

function HarmBar(props: { harm: number }): JSX.Element {
  const b = band(props.harm);
  return (
    <div class="harm">
      <div class={`harm-n ${b}`}>{props.harm}</div>
      <div class="bar thin">
        <div class={`bar-fill ${b}`} style={{ width: `${props.harm}%` }} />
      </div>
    </div>
  );
}

function ProcRow(props: { p: Proc }): JSX.Element {
  const p = props.p;
  const ui = useLocal({ open: false, confirm: false, busy: false, note: "" });

  async function doKill() {
    ui.patch({ busy: true });
    try {
      const msg = await monitor.kill(p.pid);
      ui.patch({ note: msg });
    } catch (e) {
      ui.patch({ note: e instanceof Error ? e.message : String(e) });
    } finally {
      ui.patch({ busy: false, confirm: false });
    }
  }

  return (
    <>
      <div
        class={`proc-row ${p.protected ? "prot" : ""} ${
          ui.local.open ? "open" : ""
        }`}
        onClick={() => ui.patch({ open: !ui.local.open })}
        onKeyDown={(e: KeyboardEvent) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            ui.patch({ open: !ui.local.open });
          }
        }}
        role="button"
        tabIndex={0}
        aria-expanded={ui.local.open}
        title={p.cmd}
      >
        <HarmBar harm={p.harm} />
        <div class="proc-name">
          <span class="pname">{p.name}</span>
          <span class="pmeta">
            #{p.pid} · {p.user}
            {p.members > 1 ? ` · ${p.members} procs` : ""}
          </span>
        </div>
        <div class="num">
          {bytes(p.rss)}
          <span class="sub">{pct(p.rss_pct)}</span>
        </div>
        <div class="num">
          {p.cpu_core.toFixed(0)}%
          <span class="sub">{(p.cpu_core / 100).toFixed(1)}c</span>
        </div>
        <div class="num">{rate(p.grow_bps)}</div>
        <div class="sigcell">
          {p.signals.slice(0, 2).map((s, i) => (
            <span key={i} class={`chip sm ${s.severity}`}>{s.text}</span>
          ))}
          {p.signals.length > 2
            ? <span class="chip sm info">+{p.signals.length - 2}</span>
            : null}
        </div>
        <div class="act" onClick={(e: Event) => e.stopPropagation()}>
          {!p.killable
            ? (
              <span
                class="lock"
                title={p.protected
                  ? "protected — never signalled"
                  : "other user — never signalled"}
              >
                🔒
              </span>
            )
            : ui.local.confirm
            ? (
              <button
                type="button"
                class="kill danger"
                disabled={ui.local.busy}
                onClick={() => doKill()}
              >
                {ui.local.busy ? "…" : "confirm"}
              </button>
            )
            : (
              <button
                type="button"
                class="kill"
                onClick={() => ui.patch({ confirm: true })}
                title="kill this process tree"
              >
                kill
              </button>
            )}
        </div>
      </div>
      {ui.local.open
        ? (
          <div class="proc-detail">
            <div>
              <span class="dk">cmd</span> <code>{p.cmd}</code>
            </div>
            <div class="dgrid">
              <span>
                <span class="dk">uid</span> {p.uid}
              </span>
              <span>
                <span class="dk">threads</span> {p.threads}
              </span>
              <span>
                <span class="dk">D-state</span> {p.d_state}
              </span>
              <span>
                <span class="dk">oom_score</span> {p.oom_score}
              </span>
              <span>
                <span class="dk">age</span> {dur(p.age_s)}
              </span>
              <span>
                <span class="dk">cpu of machine</span> {pct(p.cpu_machine, 2)}
              </span>
            </div>
            {ui.local.note ? <div class="dnote">{ui.local.note}</div> : null}
          </div>
        )
        : null}
    </>
  );
}

export function Processes(): JSX.Element {
  const snap = monitor.snapshot;
  const q = useLocal("");
  const killableOnly = useLocal(false);
  const sort = useLocal<SortKey>("harm");

  const all = snap?.processes ?? [];
  const query = q.local.trim().toLowerCase();
  let list = all.filter((p) => {
    if (killableOnly.local && !p.killable) return false;
    if (!query) return true;
    return p.name.toLowerCase().includes(query) ||
      p.cmd.toLowerCase().includes(query) ||
      String(p.pid) === query;
  });
  list = sortProcs(list, sort.local);

  return (
    <section class="card grow">
      <header class="card-hd">
        <span class="card-title">Processes</span>
        <span class="card-right">
          {list.length}/{all.length} · {monitor.killableCount()} killable
        </span>
      </header>
      <div class="toolbar">
        <input
          class="filter"
          type="search"
          placeholder="filter name, cmd or pid…"
          value={q.local}
          onInput={(e: Event) => q.set((e.target as HTMLInputElement).value)}
          aria-label="Filter processes"
        />
        <div class="sorts">
          {(["harm", "rss", "cpu", "grow", "name"] as SortKey[]).map((k) => (
            <button
              key={k}
              type="button"
              class={`sortbtn ${sort.local === k ? "on" : ""}`}
              onClick={() => sort.set(k)}
            >
              {k}
            </button>
          ))}
        </div>
        <label class="toggle">
          <input
            type="checkbox"
            checked={killableOnly.local}
            onChange={() => killableOnly.set(!killableOnly.local)}
            aria-label="Killable only"
          />
          killable only
        </label>
      </div>
      <div class="proc-head">
        <span>harm</span>
        <span>process</span>
        <span class="r">RSS</span>
        <span class="r">CPU</span>
        <span class="r">growth</span>
        <span>signals</span>
        <span class="r">action</span>
      </div>
      <div class="proc-body">
        {list.length === 0
          ? <div class="empty">no matching processes</div>
          : list.map((p) => <ProcRow key={p.pid} p={p} />)}
      </div>
    </section>
  );
}
