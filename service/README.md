# protectord

The Rust daemon behind Protector. It samples `/proc` and `/proc/pressure`,
scores process trees by how much they threaten system stability, and kills a
runaway tree only when the machine is genuinely approaching a freeze. It runs
independently of the UI and survives the UI closing.

```sh
cargo build --release          # → target/release/protectord
cargo test                     # policy unit tests

./target/release/protectord --print            # one snapshot as JSON, exit
./target/release/protectord                    # run (observe mode), serve API
./target/release/protectord --armed            # run and actually kill
./target/release/protectord --home=/tmp/x --port=7899 --observe
```

## Flags

| flag                    | meaning                                                 |
| ----------------------- | ------------------------------------------------------- |
| `--mode=observe\|armed` | observe logs what it would kill; armed kills            |
| `--armed` / `--observe` | shorthand for `--mode`                                  |
| `--interval=MS`         | sampling cadence (default 1000)                         |
| `--port=N`              | loopback API port (default 7801; 0 = auto)              |
| `--home=DIR`            | state dir (default `$PROTECTOR_HOME` or `~/.protector`) |
| `--config=PATH`         | config file (default `<home>/config.json`)              |
| `--token=TOK`           | override the API bearer token                           |
| `--print`               | sample once, print the snapshot JSON, exit              |
| `--no-log`              | do not write the log file                               |

## Files (under `--home`)

- `config.json` — thresholds + mode + token (created on first run)
- `runtime.json` — `{pid,port,token,version,started}` while running
- `protector.log` — start line, policy, mode changes, kills, kernel faults,
  alerts, 30 s heartbeats

`sh install.sh` builds and installs the daemon as a systemd user service.
`PROTECTOR_KMSG=PATH` reads kernel records from PATH instead of `/dev/kmsg`
(tests).

## API (loopback, `Authorization: Bearer <token>`)

- `GET  /api/health`
- `GET  /api/snapshot` — the full state the UI renders
- `GET  /api/config`, `GET /api/info`
- `POST /api/mode` `{"mode":"armed"|"observe"}`
- `POST /api/kill` `{"pid":N,"force":bool}` — only killable trees; refused in
  observe mode unless `force`
- `POST /api/protect` `{"pid":N}` or `{"prefix":"name"}`

The aio app reads `runtime.json` for the port and token and calls this API from
its server side (`src/protectord.server.ts`).

## Safety

See the top-level README. In short: never PID 1 / kernel / desktop / other-user
processes; protects itself, its ancestry and any registered pid; two-stage
WARN→KILL; kills only with tasks stalled on machine-wide reclaim and RAM + swap
both spent, or the machine gridlocked (a level alone — swap, CPU, RAM — never);
the victim is one job (≥ 5 % of RAM), not its tree; 45 s cooldown; SIGTERM →
grace → SIGKILL; observe mode by default.

`PROTECTOR_REPLAY=~/.protector/protector.log cargo test replay -- --nocapture`
replays every logged heartbeat through the policy and fails on any trigger.
