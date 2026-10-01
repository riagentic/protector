// Server-only bridge to the Rust daemon (`service/`). This module runs on the
// aio server — never in the browser — so it is free to touch the filesystem,
// Deno APIs and the daemon's loopback HTTP API. The cell imports it dynamically
// (`await import("./protectord.server.ts")`), which is what keeps it out of the
// client bundle (docs/build/imports.md).
//
// It does three things:
//   1. find the running daemon via ~/.protector/runtime.json,
//   2. talk to it (snapshot / mode / kill / protect),
//   3. start and stop it for the user.

export type Runtime = {
  pid: number;
  port: number;
  token: string;
  version: string;
  mode: string;
  started: number;
  config_path: string;
};

export function homeDir(): string {
  const h = Deno.env.get("PROTECTOR_HOME");
  if (h && h.length > 0) return h;
  const home = Deno.env.get("HOME") ?? Deno.env.get("USERPROFILE") ?? ".";
  return `${home}/.protector`;
}

function runtimePath(): string {
  return `${homeDir()}/runtime.json`;
}

/** The daemon's runtime file, or null when it is not running (or stale). */
export async function readRuntime(): Promise<Runtime | null> {
  let text: string;
  try {
    text = await Deno.readTextFile(runtimePath());
  } catch {
    return null;
  }
  let rt: Runtime;
  try {
    rt = JSON.parse(text) as Runtime;
  } catch {
    return null;
  }
  if (!rt || !rt.port || !rt.token || !rt.pid) return null;
  // A leftover file from a killed daemon is not a running daemon — and its pid
  // may belong to something else by now, which "stop" must never signal.
  try {
    const comm = await Deno.readTextFile(`/proc/${rt.pid}/comm`);
    if (comm.trim() !== "protectord") return null;
  } catch {
    return null;
  }
  return rt;
}

async function call<T>(
  rt: Runtime,
  path: string,
  init?: { method: string; body?: unknown },
): Promise<T> {
  const res = await fetch(`http://127.0.0.1:${rt.port}${path}`, {
    method: init?.method ?? "GET",
    headers: {
      Authorization: `Bearer ${rt.token}`,
      ...(init?.body ? { "Content-Type": "application/json" } : {}),
    },
    body: init?.body ? JSON.stringify(init.body) : undefined,
    signal: AbortSignal.timeout(4000),
  });
  const text = await res.text();
  if (!res.ok) {
    let msg = `service returned ${res.status}`;
    try {
      const j = JSON.parse(text);
      if (j && typeof j.error === "string") msg = j.error;
    } catch { /* non-JSON error body */ }
    throw new Error(msg);
  }
  return (text ? JSON.parse(text) : null) as T;
}

/** Fetch the full snapshot. Throws a readable error when the daemon is down. */
export async function fetchSnapshot(): Promise<unknown> {
  const rt = await readRuntime();
  if (!rt) throw new Error("service offline");
  return await call(rt, "/api/snapshot");
}

export async function postJson(path: string, body: unknown): Promise<unknown> {
  const rt = await readRuntime();
  if (!rt) throw new Error("service offline");
  return await call(rt, path, { method: "POST", body });
}

/** Ask the daemon to never signal this aio server (and its ancestry). */
export async function registerSelf(): Promise<void> {
  await postJson("/api/protect", { pid: Deno.pid });
}

// ── locating and running the daemon ──────────────────────────────────

const BIN_NAME = "protectord";

export async function findBinary(): Promise<string | null> {
  const candidates: string[] = [];
  const env = Deno.env.get("PROTECTOR_BIN");
  if (env) candidates.push(env);
  const cwd = Deno.cwd();
  candidates.push(`${cwd}/service/target/release/${BIN_NAME}`);
  candidates.push(`${cwd}/service/target/debug/${BIN_NAME}`);
  candidates.push(`${homeDir()}/../.local/bin/${BIN_NAME}`);
  for (const c of candidates) {
    try {
      const st = await Deno.stat(c);
      if (st.isFile) return c;
    } catch { /* keep looking */ }
  }
  // On PATH?
  try {
    const out = await new Deno.Command("sh", {
      args: ["-c", `command -v ${BIN_NAME}`],
      stdout: "piped",
      stderr: "null",
    }).output();
    const p = new TextDecoder().decode(out.stdout).trim();
    if (p) return p;
  } catch { /* no shell / not found */ }
  return null;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/** Spawn the daemon in its own session so it outlives this app. */
export async function startService(): Promise<Runtime> {
  const existing = await readRuntime();
  if (existing) return existing;
  const bin = await findBinary();
  if (!bin) {
    throw new Error(
      `protectord not found — build it:  cd service && cargo build --release`,
    );
  }
  const home = homeDir();
  const spawn = (cmd: string, args: string[]) =>
    new Deno.Command(cmd, {
      args,
      stdin: "null",
      stdout: "null",
      stderr: "null",
    }).spawn();

  // `setsid -f` double-forks, so the daemon is reparented to init and is not
  // torn down with this app's process tree.
  try {
    spawn("setsid", ["-f", bin, "--home", home]);
  } catch {
    spawn(bin, ["--home", home]);
  }

  for (let i = 0; i < 50; i++) {
    await sleep(120);
    const rt = await readRuntime();
    if (rt) return rt;
  }
  throw new Error("service did not come up — check ~/.protector/protector.log");
}

/** Stop the daemon (SIGTERM). */
export async function stopService(): Promise<boolean> {
  const rt = await readRuntime();
  if (!rt) return false;
  try {
    Deno.kill(rt.pid, "SIGTERM");
  } catch {
    try {
      await new Deno.Command("kill", { args: ["-TERM", String(rt.pid)] })
        .output();
    } catch {
      return false;
    }
  }
  return true;
}
