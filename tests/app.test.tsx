import { assert, assertEquals } from "@std/assert";
import { testUI } from "aio/testing";
import App from "../src/App.tsx";
import { monitor } from "../src/cell.ts";
import { fakeSnapshot } from "./fixtures.ts";

testUI(App, "shows the offline banner, then the live dashboard", async (ui) => {
  // No snapshot yet: the app offers to start the daemon.
  assert(ui.html().includes("PROTECTOR"));
  assertEquals(ui.StartServiceButton.disabled, false);
  assertEquals(ui.AutoRefreshCheckbox.checked, true);

  // A snapshot lands (as the poll would deliver it) → the dashboard renders.
  await monitor.ingest(
    fakeSnapshot({
      risk: { score: 55, status: "WATCH", reasons: ["swap 30%"] },
    }),
  );
  await ui.expectCell(monitor, (s) => s.online === true);
  const html = ui.html();
  assert(html.includes("WATCH"), "status pill should show WATCH");
  assert(html.includes("hog"), "the top process should be listed");
  assert(!html.includes("Start service"), "banner should be gone when online");
});

testUI(App, "auto-refresh checkbox writes the toggle", async (ui) => {
  ui.AutoRefreshCheckbox.uncheck();
  await ui.expectCell(monitor, (s) => s.auto_refresh === false);
  ui.AutoRefreshCheckbox.check();
  await ui.expectCell(monitor, (s) => s.auto_refresh === true);
});
