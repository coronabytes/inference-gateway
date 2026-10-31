// Mock upstream for local end-to-end testing. Speaks OpenAI Chat Completions, OpenAI
// Responses and Anthropic Messages (streaming and not), System One decisions, OpenAI
// embeddings, and rerank (Cohere v2, and Bedrock's Rerank API). No real model is called.
//
//   node dev/mock-upstream.mjs            # listens on :9999
//
// Model names control behaviour: anything containing "fail" returns 503 (to exercise
// failover/cooldown); requests with tools get a tool call back. A streamed Messages request
// with the web search tool gets a search, its results and a cited answer. Bedrock credentials
// pointed at the mock get a guardrail (see `applyGuardrail`).
import http from "node:http";

const PORT = Number(process.env.PORT ?? 9999);

function sse(res, event, data) {
  if (event) res.write(`event: ${event}\n`);
  res.write(`data: ${typeof data === "string" ? data : JSON.stringify(data)}\n\n`);
}

function lastUserText(body) {
  const msgs = body.messages ?? (Array.isArray(body.input) ? body.input : [{ content: body.input }]);
  for (let i = msgs.length - 1; i >= 0; i--) {
    const c = msgs[i].content;
    if (typeof c === "string") return c;
    if (Array.isArray(c)) {
      const t = c.find((p) => typeof p.text === "string");
      if (t) return t.text;
    }
  }
  return "";
}

const words = (body) => `echo: ${lastUserText(body)}`.split(/(?<= )/);

