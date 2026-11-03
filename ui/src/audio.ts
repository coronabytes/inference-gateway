/** The playground's audio: recording from the microphone, transcribing it (also live), and speaking text. */

import { useCallback, useEffect, useRef, useState } from "react";
import { adminFetch, adminToken } from "./api";
import { labelHeaders, type Labels } from "./labels";

/** OpenAI's speech API takes at most this many characters; longer replies are read up to here. */
export const MAX_SPEECH_CHARS = 4096;

/** What the browser records in, best first: every one is a format transcription models take. */
const RECORDING_TYPES = ["audio/webm;codecs=opus", "audio/webm", "audio/ogg;codecs=opus", "audio/mp4"];

const EXTENSION: Record<string, string> = { "audio/webm": "webm", "audio/ogg": "ogg", "audio/mp4": "m4a" };

async function failure(res: Response): Promise<Error> {
  const data = await res.json().catch(() => ({}));
  return new Error(data?.error?.message ?? data?.error ?? `request failed (${res.status})`);
}

export interface Transcript {
  text: string;
  /** The response as the endpoint returned it: JSON, or text for `text`, `srt` and `vtt`. */
  raw: string;
  latency: number;
  requestId?: string;
  /** Seconds of audio or tokens, as the model reported them. */
  usage?: { type?: string; seconds?: number; input_tokens?: number; output_tokens?: number };
}

/** Transcribe `audio` with `model` as the key, through the admin playground. */
export async function transcribe(
  keyId: string,
  model: string,
  audio: Blob,
  labels: Labels,
  fields: Record<string, string> = {},
  signal?: AbortSignal,
): Promise<Transcript> {
  const form = new FormData();
  form.append("model", model);
  const type = audio.type.split(";")[0];
  const name = audio instanceof File ? audio.name : `recording.${EXTENSION[type] ?? "webm"}`;
  form.append("file", audio, name);
  for (const [k, v] of Object.entries(fields)) if (v) form.append(k, v);
  const started = performance.now();
  const res = await adminFetch(`/playground/transcriptions?key_id=${encodeURIComponent(keyId)}`, form, signal, labelHeaders(labels));
  if (!res.ok) throw await failure(res);
  const raw = await res.text();
  const json = res.headers.get("content-type")?.includes("json") ? JSON.parse(raw) : null;
  return {
    text: json ? (json.text ?? "") : raw,
    raw: json ? JSON.stringify(json, null, 2) : raw,
    latency: performance.now() - started,
    requestId: res.headers.get("x-request-id") ?? undefined,
    usage: json?.usage,
  };
}

export interface Speech {
  audio: Blob;
  latency: number;
  requestId?: string;
}

/** Speak `text` with `model` as the key, through the admin playground. */
export async function speak(
  keyId: string,
  model: string,
  text: string,
  labels: Labels,
  options: { voice?: string; response_format?: string } = {},
  signal?: AbortSignal,
): Promise<Speech> {
  const body = { model, input: text.slice(0, MAX_SPEECH_CHARS), ...Object.fromEntries(Object.entries(options).filter(([, v]) => v)) };
  const started = performance.now();
  const res = await adminFetch(`/playground/speech?key_id=${encodeURIComponent(keyId)}`, body, signal, labelHeaders(labels));
  if (!res.ok) throw await failure(res);
  return { audio: await res.blob(), latency: performance.now() - started, requestId: res.headers.get("x-request-id") ?? undefined };
}

/** Records from the microphone: `start` asks for it, `stop` resolves with the recording. */
export function useRecorder() {
  const [recording, setRecording] = useState(false);
  const recorder = useRef<MediaRecorder | null>(null);
  const done = useRef<((b: Blob | null) => void) | null>(null);
  const supported = typeof window !== "undefined" && "MediaRecorder" in window && !!navigator.mediaDevices?.getUserMedia;

  const start = useCallback(async () => {
    if (recorder.current) return;
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const mimeType = RECORDING_TYPES.find((t) => MediaRecorder.isTypeSupported(t));
    const r = new MediaRecorder(stream, mimeType ? { mimeType } : undefined);
    const chunks: Blob[] = [];
    r.ondataavailable = (e) => e.data.size && chunks.push(e.data);
    r.onstop = () => {
      stream.getTracks().forEach((t) => t.stop());
      recorder.current = null;
      setRecording(false);
      const blob = chunks.length ? new Blob(chunks, { type: r.mimeType || mimeType || "audio/webm" }) : null;
      done.current?.(blob);
      done.current = null;
    };
    r.start();
    recorder.current = r;
    setRecording(true);
  }, []);

  /** The recording, or null when nothing was recorded. */
  const stop = useCallback(
    () =>
      new Promise<Blob | null>((resolve) => {
        if (!recorder.current) return resolve(null);
        done.current = resolve;
        recorder.current.stop();
      }),
    [],
  );

  // The microphone is released when the page goes away mid-recording.
  useEffect(() => () => recorder.current?.stop(), []);
  return { supported, recording, start, stop };
}

