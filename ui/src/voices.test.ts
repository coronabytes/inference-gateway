import { describe, expect, it } from "bun:test";
import { duplicateVoices, savedVoices, voiceRows } from "./components/VoiceEditor";
import { presetVoices } from "./voices";

describe("voices", () => {
  it("saves the listed voices, then * for others, and none for any voice", () => {
    const rows = [
      { id: " voice_478 ", name: "Sheila (English, US)" },
      { id: "alloy", name: " " },
      { id: "", name: "half typed" },
    ];
    expect(savedVoices(rows, false)).toEqual([{ id: "voice_478", name: "Sheila (English, US)" }, { id: "alloy" }]);
    expect(savedVoices(rows, true).at(-1)).toEqual({ id: "*" });
    // * alone would mean any voice anyway, which the gateway asks to say with no list.
    expect(savedVoices([], true)).toEqual([]);
    expect(voiceRows(savedVoices(rows, true))).toEqual({
      rows: [
        { id: "voice_478", name: "Sheila (English, US)" },
        { id: "alloy", name: "" },
      ],
      others: true,
    });
    expect([
      ...duplicateVoices([
        { id: "a", name: "" },
        { id: "a ", name: "" },
        { id: "b", name: "" },
      ]),
    ]).toEqual(["a"]);
  });

  it("has presets for the providers with known voices", () => {
    expect(presetVoices("mistral")[0]).toEqual({ id: "voice_478", name: "Sheila (English, US)" });
    expect(presetVoices("azure").map((v) => v.id)).toContain("alloy");
    expect(presetVoices("bedrock")).toEqual([]);
  });
});
