-- Searches a request was billed for: the web searches a chat model ran, or a rerank model's
-- searches (a query over up to 100 documents). A routed request's entry sums its calls'.
ALTER TABLE request_logs
    ADD COLUMN searches BIGINT NOT NULL DEFAULT 0,
    -- The searches' part of cost_usd, at the deployment's price per 1,000 when the request ran.
    ADD COLUMN search_cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0;