/** Plays one sound at a time; `playing` is the id of what plays. */
export function usePlayer() {
  const [playing, setPlaying] = useState<string | null>(null);
  const audio = useRef<HTMLAudioElement | null>(null);

  const stop = useCallback(() => {
    if (audio.current) {
      audio.current.pause();
      URL.revokeObjectURL(audio.current.src);
      audio.current = null;
    }
    setPlaying(null);
  }, []);

  const play = useCallback(
    async (id: string, blob: Blob) => {
      stop();
      const a = new Audio(URL.createObjectURL(blob));
      audio.current = a;
      a.onended = () => audio.current === a && stop();
      setPlaying(id);
      await a.play();
    },
    [stop],
  );

  useEffect(() => stop, [stop]);
  return { playing, play, stop };
}

/** A microphone error, in words. */
export function microphoneError(e: unknown): string {
  const name = (e as Error)?.name;
  if (name === "NotAllowedError") return "The browser did not allow the microphone. Allow it for this site and try again.";
  if (name === "NotFoundError") return "No microphone found.";
  return `Could not record: ${(e as Error)?.message ?? e}`;
}

/** Below this peak (about -50 dB) a recording holds no speech: a muted or wrong microphone. */
const SILENT_PEAK = 0.003;

export const SILENT_RECORDING =
  "The recording is silent, so its transcript would be empty. Check which microphone the browser uses (Firefox asks when it gets access; its address bar icon changes it) and that it is not muted.";

/** Whether `audio` is silent; false where the browser cannot decode it, which leaves it to the model. */
export async function isSilent(audio: Blob): Promise<boolean> {
  const ctx = new AudioContext();
  try {
    const buf = await ctx.decodeAudioData(await audio.arrayBuffer());
    for (let c = 0; c < buf.numberOfChannels; c++) {
      const samples = buf.getChannelData(c);
      for (let i = 0; i < samples.length; i++) if (Math.abs(samples[i]) >= SILENT_PEAK) return false;
    }
    return buf.length > 0;
  } catch {
    return false;
  } finally {
    ctx.close();
  }
}

// ---------------------------------------------------------------- live transcription

/** OpenAI's rate for realtime transcription, which the gateway passes on to the model. */
const LIVE_RATE = 24_000;

/**
 * Captures the microphone as PCM16 at `rate`, in chunks of 100 ms, posted to the main thread.
 * Each output sample averages the input samples it covers, which keeps resampling from aliasing.
 */
const CAPTURE_WORKLET = `
class PcmCapture extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.ratio = sampleRate / options.processorOptions.rate;
    this.size = Math.round(options.processorOptions.rate / 10);
    this.out = new Int16Array(this.size);
    this.n = 0; this.sum = 0; this.count = 0; this.acc = 0; this.last = 0;
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;
    for (let i = 0; i < ch.length; i++) {
      this.sum += ch[i]; this.count++; this.acc += 1;
      while (this.acc >= this.ratio) {
        this.acc -= this.ratio;
        if (this.count) { this.last = this.sum / this.count; this.sum = 0; this.count = 0; }
        const v = Math.max(-1, Math.min(1, this.last));
        this.out[this.n++] = v < 0 ? v * 0x8000 : v * 0x7fff;
        if (this.n === this.size) {
          this.port.postMessage(this.out.buffer, [this.out.buffer]);
          this.out = new Int16Array(this.size); this.n = 0;
        }
      }
    }
    return true;
  }
}
registerProcessor("pcm-capture", PcmCapture);
`;

