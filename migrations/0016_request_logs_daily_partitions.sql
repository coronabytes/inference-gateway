-- The request log in daily partitions (UTC days), so retention drops whole days instead of deleting
-- rows, and queries over a time range read only the days in it.
--
-- The existing table is not copied: it becomes the first partition (request_logs_legacy), holding
-- everything up to the end of the day of its newest entry. Retention deletes from it as before
-- and drops it once its last day has expired. The gateway creates the daily partitions ahead of
-- time (`gw_request_log_partitions`, at startup and hourly) and drops expired ones
-- (`gw_drop_request_log_partitions`).

ALTER TABLE request_bodies DROP CONSTRAINT request_bodies_id_fkey;

ALTER TABLE request_logs RENAME TO request_logs_legacy;
ALTER INDEX request_logs_pkey RENAME TO request_logs_legacy_pkey;
ALTER INDEX request_logs_ts_idx RENAME TO request_logs_legacy_ts_idx;
ALTER INDEX request_logs_key_ts_idx RENAME TO request_logs_legacy_key_ts_idx;
ALTER INDEX request_logs_model_ts_idx RENAME TO request_logs_legacy_model_ts_idx;
ALTER INDEX request_logs_parent_idx RENAME TO request_logs_legacy_parent_idx;
ALTER INDEX request_logs_project_ts RENAME TO request_logs_legacy_project_ts;
ALTER INDEX request_logs_labels_idx RENAME TO request_logs_legacy_labels_idx;
ALTER INDEX request_logs_labelled_ts_idx RENAME TO request_logs_legacy_labelled_ts_idx;
ALTER INDEX IF EXISTS request_logs_errors_ts_idx RENAME TO request_logs_legacy_errors_ts_idx;

CREATE TABLE request_logs (LIKE request_logs_legacy INCLUDING DEFAULTS INCLUDING COMMENTS INCLUDING COMPRESSION INCLUDING STORAGE)
    PARTITION BY RANGE (ts);
-- A partitioned table's keys include its partition key.
ALTER TABLE request_logs ADD PRIMARY KEY (id, ts);
ALTER TABLE request_logs_legacy DROP CONSTRAINT request_logs_legacy_pkey;
ALTER TABLE request_logs_legacy ADD CONSTRAINT request_logs_legacy_pkey PRIMARY KEY (id, ts);

DO $$
DECLARE
    newest TIMESTAMPTZ := (SELECT max(ts) FROM request_logs_legacy);
    until TIMESTAMPTZ := (date_trunc('day', greatest(coalesce(newest, now()), now()) AT TIME ZONE 'UTC') + interval '1 day') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE request_logs ATTACH PARTITION request_logs_legacy FOR VALUES FROM (MINVALUE) TO (%L)', until);
END $$;

-- The same indexes as before; the legacy partition's own are attached to them, not rebuilt.
CREATE INDEX request_logs_ts_idx ON request_logs (ts DESC);
CREATE INDEX request_logs_key_ts_idx ON request_logs (key_id, ts DESC);
CREATE INDEX request_logs_model_ts_idx ON request_logs (model_name, ts DESC);
CREATE INDEX request_logs_parent_idx ON request_logs (parent_id) WHERE parent_id IS NOT NULL;
CREATE INDEX request_logs_project_ts ON request_logs (project_id, ts) WHERE project_id IS NOT NULL;
CREATE INDEX request_logs_labels_idx ON request_logs USING GIN (labels jsonb_path_ops) WHERE labels IS NOT NULL;
CREATE INDEX request_logs_labelled_ts_idx ON request_logs (ts DESC) WHERE labels IS NOT NULL;
CREATE INDEX request_logs_errors_ts_idx ON request_logs (ts DESC) WHERE status_code >= 400;

-- Bodies belong to their entry by id and time (the entry's ts, which they keep).
ALTER TABLE request_bodies
    ADD CONSTRAINT request_bodies_entry_fkey FOREIGN KEY (id, ts) REFERENCES request_logs (id, ts) ON DELETE CASCADE;

-- Creates the daily partitions from today to `ahead` days on that do not exist yet; days the legacy
-- partition still covers are skipped. Returns how many it created. Safe to run on several
-- replicas at once.
CREATE FUNCTION gw_request_log_partitions(ahead INTEGER) RETURNS INTEGER LANGUAGE plpgsql AS $$
DECLARE
    today DATE := (now() AT TIME ZONE 'UTC')::date;
    day DATE;
    made INTEGER := 0;
BEGIN
    FOR day IN SELECT generate_series(today, today + ahead, interval '1 day')::date LOOP
        CONTINUE WHEN to_regclass(format('request_logs_%s', to_char(day, 'YYYYMMDD'))) IS NOT NULL;
        BEGIN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF request_logs FOR VALUES FROM (%L) TO (%L)',
                format('request_logs_%s', to_char(day, 'YYYYMMDD')),
                day::timestamp AT TIME ZONE 'UTC',
                (day + 1)::timestamp AT TIME ZONE 'UTC'
            );
            made := made + 1;
        EXCEPTION
            -- Covered by the legacy partition, or created by another replica meanwhile.
            WHEN invalid_object_definition OR duplicate_table THEN NULL;
        END;
    END LOOP;
    RETURN made;
END $$;

-- Drops the partitions whose days all end at or before `before`, with the logged bodies of their
-- entries. Returns how many entries they held.
CREATE FUNCTION gw_drop_request_log_partitions(before TIMESTAMPTZ) RETURNS BIGINT LANGUAGE plpgsql AS $$
DECLARE
    part REGCLASS;
    upper TIMESTAMPTZ;
    n BIGINT;
    dropped BIGINT := 0;
BEGIN
    FOR part, upper IN
        SELECT c.oid::regclass, substring(pg_get_expr(c.relpartbound, c.oid) FROM $r$TO \('([^']+)'\)$r$)::timestamptz
        FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
        WHERE i.inhparent = 'request_logs'::regclass
        ORDER BY 2
    LOOP
        CONTINUE WHEN upper IS NULL OR upper > before;
        EXECUTE format('SELECT count(*) FROM %s', part) INTO n;
        DELETE FROM request_bodies WHERE ts < upper;
        EXECUTE format('ALTER TABLE request_logs DETACH PARTITION %s', part);
        EXECUTE format('DROP TABLE %s', part);
        dropped := dropped + n;
    END LOOP;
    RETURN dropped;
END $$;

SELECT gw_request_log_partitions(7);
