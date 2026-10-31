-- When a sign-in to an `oauth_user` upstream was last used, at most an hour off: sign-ins unused for
-- longer than the upstream's `sign_in_retention_days` (7 by default) are deleted.
ALTER TABLE oauth_tokens ADD COLUMN last_used_at TIMESTAMPTZ NOT NULL DEFAULT now();
