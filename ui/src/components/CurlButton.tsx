import { useState, type ReactNode } from "react";
import { bytes } from "../format";
import { labelHeaders, type Labels } from "../labels";
import { Button } from "./controls";
import { Modal } from "./ui";

/** A request to the gateway, as the Playground would send it; a GET without a `body`. */
export interface CurlRequest {
  path: string;
  body?: unknown;
  labels?: Labels;
  /** More headers to send, such as the guardrails to apply. */
  headers?: Record<string, string>;
  /** A streamed response: curl prints it as it arrives (`-N`). */
  stream?: boolean;
}

/** Single-quotes `s` for a POSIX shell. */
const shellQuote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;

/** The request as a curl command for this gateway; the key is left to `$GATEWAY_API_KEY`. */
export function curlCommand({ path, body, labels = {}, headers: extra, stream }: CurlRequest) {
  const headers = { Authorization: "Bearer $GATEWAY_API_KEY", ...(body !== undefined && { "Content-Type": "application/json" }), ...labelHeaders(labels), ...extra };
  const url = `${location.origin}${path}`;
  return [
    // A query string's `&` and `?` would mean something else to the shell.
    `curl${stream ? " -N" : ""} ${/^[\w\-.:/]+$/.test(url) ? url : shellQuote(url)}`,
    // Double quotes so the shell fills in the key; the other headers are quoted as they are.
    ...Object.entries(headers).map(([k, v]) => `-H ${k === "Authorization" ? `"${k}: ${v}"` : shellQuote(`${k}: ${v}`)}`),
    ...(body !== undefined ? [`-d ${shellQuote(JSON.stringify(body, null, 2))}`] : []),
  ].join(" \\\n  ");
}

/** For reading: long base64 runs (attached files) cut short, with their size. */
const abridged = (curl: string) =>
  curl.replace(/[A-Za-z0-9+/]{200,}={0,2}/g, (b64) => `${b64.slice(0, 32)}…(${bytes((b64.length * 3) / 4 - (b64.match(/=*$/)?.[0].length ?? 0))})`);

/**
 * Show curl: opens a dialog with the request `request()` returns (read when clicked) as a curl
 * command, and a Copy button. `keyName` is the virtual key whose secret `$GATEWAY_API_KEY` stands for.
 */
export function CurlButton({ request, keyName, disabled, title }: { request: () => CurlRequest | null; keyName?: string; disabled?: boolean; title?: string }) {
  const [shown, setShown] = useState<CurlRequest | null>(null);
  return (
    <>
      <Button onClick={() => setShown(request())} disabled={disabled} title={title ?? "The request as a curl command."}>
        Show curl
      </Button>
      {shown && <CurlDialog request={shown} keyName={keyName} onClose={() => setShown(null)} />}
    </>
  );
}

/**
 * The dialog of `CurlButton`, for opening it from elsewhere, such as a menu. `note` replaces the
 * line on which key `$GATEWAY_API_KEY` stands for.
 */
export function CurlDialog({ request, keyName, note, onClose }: { request: CurlRequest; keyName?: string; note?: ReactNode; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  const curl = curlCommand(request);

  const copy = async () => {
    await navigator.clipboard.writeText(curl);
    setCopied(true);
  };

  return (
    <Modal
      title="Request as curl"
      size="wide"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Close</Button>
          <Button variant="primary" onClick={copy}>
            {copied ? "Copied" : "Copy"}
          </Button>
        </>
      }
    >
      <p className="muted small">
        {note ?? <>Set <code>GATEWAY_API_KEY</code> to the secret of {keyName ? <>key <strong>{keyName}</strong></> : "the virtual key"}.</>}
        {curl !== abridged(curl) && " Attached files are shortened here; Copy copies them in full."}
      </p>
      <pre className="raw curl">{abridged(curl)}</pre>
    </Modal>
  );
}
