-- A knowledge store the customer chat must never search (the website's legal pages: ingested, never answered
-- from). Tenant rows only: a base store's release supplies its own reach, and a tenant row that overrides a base
-- store still may exclude it.
ALTER TABLE knowledge_stores
    ADD COLUMN chat_excluded boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT knowledge_stores_chat_excluded_base_check
        CHECK (origin = 'tenant' OR NOT chat_excluded);

-- 0010's view expanded `w.*` at creation, and its computed `overridden` column comes after the table's own
-- columns: a column appended to the table would land BEFORE `overridden`, which CREATE OR REPLACE VIEW cannot
-- accept (it may only append). No other view reads this one, so recreate it and restore 0010's grant.
DROP VIEW eff_knowledge_stores;
CREATE VIEW eff_knowledge_stores WITH (security_invoker = true) AS
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
-- 0010's loop did exactly this pair: the migration role's default privileges hand carebus_app ALL on new
-- objects, so REVOKE first (a plain GRANT SELECT would leave INSERT behind); the view is read-only.
REVOKE ALL ON eff_knowledge_stores FROM carebus_app;
GRANT SELECT ON eff_knowledge_stores TO carebus_app;