function base64(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

interface LiveSession {
  ws: WebSocket;
  stream: MediaStream;
  ctx: AudioContext;
  /** Resolves `stop` with the item's transcript, or null when it failed. */
  finished?: (transcript: string | null) => void;
  text: string;
}

/**
 * Streams the microphone to a realtime transcription model over the playground's WebSocket
 * (OpenAI's Realtime protocol): `start` begins and reports the text as it is heard, `stop` commits
 * the audio and resolves with the final transcript.
 */
export function useLiveTranscription() {
  const [live, setLive] = useState(false);
  const session = useRef<LiveSession | null>(null);
  const supported =
    typeof window !== "undefined" && "AudioWorkletNode" in window && "WebSocket" in window && !!navigator.mediaDevices?.getUserMedia;

  const release = useCallback(() => {
    const s = session.current;
    if (!s) return;
    session.current = null;
    s.stream.getTracks().forEach((t) => t.stop());
    s.ctx.close().catch(() => {});
    if (s.ws.readyState <= WebSocket.OPEN) s.ws.close();
    setLive(false);
  }, []);

  const start = useCallback(
    async (keyId: string, model: string, onText: (text: string) => void, onError: (message: string) => void, language?: string) => {
      if (session.current) return;
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
      });
      const token = adminToken();
      const scheme = location.protocol === "https:" ? "wss:" : "ws:";
      const ws = new WebSocket(
        `${scheme}//${location.host}/admin/api/playground/realtime?key_id=${encodeURIComponent(keyId)}`,
        token ? ["realtime", `openai-insecure-api-key.${token}`] : undefined,
      );
      const ctx = new AudioContext();
      const s: LiveSession = { ws, stream, ctx, text: "" };
      session.current = s;
      setLive(true);
      ws.onmessage = (m) => {
        const e = JSON.parse(String(m.data));
        if (e.type === "conversation.item.input_audio_transcription.delta") {
          s.text += e.delta ?? "";
          onText(s.text);
        } else if (e.type === "conversation.item.input_audio_transcription.completed") {
          s.finished?.(e.transcript ?? s.text);
        } else if (e.type === "conversation.item.input_audio_transcription.failed") {
          onError(e.error?.message ?? "the transcription failed");
          s.finished?.(null);
        } else if (e.type === "error" && e.error?.code === "input_audio_buffer_commit_empty") {
          // Stopped before any audio was sent: nothing to transcribe.
          s.finished?.(null);
        } else if (e.type === "error") {
          onError(e.error?.message ?? "the realtime session failed");
        }
      };
      ws.onclose = () => {
        s.finished?.(null);
        if (session.current === s) release();
      };
      try {
        await new Promise<void>((resolve, reject) => {
          ws.onopen = () => resolve();
          ws.onerror = () => reject(new Error("could not open the realtime session"));
        });
        const input: Record<string, unknown> = { model, ...(language && { language }) };
        ws.send(
          JSON.stringify({
            type: "session.update",
            session: {
              type: "transcription",
              audio: { input: { format: { type: "audio/pcm", rate: LIVE_RATE }, transcription: input, turn_detection: null } },
            },
          }),
        );
        const url = URL.createObjectURL(new Blob([CAPTURE_WORKLET], { type: "text/javascript" }));
        await ctx.audioWorklet.addModule(url);
        URL.revokeObjectURL(url);
        const capture = new AudioWorkletNode(ctx, "pcm-capture", { processorOptions: { rate: LIVE_RATE } });
        capture.port.onmessage = (e) => {
          if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: "input_audio_buffer.append", audio: base64(e.data) }));
        };
        // Pulled by the output so it keeps running, but silent: the microphone is not played back.
        const mute = ctx.createGain();
        mute.gain.value = 0;
        ctx.createMediaStreamSource(stream).connect(capture).connect(mute).connect(ctx.destination);
      } catch (e) {
        release();
        throw e;
      }
    },
    [release],
  );

  /** Commits what was said and resolves with its transcript (null when there is none). */
  const stop = useCallback(async (): Promise<string | null> => {
    const s = session.current;
    if (!s) return null;
    s.stream.getTracks().forEach((t) => t.stop());
    await s.ctx.close().catch(() => {});
    if (s.ws.readyState !== WebSocket.OPEN) {
      release();
      return s.text || null;
    }
    const transcript = await new Promise<string | null>((resolve) => {
      s.finished = resolve;
      s.ws.send(JSON.stringify({ type: "input_audio_buffer.commit" }));
      setTimeout(() => resolve(s.text || null), 15_000);
    });
    release();
    return transcript;
  }, [release]);

  useEffect(() => release, [release]);
  return { supported, live, start, stop };
}
