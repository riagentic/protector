import { assertEquals } from "@std/assert";
import { band, bytes, dur, pct, rate, statusBand } from "../src/ui/format.ts";

Deno.test("bytes formats across magnitudes", () => {
  assertEquals(bytes(512), "512 B");
  assertEquals(bytes(2048), "2 KB");
  assertEquals(bytes(5 * 1024 * 1024), "5 MB");
  assertEquals(bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
  assertEquals(bytes(0), "0");
});

Deno.test("rate formats bytes per second", () => {
  assertEquals(rate(0), "0");
  assertEquals(rate(1536), "2 KB/s");
  assertEquals(rate(5 * 1024 * 1024), "5.0 MB/s");
});

Deno.test("pct and dur are stable", () => {
  assertEquals(pct(12.345), "12.3%");
  assertEquals(pct(12.345, 0), "12%");
  assertEquals(dur(0), "0s");
  assertEquals(dur(90), "1m 30s");
  assertEquals(dur(3660), "1h 1m");
  assertEquals(dur(90000), "1d 1h");
});

Deno.test("band and statusBand map to the four colours", () => {
  assertEquals(band(0), "ok");
  assertEquals(band(45), "watch");
  assertEquals(band(75), "danger");
  assertEquals(band(95), "crit");
  assertEquals(statusBand("OK"), "ok");
  assertEquals(statusBand("WATCH"), "watch");
  assertEquals(statusBand("CRITICAL"), "danger");
  assertEquals(statusBand("PANIC"), "crit");
  assertEquals(statusBand("OFFLINE"), "off");
});
