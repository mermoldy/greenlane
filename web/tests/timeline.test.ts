import { expect, test } from "bun:test";
import { formatTime } from "../src/timeline.ts";

test("formatTime uses compact units", () => {
  expect(formatTime(0.0000004)).toBe("0 ns");
  expect(formatTime(0.05)).toBe("50.0 µs");
  expect(formatTime(5)).toBe("5.00 ms");
  expect(formatTime(1500)).toBe("1.50 s");
});
