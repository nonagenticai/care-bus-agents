-- Server-side search. Each searched table gets one lowered, concatenated column and a trigram GIN
-- index, so `search_text LIKE '%token%'` does not seq-scan. The API strips `%` and `_` from every
-- token before binding it, so a query can never turn into a wildcard scan of its own.
--
-- `ADD COLUMN ... GENERATED ... STORED` rewrites the table once. These are admin-scale tables; if a
-- table ever grows past ~1M rows, replace the generated column with an expression index instead.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

ALTER TABLE case_history
    ADD COLUMN search_text text GENERATED ALWAYS AS (
        lower(
            case_id || ' ' || customer_id || ' ' || request_type || ' ' || area || ' ' ||
            context_space || ' ' || coalesce(outcome, '') || ' ' || coalesce(reason, '')
        )
    ) STORED;
CREATE INDEX case_history_search_trgm ON case_history USING gin (search_text gin_trgm_ops);

ALTER TABLE sources
    ADD COLUMN search_text text GENERATED ALWAYS AS (lower(name || ' ' || type)) STORED;
CREATE INDEX sources_search_trgm ON sources USING gin (search_text gin_trgm_ops);

ALTER TABLE source_runs
    ADD COLUMN search_text text GENERATED ALWAYS AS (
        lower(id::text || ' ' || status || ' ' || started_by || ' ' || coalesce(error, ''))
    ) STORED;
CREATE INDEX source_runs_search_trgm ON source_runs USING gin (search_text gin_trgm_ops);

-- Connectors are searched through `eff_connectors` / the base layer, whose views predate this
-- migration and expand `t.*` at creation time; they cannot see a column added here. Connectors are
-- a tenant's own short list, so the query concatenates the view's columns and no index is needed.
