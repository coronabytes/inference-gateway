import type { ReasoningEffort } from "../../api";

/*
 * The playground's chat protocols: each API's request body, its streamed events and its whole
 * responses, read into one `Result`. Pure functions, so they can be tested without a browser.
 */

/** The chat APIs the playground speaks, by the gateway endpoint each is sent to. */
export type Api = "chat" | "responses" | "messages";

export const APIS: { id: Api; label: string; path: string }[] = [
  { id: "responses", label: "Responses", path: "/v1/responses" },
  { id: "chat", label: "Chat Completions", path: "/v1/chat/completions" },
  { id: "messages", label: "Anthropic Messages", path: "/v1/messages" },
];

/** A file sent with a user turn, inline as base64. */
export interface Attachment {
  name: string;
  type: string;
  size: number;
  data: string;
}

export interface Turn {
  role: "user" | "assistant";
  text: string;
  files?: Attachment[];
}

export const dataUrl = (f: Attachment) => `data:${f.type};base64,${f.data}`;
export const isImage = (f: Attachment) => f.type.startsWith("image/");

/** A turn's content in the API's shape: plain text, or its files and then its text as parts. */
function content(apiId: Api, t: Turn) {
  if (!t.files?.length) return t.text;
  const text = t.text ? [t.text] : [];
  switch (apiId) {
    case "chat":
      return [
        ...t.files.map((f) =>
          isImage(f) ? { type: "image_url", image_url: { url: dataUrl(f) } } : { type: "file", file: { filename: f.name, file_data: dataUrl(f) } },
        ),
        ...text.map((s) => ({ type: "text", text: s })),
      ];
    case "responses":
      return [
        ...t.files.map((f) => (isImage(f) ? { type: "input_image", image_url: dataUrl(f) } : { type: "input_file", filename: f.name, file_data: dataUrl(f) })),
        ...text.map((s) => ({ type: "input_text", text: s })),
      ];
    case "messages":
      return [
        ...t.files.map((f) => {
          const source = { type: "base64", media_type: f.type, data: f.data };
          return isImage(f) ? { type: "image", source } : { type: "document", title: f.name, source };
        }),
        ...text.map((s) => ({ type: "text", text: s })),
      ];
  }
}

/** A web page an answer cites. */
export interface Source {
  url: string;
  title?: string;
}

/** What the gateway did with an MCP server's tools for a reply, as the API returned it: the tools
 *  the server offered (Responses only), or a call with its arguments and result. */
export type McpView =
  | { kind: "tools"; server: string; tools: string[] }
  | { kind: "call"; id?: string; server: string; tool: string; arguments: string; output?: string; error?: string };

export interface Result {
  text: string;
  input?: number;
  output?: number;
  /** Web searches the model ran, where the API reports them. */
  searches?: number;
  sources: Source[];
  mcp: McpView[];
  responseId?: string;
  error?: string;
}

/** JSON indented for reading; `null` for anything else, such as a truncated answer or one from a
 *  model that ignored the schema. */
export function indented(text: string): string | null {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return null;
  }
}

/** Adds the pages not cited yet. */
function cite(r: Result, sources: (Source | undefined)[]) {
  for (const s of sources) if (s?.url && !r.sources.some((x) => x.url === s.url)) r.sources.push({ url: s.url, title: s.title });
}

/** A Responses API output item of the gateway's MCP tools: `mcp_list_tools` or `mcp_call`. */
function responsesMcp(o: any): McpView | null {
  if (o?.type === "mcp_list_tools") return { kind: "tools", server: o.server_label, tools: (o.tools ?? []).map((t: any) => t.name) };
  if (o?.type === "mcp_call")
    return { kind: "call", server: o.server_label, tool: o.name, arguments: o.arguments ?? "", output: o.output ?? undefined, error: o.error ?? undefined };
  return null;
}

