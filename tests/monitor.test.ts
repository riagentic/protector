import { assertEquals } from "@std/assert";
import { testCell } from "aio/testing";
import { monitor } from "../src/cell.ts";
import { fakeSnapshot, proc } from "./fixtures.ts";

testCell(monitor, "starts offline and ingests a snapshot", (t) => {
  t.expect.state((s) => s.online === false && s.snapshot === null);
  t.send.ingest(fakeSnapshot());
  t.expect.state((s) => s.online === true && s.error === null);
  assertEquals(monitor.status(), "OK");
  assertEquals(monitor.top(2).length, 2);
});

testCell(monitor, "selectors read the ingested state", (t) => {
  t.send.ingest(
    fakeSnapshot({
      risk: { score: 55, status: "WATCH", reasons: ["swap 30%"] },
      processes: [
        proc({ pid: 1, name: "hog", harm: 80 }),
        proc({ pid: 2, name: "small", harm: 5 }),
        proc({ pid: 3, name: "systemd", protected: true, killable: false }),
      ],
    }),
  );
  assertEquals(monitor.status(), "WATCH");
  assertEquals(monitor.killableCount(), 2);
  assertEquals(monitor.worst()?.name, "hog");
  assertEquals(monitor.top(1)[0]?.pid, 1);
});

testCell(monitor, "auto-refresh toggle is state", (t) => {
  t.send.setAutoRefresh(false);
  t.expect.state((s) => s.auto_refresh === false);
  t.send.setAutoRefresh(true);
  t.expect.state((s) => s.auto_refresh === true);
});
