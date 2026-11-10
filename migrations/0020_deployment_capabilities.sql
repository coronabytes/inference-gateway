-- What a deployment can do, in the operator's own words (e.g. eu-hosted, long-context, coding):
-- shown in the console and the model hub, and filtered on there. Unlike tags they grant or limit
-- no access.
ALTER TABLE deployments ADD COLUMN capabilities TEXT[] NOT NULL DEFAULT '{}';
