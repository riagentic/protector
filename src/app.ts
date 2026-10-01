import "./cell.ts";
import { monitor } from "./cell.ts";
import { aio } from "aio";

// The daemon is the protector; this app is its window. A static schedule keeps
// the dashboard live (re-registered on every boot by the framework), and
// `onStart` gets a first snapshot on screen immediately.
await aio.run({
  ui: {
    theme: "none", // src/style.css owns the entire look
    title: "Protector",
    width: 1120,
    height: 780,
    chrome: "standard",
  },
  onStart: () => {
    monitor.poll();
  },
  schedules: [
    { id: "monitor:poll", every: 1500, action: monitor.poll.action() },
  ],
});
