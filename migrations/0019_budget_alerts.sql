-- Outgoing mail (Cluster → Email), for budget alerts. `password` is sealed with the encryption key
-- like other secrets. `console_url` is how the people who get the emails reach the console.
CREATE TABLE smtp_settings (
    id              SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    enabled         BOOLEAN NOT NULL DEFAULT false,
    host            TEXT NOT NULL,
    port            INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    -- starttls: upgrade a plain connection (port 587); tls: TLS from the start (465); none: plain.
    security        TEXT NOT NULL CHECK (security IN ('starttls', 'tls', 'none')),
    username        TEXT,
    password        BYTEA,
    from_address    TEXT NOT NULL,
    from_name       TEXT,
    console_url     TEXT,
    updated_at      TIMESTAMPTZ NOT NULL
);

-- Who is emailed when a project's or a key's spend in its budget period crosses a threshold
-- (percent of the budget). Recipients are people who signed in (admin_users); their email at the
-- time of sending is used, and people removed since are skipped.
CREATE TABLE budget_alerts (
    project_id  UUID UNIQUE REFERENCES projects (id) ON DELETE CASCADE,
    key_id      UUID UNIQUE REFERENCES virtual_keys (id) ON DELETE CASCADE,
    thresholds  SMALLINT[] NOT NULL,
    recipients  UUID[] NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    CHECK ((project_id IS NULL) <> (key_id IS NULL))
);

-- Alerts sent, so each goes out once per budget period, whichever replica checks. `spender` is
-- `project:<id>` or `key:<id>`, `window_id` the budget period (`d:2026-11-04`, `w:2026-W45`,
-- `m:2026-11`). Reset spend removes the current period's rows; old rows are pruned.
CREATE TABLE budget_alerts_sent (
    spender     TEXT NOT NULL,
    window_id   TEXT NOT NULL,
    threshold   SMALLINT NOT NULL,
    sent_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (spender, window_id, threshold)
);
CREATE INDEX budget_alerts_sent_at ON budget_alerts_sent (sent_at);
