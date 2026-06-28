import { expect, test } from "bun:test";
import { formatTime } from "../src/timeline.ts";

test("formatTime picks a compact unit per magnitude", () => {
  expect(formatTime(0.0000004)).toBe("0 ns");
  expect(formatTime(0.0005)).toBe("500 ns"); // sub-µs → ns
  expect(formatTime(0.05)).toBe("50.0 µs"); // <0.1ms → 1 decimal µs
  expect(formatTime(0.5)).toBe("500 µs"); // ≥0.1ms → whole µs
  expect(formatTime(5)).toBe("5.00 ms"); // <10ms → 2 decimals
  expect(formatTime(50)).toBe("50.0 ms"); // ≥10ms → 1 decimal
  expect(formatTime(1500)).toBe("1.50 s"); // ≥1s → seconds
});

test("formatTime handles unit boundaries", () => {
  expect(formatTime(0.001)).toBe("1.0 µs"); // exactly 1µs
  expect(formatTime(1)).toBe("1.00 ms"); // exactly 1ms
  expect(formatTime(1000)).toBe("1.00 s"); // exactly 1s
});

test("formatTime keeps the sign for negative durations", () => {
  expect(formatTime(-5)).toBe("-5.00 ms");
  expect(formatTime(-0.5)).toBe("-500 µs");
});
