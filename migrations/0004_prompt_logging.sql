-- Prompt logging, a debugging aid: full request and response bodies of the keys it is turned on
-- for. A project denies, allows or forces it for its keys; a key opts in where its project allows
-- it. Keys outside projects follow the gateway setting `prompt_logging`, which cannot force.
ALTER TABLE projects ADD COLUMN prompt_logging TEXT NOT NULL DEFAULT 'deny'
    CHECK (prompt_logging IN ('deny', 'allow', 'force'));
ALTER TABLE virtual_keys ADD COLUMN prompt_logging BOOLEAN NOT NULL DEFAULT false;
