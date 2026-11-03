import { describe, expect, it } from "bun:test";
import { deploymentPrices, perMillion, priceDifferences, suggestModelName, upstreamFromKey } from "./pricing";

describe("catalog prices", () => {
  const entry = {
    input_cost_per_token: 3e-6,
    output_cost_per_token: 15e-6,
    cache_read_input_token_cost: 0.3e-6,
    cache_creation_input_token_cost: 3e-6,
    litellm_provider: "anthropic",
    mode: "chat",
  };

  it("converts per-token prices to USD per 1M, leaving cache prices equal to input empty", () => {
    const p = deploymentPrices(entry, "chat");
    expect(p.input_cost_per_mtok).toBe(3);
    expect(p.output_cost_per_mtok).toBe(15);
    expect(p.cache_read_cost_per_mtok).toBe(0.3);
    expect(p.cache_write_cost_per_mtok).toBeNull();
    expect(deploymentPrices(entry, "embedding").output_cost_per_mtok).toBe(0);
  });

  it("prices audio models per minute and per character, as the gateway does", () => {
    const whisper = deploymentPrices({ input_cost_per_second: 0.0001, output_cost_per_second: 0.0001 }, "transcription");
    expect([whisper.cost_per_audio_minute, whisper.input_cost_per_mtok, whisper.output_cost_per_mtok]).toEqual([0.006, 0, 0]);
    const gpt = deploymentPrices(
      { input_cost_per_audio_token: 6e-6, input_cost_per_token: 2.5e-6, output_cost_per_token: 1e-5 },
      "transcription",
    );
    expect([gpt.input_cost_per_mtok, gpt.output_cost_per_mtok, gpt.cost_per_audio_minute]).toEqual([6, 10, null]);
    expect(deploymentPrices({ input_cost_per_character: 1.5e-5 }, "speech").cost_per_m_characters).toBe(15);
    const mini = deploymentPrices({ input_cost_per_token: 6e-7, output_cost_per_token: 1e-5, output_cost_per_second: 0.00025 }, "speech");
    expect([mini.cost_per_m_characters, mini.input_cost_per_mtok, mini.output_cost_per_mtok]).toEqual([16.666667, 0, 0]);
  });

  it("compares prices as billed", () => {
    const p = deploymentPrices(entry, "chat");
    expect(priceDifferences(p, { ...p, cache_write_cost_per_mtok: 3 })).toEqual([]);
    expect(priceDifferences(p, { ...p, output_cost_per_mtok: 16 })).toEqual(["output_cost_per_mtok"]);
  });

  it("shows prices per 1M", () => {
    expect(perMillion(2.5)).toBe("$2.50");
    expect(perMillion(3)).toBe("$3");
    expect(perMillion(0.0425)).toBe("$0.0425");
    expect(perMillion(null)).toBe("–");
  });
});

describe("model names", () => {
  it("suggests a client-facing name for an upstream model", () => {
    expect(suggestModelName("us.anthropic.claude-sonnet-4-5-20250929-v1:0")).toBe("claude-sonnet-4-5");
    expect(suggestModelName("amazon.titan-embed-text-v2:0")).toBe("titan-embed-text-v2");
    expect(suggestModelName("claude-opus-4@20250514")).toBe("claude-opus-4");
  });

  it("strips catalog prefixes from upstream names", () => {
    expect(upstreamFromKey("anthropic/claude-x", { litellm_provider: "anthropic" }, "anthropic")).toBe("claude-x");
    expect(upstreamFromKey("azure/eu/gpt-4o", { litellm_provider: "azure" }, "azure")).toBe("gpt-4o");
  });
});
