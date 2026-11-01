import { describe, expect, it } from "vitest";
import { applyEvent, buildBody, indented, parseFull, type Api, type ChatOptions, type Result, type Turn } from "./protocol";

const PLAIN: ChatOptions = { effort: "", webSearch: false, mcp: [], schema: null };
const HISTORY: Turn[] = [
  { role: "user", text: "Hi" },
  { role: "assistant", text: "Hello" },
  { role: "user", text: "Why is the sky blue?" },
];
const PNG = { name: "a.png", type: "image/png", size: 3, data: "AAA" };
const PDF = { name: "b.pdf", type: "application/pdf", size: 3, data: "BBB" };

/** A request body, read field by field whatever its API. */
const build = (...args: Parameters<typeof buildBody>): Record<string, any> => buildBody(...args);

/** The result of a stream of JSON events, as the playground reads it. */
function stream(apiId: Api, events: (object | string)[]): Result {
  const r: Result = { text: "", sources: [], mcp: [] };
  for (const e of events) applyEvent(apiId, { data: typeof e === "string" ? e : JSON.stringify(e) }, r);
  return r;
}

describe("request bodies", () => {
  it("sends the system prompt in each API's own field", () => {
    const chat = build("chat", "m", " Be brief. ", HISTORY, null, false, PLAIN);
    expect(chat).toEqual({
      model: "m",
      stream: true,
      stream_options: { include_usage: true },
      messages: [{ role: "system", content: "Be brief." }, ...HISTORY.map(({ role, text }) => ({ role, content: text }))],
    });
    const responses = build("responses", "m", "Be brief.", HISTORY, null, true, PLAIN);
    expect(responses).toMatchObject({ model: "m", stream: true, store: true, instructions: "Be brief." });
    expect(responses.input).toHaveLength(3);
    const messages = build("messages", "m", "Be brief.", HISTORY, null, false, PLAIN);
    expect(messages).toMatchObject({ model: "m", stream: true, max_tokens: 4096, system: "Be brief." });
    expect(messages.messages).toHaveLength(3);
  });

  it("leaves an empty system prompt out", () => {
    expect(build("chat", "m", "  ", HISTORY, null, false, PLAIN).messages).toHaveLength(3);
    expect(build("responses", "m", "", HISTORY, null, false, PLAIN)).not.toHaveProperty("instructions");
    expect(build("messages", "m", "", HISTORY, null, false, PLAIN)).not.toHaveProperty("system");
  });

  it("continues a Responses conversation with only the new turn", () => {
    const body = build("responses", "m", "", HISTORY, "resp_1", true, PLAIN);
    expect(body.previous_response_id).toBe("resp_1");
    expect(body.input).toEqual([{ role: "user", content: "Why is the sky blue?" }]);
  });

  it("sends files before the text, in each API's parts", () => {
    const turn: Turn[] = [{ role: "user", text: "What is this?", files: [PNG, PDF] }];
    expect(build("chat", "m", "", turn, null, false, PLAIN).messages[0].content).toEqual([
      { type: "image_url", image_url: { url: "data:image/png;base64,AAA" } },
      { type: "file", file: { filename: "b.pdf", file_data: "data:application/pdf;base64,BBB" } },
      { type: "text", text: "What is this?" },
    ]);
    expect(build("responses", "m", "", turn, null, false, PLAIN).input[0].content).toEqual([
      { type: "input_image", image_url: "data:image/png;base64,AAA" },
      { type: "input_file", filename: "b.pdf", file_data: "data:application/pdf;base64,BBB" },
      { type: "input_text", text: "What is this?" },
    ]);
    expect(build("messages", "m", "", turn, null, false, PLAIN).messages[0].content).toEqual([
      { type: "image", source: { type: "base64", media_type: "image/png", data: "AAA" } },
      { type: "document", title: "b.pdf", source: { type: "base64", media_type: "application/pdf", data: "BBB" } },
      { type: "text", text: "What is this?" },
    ]);
  });

  it("puts the effort, web search, MCP servers and schema in each API's fields", () => {
    const schema = { type: "object" };
    const all: ChatOptions = { effort: "high", webSearch: true, mcp: ["docs"], schema };
    const mcpTool = { type: "mcp", server_label: "docs", require_approval: "never" };
    expect(build("chat", "m", "", HISTORY, null, false, all)).toMatchObject({
      reasoning_effort: "high",
      web_search_options: {},
      tools: [mcpTool],
      response_format: { type: "json_schema", json_schema: { name: "output", schema, strict: true } },
    });
    expect(build("responses", "m", "", HISTORY, null, false, all)).toMatchObject({
      reasoning: { effort: "high" },
      tools: [{ type: "web_search" }, mcpTool],
      text: { format: { type: "json_schema", name: "output", schema, strict: true } },
    });
    expect(build("messages", "m", "", HISTORY, null, false, all)).toMatchObject({
      thinking: { type: "adaptive" },
      output_config: { effort: "high", format: { type: "json_schema", schema } },
      tools: [{ type: "web_search_20250305", name: "web_search", max_uses: 5 }],
      mcp_servers: [{ type: "url", name: "docs" }],
    });
  });

  it("turns Anthropic's thinking off for effort none, and sends no tools without any", () => {
    const body = build("messages", "m", "", HISTORY, null, false, { ...PLAIN, effort: "none" });
    expect(body).toMatchObject({ thinking: { type: "disabled" } });
    expect(body).not.toHaveProperty("output_config");
    expect(body).not.toHaveProperty("tools");
  });
});

