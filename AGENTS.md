# Working on protector with an AI agent

This app is built on [aio](https://github.com/riagentic/aio) — one
`cell({ state, methods })` drives server state, persistence, sync and the UI. It
is not React, Express or Next; guessing from those produces code that
type-checks and is wrong.

## Read this first

    am agent

One command, one page (Markdown): the model, the full API, every `am` verb, how
to add state/UI/tests, debug and ship. `am agent --min` when your context is
small, `am agent --max` when it is large; `am agent --task=<slug>` for one
section (`--list` for all; `--task=new` is the build flow).

## Rules

- **Never end an app by process match.** `pkill -f app.ts` matches EVERY aio app
  on the machine. Use `am stop` (`am stop --all` for this project);
  `am instances` shows what is running.
- **Never take over the screen.** Use `am start --client=server-only` and read
  the UI with `am surface` — it needs no window.
- **Never script around `am`.** Python, jq or curl against this app is a worse
  copy of a verb. Every command takes `--json`.
- **Learn before editing.** `am agent` first — aio is not React/Next.
- **check → run → test.** Types first, running app second, tests last. Rewrite
  or delete the scaffold `tests/cell.test.ts` when you replace the cell.
- **`type` alias for cell state, never `interface`.**
- **Leave it running when the human asked to see it** — report the
  `am instances` line instead of `am stop`.

## The loop

Order that works: **check → run → test** (do not write tests before the app
runs).

    deno task check && deno task lint
    am start --client=server-only  # daemonised; deno task dev dies with your shell
    am surface --json              # what is on screen, by NAME
    am dispatch <cell:method> args # drive the state machine
    am expect <path> eq <value>    # assert — do not pipe state to a parser
    am timeline --lines=20         # what happened, with state diffs
    deno task test                 # last — after it runs