function chat(body, res) {
  const id = "chatcmpl-mock";
  const toolCall = body.tools?.length && !body.messages.some((m) => m.role === "tool");
  if (!body.stream) {
    const message = toolCall
      ? { role: "assistant", content: null, tool_calls: [{ id: "call_mock1", type: "function", function: { name: body.tools[0].function.name, arguments: '{"q":"x"}' } }] }
      : { role: "assistant", content: words(body).join("") };
    return json(res, 200, { id, object: "chat.completion", model: body.model, choices: [{ index: 0, message, finish_reason: toolCall ? "tool_calls" : "stop" }], usage: { prompt_tokens: 12, completion_tokens: 5, total_tokens: 17 } });
  }
  res.writeHead(200, { "content-type": "text/event-stream" });
  if (toolCall) {
    const name = body.tools[0].function.name;
    sse(res, null, { id, choices: [{ index: 0, delta: { role: "assistant", tool_calls: [{ index: 0, id: "call_mock1", type: "function", function: { name, arguments: '{"q"' } }] } }] });
    sse(res, null, { id, choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: ':"x"}' } }] } }] });
    sse(res, null, { id, choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] });
  } else {
    for (const w of words(body)) sse(res, null, { id, choices: [{ index: 0, delta: { content: w } }] });
    sse(res, null, { id, choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
  }
  sse(res, null, { id, choices: [], usage: { prompt_tokens: 12, completion_tokens: 5, total_tokens: 17, prompt_tokens_details: { cached_tokens: 2 } } });
  sse(res, null, "[DONE]");
  res.end();
}

function anthropic(body, res) {
  // Anthropic's own web search tool: answered with a search, its results and a cited answer.
  const search = body.tools?.find((t) => String(t.type ?? "").startsWith("web_search"));
  const clientTools = body.tools?.filter((t) => t !== search) ?? [];
  const toolCall = clientTools.length && !JSON.stringify(body.messages).includes("tool_result");
  const usage = { input_tokens: 11, output_tokens: 6, cache_read_input_tokens: 3, cache_creation_input_tokens: 0 };
  if (search && body.stream && !toolCall) return anthropicSearch(body, res, usage);
  if (!body.stream) {
    const content = toolCall
      ? [{ type: "tool_use", id: "toolu_mock1", name: body.tools[0].name, input: { q: "x" } }]
      : [{ type: "text", text: words(body).join("") }];
    return json(res, 200, { id: "msg_mock", type: "message", role: "assistant", model: body.model, content, stop_reason: toolCall ? "tool_use" : "end_turn", stop_sequence: null, usage });
  }
  res.writeHead(200, { "content-type": "text/event-stream" });
  sse(res, "message_start", { type: "message_start", message: { id: "msg_mock", type: "message", role: "assistant", model: body.model, content: [], usage: { ...usage, output_tokens: 1 } } });
  if (toolCall) {
    sse(res, "content_block_start", { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "toolu_mock1", name: body.tools[0].name, input: {} } });
    sse(res, "content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: '{"q":"x"}' } });
  } else {
    sse(res, "content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } });
    for (const w of words(body)) sse(res, "content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: w } });
  }
  sse(res, "content_block_stop", { type: "content_block_stop", index: 0 });
  sse(res, "message_delta", { type: "message_delta", delta: { stop_reason: toolCall ? "tool_use" : "end_turn" }, usage: { output_tokens: 6 } });
  sse(res, "message_stop", { type: "message_stop" });
  res.end();
}

function anthropicSearch(body, res, usage) {
  const result = { type: "web_search_result", url: "https://example.com/weather", title: "Mock weather", page_age: "1 hour", encrypted_content: "bW9jaw==".repeat(20) };
  const block = (index, content_block, deltas = []) => {
    sse(res, "content_block_start", { type: "content_block_start", index, content_block });
    for (const delta of deltas) sse(res, "content_block_delta", { type: "content_block_delta", index, delta });
    sse(res, "content_block_stop", { type: "content_block_stop", index });
  };
  res.writeHead(200, { "content-type": "text/event-stream" });
  sse(res, "message_start", { type: "message_start", message: { id: "msg_mock_search", type: "message", role: "assistant", model: body.model, content: [], usage: { ...usage, output_tokens: 1 } } });
  block(0, { type: "server_tool_use", id: "srvtoolu_mock1", name: "web_search", input: {} }, [{ type: "input_json_delta", partial_json: `{"query":${JSON.stringify(lastUserText(body))}}` }]);
  block(1, { type: "web_search_tool_result", tool_use_id: "srvtoolu_mock1", content: [result] });
  block(2, { type: "text", text: "" }, [
    { type: "text_delta", text: "According to the mock, it is sunny." },
    { type: "citations_delta", citation: { type: "web_search_result_location", url: result.url, title: result.title, cited_text: "Sunny all day." } },
  ]);
  sse(res, "message_delta", { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 9, server_tool_use: { web_search_requests: 1 } } });
  sse(res, "message_stop", { type: "message_stop" });
  res.end();
}

// Responses the mock "stored", per upstream model. Each model name behaves like a separate
// resource (e.g. two Azure Foundry resources): it only knows ids it issued itself.
const issued = new Map();
let responseCount = 0;

function responses(body, res) {
  const known = issued.get(body.model) ?? new Set();
  issued.set(body.model, known);
  if (body.previous_response_id && !known.has(body.previous_response_id)) {
    return json(res, 404, { error: { message: `Previous response with id '${body.previous_response_id}' not found.`, type: "invalid_request_error" } });
  }
  const id = `resp_up${++responseCount}_${body.model}`;
  if (body.store !== false) known.add(id);
  const text = words(body).join("");
  const usage = { input_tokens: 10, input_tokens_details: { cached_tokens: 0 }, output_tokens: 4, total_tokens: 14 };
  const item = { type: "message", id: "msg_mock", status: "completed", role: "assistant", content: [{ type: "output_text", text, annotations: [] }] };
  const response = { id, object: "response", status: "completed", model: body.model, output: [item], usage };
  if (!body.stream) return json(res, 200, response);
  res.writeHead(200, { "content-type": "text/event-stream" });
  let seq = 0;
  const ev = (type, data) => sse(res, type, { type, sequence_number: seq++, ...data });
  ev("response.created", { response: { ...response, status: "in_progress", output: [] } });
  ev("response.output_item.added", { output_index: 0, item: { ...item, status: "in_progress", content: [] } });
  for (const w of words(body)) ev("response.output_text.delta", { output_index: 0, content_index: 0, item_id: "msg_mock", delta: w });
  ev("response.output_text.done", { output_index: 0, content_index: 0, item_id: "msg_mock", text });
  ev("response.output_item.done", { output_index: 0, item });
  ev("response.completed", { response });
  res.end();
}

// System One decisions (TypeSafe /v1/systemone, Foundry /providers/microsoft/v1/systemone).
// Echoes the danger-zone headers it received so tests can check forwarding.
function systemone(body, req, res) {
  const answers = {};
  for (const [name, q] of Object.entries(body.questions ?? {})) {
    // With criteria: likely true when a word of the true criterion is in the state (tool routers' routes).
    if (q.type === "noul" && q.criteria?.true) {
      const state = JSON.stringify(body.state).toLowerCase();
      const hit = q.criteria.true.toLowerCase().split(/[^a-z0-9]+/).some((w) => w.length > 3 && state.includes(w));
      answers[name] = { type: "noul", noul: hit ? 0.9 : 0.1 };
    } else if (q.type === "noul") answers[name] = { type: "noul", noul: 0.75 };
    if (q.type === "choice") {
      const labels = Object.keys(q.criteria);
      const probabilities = Object.fromEntries(labels.map((l, i) => [l, i === labels.length - 1 ? 0.7 : 0.3 / (labels.length - 1)]));
      answers[name] = { type: "choice", choice: labels.at(-1), confidence: 0.4, probabilities };
    }
    if (q.type === "score") {
      const n = q.criteria.length;
      answers[name] = { type: "score", score: n - 1, confidence: 0.9, legend: q.criteria, probabilities: Object.fromEntries(q.criteria.map((_, i) => [String(i), i === n - 1 ? 1 : 0])) };
    }
    if (q.type === "box") answers[name] = { type: "box", box: { top: 0.1, bottom: 0.5, left: 0.2, right: 0.6 }, confidence: 0.8 };
  }
  const echo = Object.fromEntries(Object.entries(req.headers).filter(([k]) => k.startsWith("typesafe-danger-zone-")));
  const input_tokens = JSON.stringify(body.state).length + Object.keys(body.assets ?? {}).length * 100;
  json(res, 200, { model: `${body.model}-mock`, answers, usage: { input_tokens, output_tokens: 3 }, mock_headers: echo, mock_url: req.url });
}

// OpenAI embeddings: deterministic small vectors, float arrays or base64.
function embeddings(body, res) {
  const inputs = Array.isArray(body.input) && typeof body.input[0] !== "number" ? body.input : [body.input];
  const dims = body.dimensions ?? 4;
  const data = inputs.map((text, index) => {
    const v = Array.from({ length: dims }, (_, i) => Number(((JSON.stringify(text).length * (i + 1)) % 97 / 97).toFixed(4)));
    const embedding = body.encoding_format === "base64" ? Buffer.from(new Float32Array(v).buffer).toString("base64") : v;
    return { object: "embedding", index, embedding };
  });
  const tokens = inputs.reduce((n, t) => n + Math.ceil(JSON.stringify(t).length / 4), 0);
  json(res, 200, { object: "list", data, model: body.model, usage: { prompt_tokens: tokens, total_tokens: tokens } });
}

// Bedrock InvokeModel for Amazon Titan Text Embeddings V2: one text per call. Like Bedrock, it
// rejects unknown fields and unsupported dimensions. Model ids containing "fail" return 503.
function titan(body, req, res) {
  if (req.url.includes("fail")) return json(res, 503, { message: "mock Bedrock throttled" });
  const extra = Object.keys(body).filter((k) => !["inputText", "dimensions", "normalize", "embeddingTypes"].includes(k));
  if (extra.length) return json(res, 400, { message: `extraneous key [${extra[0]}] is not permitted` });
  const dims = body.dimensions ?? 1024;
  if (typeof body.inputText !== "string" || ![256, 512, 1024].includes(dims)) return json(res, 400, { message: "Malformed input request" });
  let v = Array.from({ length: dims }, (_, i) => ((body.inputText.length * (i + 7)) % 101) / 101 - 0.5);
  if (body.normalize) {
    const n = Math.hypot(...v);
    v = v.map((x) => x / n);
  }
  json(res, 200, { embedding: v, inputTextTokenCount: Math.ceil(body.inputText.length / 4), embeddingsByType: { float: v } });
}

// Rerank: scores each document by the share of query words it contains. Cohere v2 requests
// (`query`, `documents`) get Cohere's answer with billed searches; Bedrock Rerank requests
// (`queries`, `sources`) get Bedrock's, which reports no usage.
function rerank(body, res) {
  const words = (s) => new Set(String(s).toLowerCase().match(/[a-z0-9]+/g) ?? []);
  const score = (q, d) => {
    const [qw, dw] = [words(q), words(d)];
    return qw.size ? [...qw].filter((w) => dw.has(w)).length / qw.size : 0;
  };
  const bedrock = Array.isArray(body.queries);
  const query = bedrock ? body.queries[0]?.textQuery?.text : body.query;
  const docs = bedrock ? (body.sources ?? []).map((s) => s.inlineDocumentSource?.textDocument?.text ?? "") : body.documents ?? [];
  const n = bedrock ? body.rerankingConfiguration?.bedrockRerankingConfiguration?.numberOfResults : body.top_n;
  if (bedrock && String(body.rerankingConfiguration?.bedrockRerankingConfiguration?.modelConfiguration?.modelArn).includes("fail")) {
    return json(res, 503, { message: "mock Bedrock throttled" });
  }
  const ranked = docs
    .map((d, index) => ({ index, score: Number(score(query, d).toFixed(4)) }))
    .sort((a, b) => b.score - a.score)
    .slice(0, n ?? docs.length);
  if (bedrock) return json(res, 200, { results: ranked.map((r) => ({ index: r.index, relevanceScore: r.score })) });
  json(res, 200, {
    id: "mock-rerank",
    results: ranked.map((r) => ({ index: r.index, relevance_score: r.score })),
    meta: { api_version: { version: "2" }, billed_units: { search_units: Math.ceil(docs.length / 100) } },
  });
}

// Bedrock ApplyGuardrail, for a Bedrock credential whose endpoint is the mock. Each text block is
// screened on its own: an e-mail address or the name "Ada Lovelace" is masked ({EMAIL}, {NAME}),
// "lottery" or "casino" is flagged (a denied topic it only detects), and "ignore previous
// instructions" is blocked as a prompt attack.
function applyGuardrail(body, res) {
  const texts = (body.content ?? []).map((c) => c.text?.text ?? "");
  const pii = [];
  const topics = [];
  const filters = [];
  const outputs = texts.map((t) => {
    for (const m of t.match(/[\w.+-]+@[\w-]+\.[\w.]+/g) ?? []) pii.push({ match: m, type: "EMAIL", action: "ANONYMIZED", detected: true });
    for (const m of t.match(/Ada Lovelace/g) ?? []) pii.push({ match: m, type: "NAME", action: "ANONYMIZED", detected: true });
    if (/lottery|casino/i.test(t)) topics.push({ name: "Gambling", type: "DENY", action: "NONE", detected: true });
    if (/ignore previous instructions/i.test(t)) filters.push({ type: "PROMPT_ATTACK", confidence: "HIGH", filterStrength: "HIGH", action: "BLOCKED", detected: true });
    return t.replace(/[\w.+-]+@[\w-]+\.[\w.]+/g, "{EMAIL}").replace(/Ada Lovelace/g, "{NAME}");
  });
  const blocked = filters.length > 0;
  const intervened = blocked || pii.length > 0;
  const units = Math.max(1, Math.ceil(texts.join("").length / 1000));
  json(res, 200, {
    action: intervened ? "GUARDRAIL_INTERVENED" : "NONE",
    actionReason: blocked ? "Guardrail blocked." : intervened ? "Guardrail masked." : "No action.",
    outputs: blocked ? [{ text: "Sorry, the mock guardrail blocked this request." }] : outputs.map((text) => ({ text })),
    assessments: [{
      contentPolicy: { filters },
      topicPolicy: { topics },
      sensitiveInformationPolicy: { piiEntities: pii, regexes: [] },
    }],
    usage: {
      topicPolicyUnits: units, contentPolicyUnits: units, wordPolicyUnits: 0,
      sensitiveInformationPolicyUnits: units, sensitiveInformationPolicyFreeUnits: 0, contextualGroundingPolicyUnits: 0,
    },
  });
}

function json(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

http
  .createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      let body = {};
      try {
        body = JSON.parse(raw || "{}");
      } catch {
        return json(res, 400, { error: { message: "bad json" } });
      }
      const turns = Array.isArray(body.messages) ? body.messages.length : Array.isArray(body.input) ? body.input.length : 1;
      const prev = body.previous_response_id ? ` previous=${body.previous_response_id}` : "";
      console.log(`${req.method} ${req.url} model=${body.model} stream=${!!body.stream} messages=${turns}${prev} auth=${req.headers.authorization ? req.headers.authorization.split(" ")[0] : req.headers["x-api-key"] ? "x-api-key" : req.headers["api-key"] ? "api-key" : "none"}`);
      if (String(body.model).includes("fail")) return json(res, 503, { error: { message: "mock upstream overloaded" } });
      if (req.url.endsWith("/chat/completions")) return chat(body, res);
      if (req.url.endsWith("/responses")) return responses(body, res);
      if (req.url.endsWith("/messages/count_tokens")) return json(res, 200, { input_tokens: 42 });
      if (req.url.endsWith("/messages")) return anthropic(body, res);
      if (req.url.split("?")[0].endsWith("/systemone")) return systemone(body, req, res);
      if (req.url.split("?")[0].endsWith("/embeddings")) return embeddings(body, res);
      if (/\/model\/[^/]+\/invoke$/.test(req.url)) return titan(body, req, res);
      if (/\/guardrail\/[^/]+\/version\/[^/]+\/apply$/.test(req.url)) return applyGuardrail(body, res);
      if (req.url.split("?")[0].endsWith("/rerank")) return rerank(body, res);
      json(res, 404, { error: { message: `no route ${req.url}` } });
    });
  })
  .listen(PORT, () => console.log(`mock upstream on :${PORT}`));