describe("streamed events", () => {
  it("reads Chat Completions deltas, citations and usage", () => {
    const r = stream("chat", [
      { choices: [{ delta: { content: "The sky " } }] },
      { choices: [{ delta: { content: "scatters light.", annotations: [{ url_citation: { url: "https://a.example", title: "A" } }] } }] },
      { choices: [{ delta: { annotations: [{ url_citation: { url: "https://a.example", title: "A" } }] } }] },
      { choices: [], usage: { prompt_tokens: 12, completion_tokens: 5 } },
      "[DONE]",
    ]);
    expect(r).toEqual({ text: "The sky scatters light.", sources: [{ url: "https://a.example", title: "A" }], mcp: [], input: 12, output: 5 });
  });

  it("reads Responses text, MCP items, citations and the completed response", () => {
    const r = stream("responses", [
      { type: "response.output_item.done", item: { type: "mcp_list_tools", server_label: "docs", tools: [{ name: "search" }, { name: "fetch" }] } },
      { type: "response.output_item.done", item: { type: "mcp_call", server_label: "docs", name: "search", arguments: '{"q":"sky"}', output: "found" } },
      { type: "response.output_text.delta", delta: "Rayleigh" },
      { type: "response.output_text.delta", delta: " scattering." },
      { type: "response.output_text.annotation.added", annotation: { type: "url_citation", url: "https://b.example" } },
      {
        type: "response.completed",
        response: {
          id: "resp_2",
          usage: { input_tokens: 20, output_tokens: 7 },
          output: [{ type: "web_search_call" }, { type: "web_search_call" }, { type: "message", content: [{ annotations: [{ type: "url_citation", url: "https://c.example", title: "C" }] }] }],
        },
      },
    ]);
    expect(r.text).toBe("Rayleigh scattering.");
    expect(r.responseId).toBe("resp_2");
    expect(r).toMatchObject({ input: 20, output: 7, searches: 2 });
    expect(r.sources).toEqual([{ url: "https://b.example", title: undefined }, { url: "https://c.example", title: "C" }]);
    expect(r.mcp).toEqual([
      { kind: "tools", server: "docs", tools: ["search", "fetch"] },
      { kind: "call", server: "docs", tool: "search", arguments: '{"q":"sky"}', output: "found", error: undefined },
    ]);
  });

  it("reads a failed Responses stream as an error", () => {
    expect(stream("responses", [{ type: "response.failed", response: { error: { message: "overloaded" } } }]).error).toBe("overloaded");
    expect(stream("responses", [{ type: "error", message: "bad request" }]).error).toBe("bad request");
  });

  it("reads Anthropic text, citations, MCP calls and usage with cached tokens", () => {
    const r = stream("messages", [
      { type: "message_start", message: { usage: { input_tokens: 10, cache_read_input_tokens: 90, cache_creation_input_tokens: 5, output_tokens: 1 } } },
      { type: "content_block_start", content_block: { type: "mcp_tool_use", id: "t1", server_name: "docs", name: "search", input: { q: "sky" } } },
      { type: "content_block_start", content_block: { type: "mcp_tool_result", tool_use_id: "t1", is_error: true, content: [{ type: "text", text: "denied" }] } },
      { type: "content_block_delta", delta: { type: "text_delta", text: "Blue " } },
      { type: "content_block_delta", delta: { type: "citations_delta", citation: { url: "https://d.example", title: "D" } } },
      { type: "content_block_delta", delta: { type: "text_delta", text: "light." } },
      { type: "message_delta", usage: { output_tokens: 42, server_tool_use: { web_search_requests: 3 } } },
    ]);
    expect(r).toMatchObject({ text: "Blue light.", input: 105, output: 42, searches: 3, sources: [{ url: "https://d.example", title: "D" }] });
    expect(r.mcp).toEqual([{ kind: "call", id: "t1", server: "docs", tool: "search", arguments: '{"q":"sky"}', error: "denied" }]);
  });

  it("skips events that are not JSON, and takes an error object's message", () => {
    const r = stream("chat", ["not json", { error: { message: "rate limited" } }]);
    expect(r.text).toBe("");
    expect(r.error).toBe("rate limited");
  });
});

