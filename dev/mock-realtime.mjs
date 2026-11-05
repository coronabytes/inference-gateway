// Mock of Mistral's realtime transcription WebSocket, for local end-to-end testing of
// /v1/realtime?intent=transcription. Point a Mistral credential's endpoint at http://127.0.0.1:16608/v1.
//
//   bun dev/mock-realtime.mjs            # listens on :16608
//
// It "hears" one word of a canned sentence per half second of audio, and sends the rest when the
// audio ends. A model name containing "fail" gets an error instead of a session.

const PORT = Number(process.env.PORT ?? 16608);
const WORDS = "This is a mock transcript of your recording, streamed while you speak.".split(" ");

Bun.serve({
  port: PORT,
  fetch(req, server) {
    const url = new URL(req.url);
    if (!url.pathname.endsWith("/audio/transcriptions/realtime")) return new Response("not found", { status: 404 });
    if (!req.headers.get("authorization")) return new Response(JSON.stringify({ message: "missing key" }), { status: 401 });
    const model = url.searchParams.get("model") ?? "";
    if (server.upgrade(req, { data: { model, rate: 16000, bytes: 0, said: 0 } })) return undefined;
    return new Response("expected a WebSocket", { status: 400 });
  },
  websocket: {
    open(ws) {
      console.log(`realtime session model=${ws.data.model}`);
      if (ws.data.model.includes("fail")) {
        ws.send(JSON.stringify({ type: "error", error: { message: "mock upstream overloaded", code: 503 } }));
        return ws.close();
      }
      ws.send(JSON.stringify({ type: "session.created", session: { audio_format: { encoding: "pcm_s16le", sample_rate: 16000 } } }));
    },
    message(ws, raw) {
      const e = JSON.parse(String(raw));
      const s = ws.data;
      const say = (n) => {
        for (; s.said < Math.min(n, WORDS.length); s.said++) ws.send(JSON.stringify({ type: "transcription.text.delta", text: (s.said ? " " : "") + WORDS[s.said] }));
      };
      if (e.type === "session.update") {
        s.rate = e.session?.audio_format?.sample_rate ?? s.rate;
        ws.send(JSON.stringify({ type: "session.updated", session: e.session }));
      } else if (e.type === "input_audio.append") {
        s.bytes += Buffer.from(e.audio, "base64").length;
        say(Math.floor(s.bytes / (s.rate * 2) / 0.5));
      } else if (e.type === "input_audio.end") {
        say(WORDS.length);
        const seconds = Math.ceil(s.bytes / (s.rate * 2));
        console.log(`  ended after ${seconds} s at ${s.rate} Hz`);
        ws.send(JSON.stringify({ type: "transcription.done", text: WORDS.join(" "), model: s.model, usage: { prompt_audio_seconds: seconds } }));
      }
    },
  },
});
console.log(`mock realtime upstream on :${PORT}`);
