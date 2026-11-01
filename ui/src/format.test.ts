import { describe, expect, it } from "vitest";
import { bytes, compact, duration, ms, pct, usd } from "./format";

describe("number formats", () => {
  it("compacts large numbers", () => {
    expect(compact(950)).toBe("950");
    expect(compact(1_250)).toBe("1.25K");
    expect(compact(12_500)).toBe("12.5K");
    expect(compact(3_000_000)).toBe("3M");
    expect(compact(null)).toBe("–");
  });

  it("shows money at the precision it needs", () => {
    expect(usd(0)).toBe("$0");
    expect(usd(0.0042)).toBe("$0.0042");
    expect(usd(12.5)).toBe("$12.50");
    expect(usd(2_500)).toBe("$2.5K");
  });

  it("shows durations, sizes and shares", () => {
    expect(ms(850)).toBe("850ms");
    expect(ms(1_500)).toBe("1.5s");
    expect(ms(42_000)).toBe("42s");
    expect(bytes(820)).toBe("820 B");
    expect(bytes(14 * 1024)).toBe("14 KB");
    expect(pct(0.0005)).toBe("<0.1%");
    expect(pct(0.05)).toBe("5.0%");
    expect(pct(0.5)).toBe("50%");
    expect(duration(90_061)).toBe("1d 1h");
    expect(duration(3_720)).toBe("1h 2m");
  });
});
