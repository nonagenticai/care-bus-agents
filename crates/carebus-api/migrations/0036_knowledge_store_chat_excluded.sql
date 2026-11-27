-- A knowledge store the customer chat must never search (the website's legal pages: ingested, never answered
-- from). Tenant rows only: a base store's release supplies its own reach, and a tenant row that overrides a base
-- store still may exclude it.
ALTER TABLE knowledge_stores
    ADD COLUMN chat_excluded boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT knowledge_stores_chat_excluded_base_check
        CHECK (origin = 'tenant' OR NOT chat_excluded);

-- 0010's view expanded `w.*` at creation, so a column added to the table afterwards is invisible through it.
-- Re-create it (the new column lands last, the order CREATE OR REPLACE VIEW accepts) so `SELECT *` and the
-- knowledge routes see `chat_excluded`; the SELECT grant on the view survives the replace.
CREATE OR REPLACE VIEW eff_knowledge_stores WITH (security_invoker = true) AS
SELECT w.* FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.key) t.*,
           (t.origin = 'tenant' AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.key) > 1) AS overridden
      FROM knowledge_stores t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.key, (t.origin = 'tenant') DESC
) w
WHERE NOT w.hidden;
