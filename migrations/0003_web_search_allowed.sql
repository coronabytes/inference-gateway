-- Whether requests may ask the model to search the web, which the provider bills per search. A
-- project that forbids it forbids it for each of its keys, whatever the key's own says.
ALTER TABLE projects ADD COLUMN web_search BOOLEAN NOT NULL DEFAULT true;
ALTER TABLE virtual_keys ADD COLUMN web_search BOOLEAN NOT NULL DEFAULT true;
