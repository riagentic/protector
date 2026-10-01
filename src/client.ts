// Thin CLI client — live view of a running server's state over WebSocket.
// Run it against a dev server (`deno run -A src/client.ts`); the
// `cli-client` build target compiles it into a standalone client binary
// (add "cli-client" to build.targets, then `deno task build`). No local server.
import { connectCli } from "aio/server";

// No default URL: dev picks a FREE port, so a hard-coded one connects to
// nothing — or to a different app. The port is on the dev boot line
// (`open http://localhost:<port>`) and in `am instances`.
const url = Deno.args[0];
if (!url) {
  console.error(
    "usage: client <ws://host:port/ws>\n" +
      "  the port is on the dev server's boot line, or: deno task am instances",
  );
  Deno.exit(2);
}
console.log(`connecting to ${url} ...`);
// Bounded: a dead URL fails with a message instead of hanging forever.
const app = connectCli(url, { readyTimeoutMs: 10_000 });
await app.ready;
console.log("state:", JSON.stringify(app.state, null, 2));
app.subscribe(() => console.log("state:", JSON.stringify(app.state)));