/** Anthropic's MCP blocks: `mcp_tool_use` starts a call, the `mcp_tool_result` after it completes it. */
function messagesMcp(r: Result, b: any) {
  if (b?.type === "mcp_tool_use") r.mcp.push({ kind: "call", id: b.id, server: b.server_name, tool: b.name, arguments: JSON.stringify(b.input ?? {}) });
  if (b?.type !== "mcp_tool_result") return;
  const call = r.mcp.find((m) => m.kind === "call" && m.id === b.tool_use_id);
  if (call?.kind !== "call") return;
  const text = (b.content ?? []).map((c: any) => c.text ?? JSON.stringify(c)).join("\n");
  if (b.is_error) call.error = text;
  else call.output = text;
}

/** What a Responses API output cites, and how many web searches it ran. */
function responsesOutput(output: any[] | undefined, r: Result) {
  const items = output ?? [];
  r.searches = items.filter((o) => o.type === "web_search_call").length || r.searches;
  cite(r, items.flatMap((o) => o.content ?? []).flatMap((c: any) => c.annotations ?? []).filter((a: any) => a.type === "url_citation"));
}

/** Options sent with every chat request, in the chosen API's own fields. */
export interface ChatOptions {
  effort: ReasoningEffort | "";
  /** Let the model search the web: the API's own web search tool. */
  webSearch: boolean;
  /** Gateway MCP servers whose tools the model gets: the gateway runs the calls. */
  mcp: string[];
  /** Structured output: the answer is JSON matching this schema. */
  schema: object | null;
}

/** A reasoning effort in each API's own fields; Anthropic's `none` turns thinking off. */
function effortFields(apiId: Api, effort: ReasoningEffort | "") {
  if (!effort) return {};
  switch (apiId) {
    case "chat":
      return { reasoning_effort: effort };
    case "responses":
      return { reasoning: { effort } };
    case "messages":
      return effort === "none" ? { thinking: { type: "disabled" } } : { thinking: { type: "adaptive" }, output_config: { effort } };
  }
}

/** Web search, MCP servers and a JSON schema for the answer in each API's own fields, with the effort's.
 *  MCP servers are OpenAI's `mcp` tools in Responses and Chat Completions, Anthropic's `mcp_servers` in Messages. */
function optionFields(apiId: Api, { effort, webSearch, mcp, schema }: ChatOptions) {
  const fields: Record<string, unknown> = effortFields(apiId, effort);
  const tools: object[] = [];
  const mcpTools = mcp.map((server_label) => ({ type: "mcp", server_label, require_approval: "never" }));
  switch (apiId) {
    case "chat":
      if (webSearch) fields.web_search_options = {};
      tools.push(...mcpTools);
      if (schema) fields.response_format = { type: "json_schema", json_schema: { name: "output", schema, strict: true } };
      break;
    case "responses":
      if (webSearch) tools.push({ type: "web_search" });
      tools.push(...mcpTools);
      if (schema) fields.text = { format: { type: "json_schema", name: "output", schema, strict: true } };
      break;
    case "messages":
      if (webSearch) tools.push({ type: "web_search_20250305", name: "web_search", max_uses: 5 });
      if (mcp.length) fields.mcp_servers = mcp.map((name) => ({ type: "url", name }));
      // Beside the effort, if one is set.
      if (schema) fields.output_config = { ...(fields.output_config as object), format: { type: "json_schema", schema } };
      break;
  }
  if (tools.length) fields.tools = tools;
  return fields;
}

/** Chat requests always stream: every chat API supports it, and replies appear as they are written. */
export function buildBody(apiId: Api, model: string, system: string, history: Turn[], previousId: string | null, store: boolean, options: ChatOptions) {
  const sys = system.trim();
  switch (apiId) {
    case "chat":
      return {
        model,
        stream: true,
        stream_options: { include_usage: true },
        ...optionFields(apiId, options),
        messages: [...(sys ? [{ role: "system", content: sys }] : []), ...history.map((t) => ({ role: t.role, content: content(apiId, t) }))],
      };
    case "responses": {
      // Chained: only the new user turn plus previous_response_id; the gateway holds the rest.
      const turns = previousId ? history.slice(-1) : history;
      return {
        model,
        stream: true,
        store,
        ...optionFields(apiId, options),
        ...(sys && { instructions: sys }),
        ...(previousId && { previous_response_id: previousId }),
        input: turns.map((t) => ({ role: t.role, content: content(apiId, t) })),
      };
    }
    case "messages":
      return {
        model,
        stream: true,
        max_tokens: 4096,
        ...optionFields(apiId, options),
        ...(sys && { system: sys }),
        messages: history.map((t) => ({ role: t.role, content: content(apiId, t) })),
      };
  }
}

