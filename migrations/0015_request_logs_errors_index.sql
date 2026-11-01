-- no-transaction
-- The request log's failed entries, newest first, for the Logs page's errors-only view (see
-- db::error_logs): without it, Postgres walks every entry by time until it has found enough
-- failures. Built concurrently so a large log keeps taking writes meanwhile.
CREATE INDEX CONCURRENTLY IF NOT EXISTS request_logs_errors_ts_idx ON request_logs (ts DESC) WHERE status_code >= 400;
