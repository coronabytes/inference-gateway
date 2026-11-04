import { useContext, useState } from "react";
import { api, type SmtpSettings, type SmtpSettingsInput } from "../api";
import { SessionContext } from "../access";
import { Banner, Button, Segmented, TextInput, Toggle } from "../components/controls";
import { ErrorBanner } from "../components/ui";
import { useAdminData } from "../useAdminData";

type Security = SmtpSettingsInput["security"];

/** The usual port for each kind of connection, filled in when the security changes. */
const PORTS: Record<Security, number> = { starttls: 587, tls: 465, none: 25 };

/** Cluster → Email: the mail server budget alerts are sent through. */
export function Email() {
  const { data: settings, error, setError, reload } = useAdminData<SmtpSettings>("/settings/smtp");
  return (
    <div className="page">
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {settings && <SmtpForm key={settings.updated_at ?? "new"} initial={settings} onSaved={reload} />}
    </div>
  );
}

function SmtpForm({ initial, onSaved }: { initial: SmtpSettings; onSaved: () => Promise<void> }) {
  const session = useContext(SessionContext);
  const [enabled, setEnabled] = useState(initial.configured ? initial.enabled : true);
  const [host, setHost] = useState(initial.host);
  const [port, setPort] = useState(String(initial.port));
  const [security, setSecurity] = useState<Security>(initial.security);
  const [username, setUsername] = useState(initial.username ?? "");
  const [password, setPassword] = useState("");
  const [removePassword, setRemovePassword] = useState(false);
  const [fromAddress, setFromAddress] = useState(initial.from_address);
  const [fromName, setFromName] = useState(initial.from_name ?? "");
  const [consoleUrl, setConsoleUrl] = useState(
    initial.console_url ?? `${window.location.origin}${window.location.pathname}`.replace(/\/+$/, ""),
  );
  const [testTo, setTestTo] = useState(session?.user?.email ?? "");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const portNumber = Number(port);
  const portValid = Number.isInteger(portNumber) && portNumber >= 1 && portNumber <= 65535;
  const incomplete = !host.trim() || !portValid || !fromAddress.trim();

  const changeSecurity = (s: Security) => {
    // A port left at the old default follows the new one; one typed in stays.
    if (Number(port) === PORTS[security]) setPort(String(PORTS[s]));
    setSecurity(s);
  };

  const save = async () => {
    setBusy(true);
    setNotice(null);
    try {
      const input: SmtpSettingsInput = {
        enabled,
        host,
        port: portNumber,
        security,
        username: username.trim() || null,
        ...(password ? { password } : removePassword ? { password: "" } : {}),
        from_address: fromAddress,
        from_name: fromName.trim() || null,
        console_url: consoleUrl.trim() || null,
      };
      await api("/settings/smtp", { method: "PUT", json: input });
      setError(null);
      await onSaved();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const test = async () => {
    setBusy(true);
    setNotice(null);
    try {
      const r = await api<{ ok: boolean; error?: string }>("/settings/smtp/test", { method: "POST", json: { to: testTo.trim() } });
      if (r.ok) {
        setError(null);
        setNotice(`A sample budget alert went to ${testTo.trim()}.`);
      } else setError(r.error ?? "sending failed");
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card form">
      <h3>Email (SMTP)</h3>
      <p className="muted small">
        The mail server budget alerts go out through. Projects and keys choose who gets them, and at which share of their budget, in their
        Budget settings.
      </p>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {notice && (
        <Banner tone="good" caption="Test" onDismiss={() => setNotice(null)}>
          {notice}
        </Banner>
      )}
      <Toggle checked={enabled} onChange={setEnabled}>
        Send budget alerts
      </Toggle>
      <div className="row">
        <TextInput
          caption="Server"
          hint="The mail server's host name or address, without a port."
          mono
          value={host}
          onChange={(e) => setHost(e.target.value)}
          placeholder="e.g. smtp.example.com"
          required
        />
        <TextInput
          caption="Port"
          inputMode="numeric"
          value={port}
          onChange={(e) => setPort(e.target.value)}
          required
          hint="Usually 587 with STARTTLS, 465 with TLS, and 25 for a relay without encryption."
        />
      </div>
      <Segmented
        caption="Security"
        hint="STARTTLS upgrades a plain connection and fails if the server does not offer it. TLS encrypts from the start. None sends in the clear: only for a relay on this machine or a trusted network."
        options={[
          { value: "starttls", label: "STARTTLS" },
          { value: "tls", label: "TLS" },
          { value: "none", label: "None" },
        ]}
        value={security}
        onChange={changeSecurity}
      />
      {security === "none" && <Banner tone="warning">Mail and the password are sent unencrypted.</Banner>}
      <div className="row">
        <TextInput
          caption="Username"
          hint="Leave empty for a server that takes mail without signing in."
          mono
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          placeholder="none"
        />
        <TextInput
          caption="Password"
          hint={
            initial.password_set
              ? "Stored encrypted. Leave empty to keep it, or remove it with ×."
              : "Stored encrypted. For many providers an app password or API key."
          }
          secret
          value={password}
          onChange={(e) => {
            setPassword(e.target.value);
            setRemovePassword(false);
          }}
          placeholder={!initial.password_set ? "" : removePassword ? "removed when you save" : "stored — leave blank to keep"}
          // × clears what was typed, and then removes the stored password.
          {...(password
            ? { onClear: () => setPassword(""), clearLabel: "Clear the new password" }
            : initial.password_set &&
              !removePassword && { onClear: () => setRemovePassword(true), clearLabel: "Remove the stored password" })}
        />
      </div>
      <div className="row">
        <TextInput
          caption="From address"
          hint="The sender of every alert. Many servers accept only addresses of the account they sign in with."
          mono
          value={fromAddress}
          onChange={(e) => setFromAddress(e.target.value)}
          placeholder="e.g. gateway@example.com"
          required
        />
        <TextInput
          caption="From name"
          value={fromName}
          onChange={(e) => setFromName(e.target.value)}
          placeholder="e.g. Inference Gateway"
        />
      </div>
      <TextInput
        caption="Console URL"
        hint="How the people who get the alerts reach this console; the emails link to it."
        mono
        value={consoleUrl}
        onChange={(e) => setConsoleUrl(e.target.value)}
        placeholder="e.g. https://gateway.example.com"
      />
      {initial.configured && (
        <fieldset>
          <legend>Test</legend>
          <p className="muted small">
            Sends a sample budget alert with the saved settings, so you see the format and whether mail gets through.
          </p>
          <div className="row">
            <TextInput caption="Send to" mono value={testTo} onChange={(e) => setTestTo(e.target.value)} placeholder="you@example.com" />
            <div>
              <Button onClick={test} disabled={busy || !testTo.trim()}>
                Send test email
              </Button>
            </div>
          </div>
        </fieldset>
      )}
      <div className="modal-foot">
        <Button variant="primary" onClick={save} disabled={busy || incomplete}>
          Save
        </Button>
      </div>
    </section>
  );
}
