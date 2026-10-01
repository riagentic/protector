// Small formatting helpers for the dashboard. Pure, no framework.

export function bytes(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0";
  const kb = 1024;
  if (n >= kb * kb * kb) return `${(n / (kb * kb * kb)).toFixed(2)} GB`;
  if (n >= kb * kb) return `${(n / (kb * kb)).toFixed(0)} MB`;
  if (n >= kb) return `${(n / kb).toFixed(0)} KB`;
  return `${n} B`;
}

export function pct(n: number, digits = 1): string {
  if (!Number.isFinite(n)) return "–";
  return `${n.toFixed(digits)}%`;
}

/** bytes/second → human rate. */
export function rate(bps: number): string {
  if (!Number.isFinite(bps) || bps <= 0) return "0";
  const kb = 1024;
  if (bps >= kb * kb) return `${(bps / (kb * kb)).toFixed(1)} MB/s`;
  if (bps >= kb) return `${(bps / kb).toFixed(0)} KB/s`;
  return `${bps.toFixed(0)} B/s`;
}

export function dur(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "–";
  const s = Math.floor(seconds);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${sec}s`;
  return `${sec}s`;
}

export function clock(t: number): string {
  if (!t) return "–";
  const d = new Date(t * 1000);
  return d.toLocaleTimeString([], { hour12: false });
}

export function ago(t: number): string {
  if (!t) return "–";
  const s = Date.now() / 1000 - t;
  if (s < 2) return "now";
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  return `${Math.floor(s / 3600)}h ago`;
}

/** Colour band for a 0–100 risk/harm number. */
export function band(v: number): "ok" | "watch" | "danger" | "crit" {
  if (v >= 90) return "crit";
  if (v >= 70) return "danger";
  if (v >= 40) return "watch";
  return "ok";
}

export function statusBand(
  status: string,
): "ok" | "watch" | "danger" | "crit" | "off" {
  switch (status) {
    case "PANIC":
      return "crit";
    case "CRITICAL":
      return "danger";
    case "WATCH":
      return "watch";
    case "OFFLINE":
      return "off";
    default:
      return "ok";
  }
}
