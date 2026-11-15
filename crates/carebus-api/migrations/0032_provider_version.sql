-- A monotonic version per provider row, so a reader can cache the client it built for a provider by
-- (id, version) and never replace it with a value read before a concurrent admin's write committed.
--
-- `updated_at` is the transaction's start time, so two updates that overlap can commit out of order
-- and a reader could cache the older value last. `version` is incremented inside the UPDATE itself,
-- so the row's value is ordered by commit; a cache entry is replaced only when the number grows.
--
-- Forward-only and drift-tolerant as 0015/0020/0031: IF NOT EXISTS, no backfill (0 is "never
-- written since this migration", which is exactly what a pre-existing row is).

ALTER TABLE llm_providers ADD COLUMN IF NOT EXISTS version bigint NOT NULL DEFAULT 0;
