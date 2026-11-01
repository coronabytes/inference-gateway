import { describe, expect, it } from "vitest";
import { MAX_LABELS, labelError, labelHeaders, labelQuery, withLabel } from "./labels";

describe("request labels", () => {
  it("writes a filter as the query and header the gateway reads", () => {
    expect(labelQuery({ tenant: "acme", feature: "chat" })).toBe("tenant=acme,feature=chat");
    expect(labelHeaders({ tenant: "acme" })).toEqual({ "x-gateway-labels": "tenant=acme" });
    expect(labelHeaders({})).toEqual({});
  });

  it("keeps a filter within the labels a request can carry", () => {
    const full = Object.fromEntries(Array.from({ length: MAX_LABELS }, (_, i) => [`k${i}`, "v"]));
    expect(withLabel(full, "another", "v")).toBe(full);
    expect(withLabel(full, "k0", "changed").k0).toBe("changed");
  });

  it("refuses what the gateway refuses", () => {
    expect(labelError("tenant", "acme")).toBeNull();
    expect(labelError("bad key", "v")).not.toBeNull();
    expect(labelError("k", "a,b")).not.toBeNull();
    expect(labelError("k", "ünicode")).not.toBeNull();
  });
});
