-- The refresh token a grant's current one replaced. A client never presents it again (it got a new
-- one), so a replay means someone else has it, and the grant is revoked. A grant also ends 90 days
-- after it was approved, however often its tokens are refreshed (see `rotate_client_grant`).
ALTER TABLE client_grants ADD COLUMN previous_refresh_hash BYTEA;
CREATE INDEX client_grants_previous_refresh ON client_grants (previous_refresh_hash) WHERE previous_refresh_hash IS NOT NULL;