describe("whole responses", () => {
  it("reads a Chat Completions response", () => {
    const r = parseFull("chat", {
      choices: [{ message: { content: "Hi.", annotations: [{ url_citation: { url: "https://e.example" } }] } }],
      usage: { prompt_tokens: 3, completion_tokens: 2 },
    });
    expect(r).toEqual({ text: "Hi.", input: 3, output: 2, sources: [{ url: "https://e.example", title: undefined }], mcp: [] });
  });

  it("reads a Responses response, refusals included", () => {
    const r = parseFull("responses", {
      id: "resp_3",
      usage: { input_tokens: 4, output_tokens: 6 },
      output: [
        { type: "mcp_call", server_label: "docs", name: "fetch", arguments: "{}", error: "timeout" },
        { type: "message", content: [{ text: "Part one. " }, { refusal: "No more." }] },
      ],
    });
    expect(r).toMatchObject({ text: "Part one. No more.", responseId: "resp_3", input: 4, output: 6 });
    expect(r.mcp).toEqual([{ kind: "call", server: "docs", tool: "fetch", arguments: "{}", output: undefined, error: "timeout" }]);
  });

  it("reads an Anthropic response", () => {
    const r = parseFull("messages", {
      usage: { input_tokens: 1, cache_read_input_tokens: 2, output_tokens: 3, server_tool_use: { web_search_requests: 1 } },
      content: [
        { type: "mcp_tool_use", id: "t2", server_name: "docs", name: "search", input: {} },
        { type: "mcp_tool_result", tool_use_id: "t2", content: [{ type: "text", text: "ok" }] },
        { type: "text", text: "Sky ", citations: [{ url: "https://f.example", title: "F" }] },
        { type: "text", text: "is blue." },
      ],
    });
    expect(r).toMatchObject({ text: "Sky is blue.", input: 3, output: 3, searches: 1, sources: [{ url: "https://f.example", title: "F" }] });
    expect(r.mcp).toEqual([{ kind: "call", id: "t2", server: "docs", tool: "search", arguments: "{}", output: "ok" }]);
  });
});

describe("JSON answers", () => {
  it("indents JSON and leaves anything else alone", () => {
    expect(indented('{"a":1}')).toBe('{\n  "a": 1\n}');
    expect(indented('{"a":')).toBeNull();
  });
});