/** Applies one stream event to the running result. */
export function applyEvent(apiId: Api, ev: { event?: string; data: string }, r: Result) {
  if (ev.data === "[DONE]") return;
  let v: any;
  try {
    v = JSON.parse(ev.data);
  } catch {
    return;
  }
  if (v.error) r.error = v.error.message ?? String(v.error);
  switch (apiId) {
    case "chat":
      r.text += v.choices?.[0]?.delta?.content ?? "";
      cite(r, (v.choices?.[0]?.delta?.annotations ?? []).map((a: any) => a.url_citation));
      if (v.usage) Object.assign(r, { input: v.usage.prompt_tokens, output: v.usage.completion_tokens });
      break;
    case "responses":
      if (v.type === "response.output_text.delta") r.text += v.delta ?? "";
      if (v.type === "response.output_text.annotation.added" && v.annotation?.type === "url_citation") cite(r, [v.annotation]);
      if (v.type === "response.output_item.done") {
        const m = responsesMcp(v.item);
        if (m) r.mcp.push(m);
      }
      if (v.type === "response.completed" || v.type === "response.incomplete") {
        r.responseId = v.response?.id;
        Object.assign(r, { input: v.response?.usage?.input_tokens, output: v.response?.usage?.output_tokens });
        responsesOutput(v.response?.output, r);
      }
      if (v.type === "response.failed") r.error = v.response?.error?.message ?? "response failed";
      if (v.type === "error") r.error = v.message ?? r.error;
      break;
    case "messages": {
      if (v.type === "content_block_delta" && v.delta?.type === "text_delta") r.text += v.delta.text;
      if (v.type === "content_block_delta" && v.delta?.type === "citations_delta") cite(r, [v.delta.citation]);
      if (v.type === "content_block_start") messagesMcp(r, v.content_block);
      const u = v.type === "message_start" ? v.message?.usage : v.type === "message_delta" ? v.usage : null;
      if (u) {
        const input = (u.input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0);
        if (input) r.input = input;
        if (u.output_tokens) r.output = u.output_tokens;
        if (u.server_tool_use?.web_search_requests) r.searches = u.server_tool_use.web_search_requests;
      }
      break;
    }
  }
}

export function parseFull(apiId: Api, v: any): Result {
  switch (apiId) {
    case "chat": {
      const m = v.choices?.[0]?.message;
      const r: Result = { text: m?.content ?? "", input: v.usage?.prompt_tokens, output: v.usage?.completion_tokens, sources: [], mcp: [] };
      cite(r, (m?.annotations ?? []).map((a: any) => a.url_citation));
      return r;
    }
    case "responses": {
      const text = (v.output ?? [])
        .filter((o: any) => o.type === "message")
        .flatMap((o: any) => o.content ?? [])
        .map((c: any) => c.text ?? c.refusal ?? "")
        .join("");
      const mcp = (v.output ?? []).map(responsesMcp).filter((m: McpView | null): m is McpView => !!m);
      const r: Result = { text, responseId: v.id, input: v.usage?.input_tokens, output: v.usage?.output_tokens, sources: [], mcp };
      responsesOutput(v.output, r);
      return r;
    }
    case "messages": {
      const u = v.usage ?? {};
      const blocks = (v.content ?? []).filter((c: any) => c.type === "text");
      const r: Result = {
        text: blocks.map((c: any) => c.text).join(""),
        input: (u.input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0),
        output: u.output_tokens,
        searches: u.server_tool_use?.web_search_requests,
        sources: [],
        mcp: [],
      };
      for (const b of v.content ?? []) messagesMcp(r, b);
      cite(r, blocks.flatMap((c: any) => c.citations ?? []));
      return r;
    }
  }
}
