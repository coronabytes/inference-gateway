-- An `oauth_user` upstream may keep a sign-in per tenant, or per tenant and user, within a key,
-- told apart by the request labels its OAuth client names (`sign_in_by`). `labels` holds those
-- labels as `key=value` pairs, sorted by key and joined with commas; it is empty for a sign-in of
-- the whole key.
ALTER TABLE oauth_tokens ADD COLUMN labels TEXT NOT NULL DEFAULT '';
ALTER TABLE oauth_tokens DROP CONSTRAINT oauth_tokens_pkey;
ALTER TABLE oauth_tokens ADD PRIMARY KEY (upstream_kind, upstream_id, key_id, labels);
