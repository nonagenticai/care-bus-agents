-- W3 plan 11 Task 2: the effective configuration and the base write guard.
--
-- One view per overlay table: per natural key the tenant row if one exists, else the base row of the job's CURRENT
-- release; a hidden (tombstone) winner is dropped; a child is dropped when its parent is not effective. Every view is
-- `security_invoker`: a plain view runs as its owner, and the owner bypasses RLS on its tables, so a plain view would
-- serve every tenant's rows to carebus_app. `overridden` = the winner is a tenant row and a current base row of the
-- same key exists (the window count runs over the filtered rows before DISTINCT ON picks the winner).
--
-- Base rows are written only by the release apply, which sets the transaction-local GUC `app.release_apply`;
-- restrictive policies refuse every other INSERT/UPDATE/DELETE of an `origin = 'base'` row by carebus_app.

CREATE FUNCTION app_release_apply() RETURNS boolean
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT coalesce(current_setting('app.release_apply', true), '') = 'on' $$;

CREATE VIEW eff_context_spaces WITH (security_invoker = true) AS
SELECT w.* FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.key) t.*,
           (t.origin = 'tenant' AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.key) > 1) AS overridden
      FROM context_spaces t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.key, (t.origin = 'tenant') DESC
) w
WHERE NOT w.hidden;

CREATE VIEW eff_ontology_entities WITH (security_invoker = true) AS
SELECT w.* FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.name) t.*,
           (t.origin = 'tenant' AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.name) > 1) AS overridden
      FROM ontology_entities t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.name, (t.origin = 'tenant') DESC
) w
WHERE NOT w.hidden;

CREATE VIEW eff_ontology_attributes WITH (security_invoker = true) AS
SELECT w.* FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.entity, t.name) t.*,
           (t.origin = 'tenant'
            AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.entity, t.name) > 1) AS overridden
      FROM ontology_attributes t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.entity, t.name, (t.origin = 'tenant') DESC
) w
WHERE NOT w.hidden
  AND EXISTS (SELECT 1 FROM eff_ontology_entities e
               WHERE e.tenant_id = w.tenant_id AND e.job_id = w.job_id AND e.name = w.entity);

CREATE VIEW eff_ontology_relations WITH (security_invoker = true) AS
SELECT w.* FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.subject, t.predicate, t.object) t.*,
           (t.origin = 'tenant'
            AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.subject, t.predicate, t.object) > 1) AS overridden
      FROM ontology_relations t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.subject, t.predicate, t.object, (t.origin = 'tenant') DESC
) w
WHERE NOT w.hidden
  AND EXISTS (SELECT 1 FROM eff_ontology_entities e
               WHERE e.tenant_id = w.tenant_id AND e.job_id = w.job_id AND e.name = w.subject)
  AND EXISTS (SELECT 1 FROM eff_ontology_entities e
               WHERE e.tenant_id = w.tenant_id AND e.job_id = w.job_id AND e.name = w.object);

CREATE VIEW eff_connectors WITH (security_invoker = true) AS
SELECT w.* FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.id) t.*,
           (t.origin = 'tenant' AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.id) > 1) AS overridden
      FROM connectors t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.id, (t.origin = 'tenant') DESC
) w
WHERE NOT w.hidden;

CREATE VIEW eff_connector_field_mappings WITH (security_invoker = true) AS
SELECT w.* FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.connector_id, t.backend_field) t.*,
           (t.origin = 'tenant'
            AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.connector_id, t.backend_field) > 1) AS overridden
      FROM connector_field_mappings t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.connector_id, t.backend_field, (t.origin = 'tenant') DESC
) w
WHERE NOT w.hidden
  AND EXISTS (SELECT 1 FROM eff_connectors c
               WHERE c.tenant_id = w.tenant_id AND c.job_id = w.job_id AND c.id = w.connector_id);

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

-- Procedure versions: natural key (name, version); a version's context space must be effective. A base PUBLISHED
-- version is `shadowed` when a visible tenant PUBLISHED version (on an effective context space) exists for the same
-- name or the same context space; only non-shadowed PUBLISHED versions are served.
CREATE VIEW eff_procedures WITH (security_invoker = true) AS
SELECT w.*,
       (w.origin = 'base' AND w.version_status = 'PUBLISHED' AND EXISTS (
            SELECT 1 FROM procedures p
             WHERE p.tenant_id = w.tenant_id AND p.job_id = w.job_id AND p.origin = 'tenant' AND NOT p.hidden
               AND p.version_status = 'PUBLISHED' AND (p.name = w.name OR p.context_space = w.context_space)
               AND EXISTS (SELECT 1 FROM eff_context_spaces c
                            WHERE c.tenant_id = p.tenant_id AND c.job_id = p.job_id AND c.key = p.context_space)
       )) AS shadowed
  FROM (
    SELECT DISTINCT ON (t.tenant_id, t.job_id, t.name, t.version) t.*,
           (t.origin = 'tenant'
            AND count(*) OVER (PARTITION BY t.tenant_id, t.job_id, t.name, t.version) > 1) AS overridden
      FROM procedures t
     WHERE t.origin = 'tenant'
        OR t.release_id = (SELECT r.release_id FROM job_releases r
                            WHERE r.tenant_id = t.tenant_id AND r.job_id = t.job_id AND r.is_current)
     ORDER BY t.tenant_id, t.job_id, t.name, t.version, (t.origin = 'tenant') DESC
  ) w
 WHERE NOT w.hidden
   AND EXISTS (SELECT 1 FROM eff_context_spaces c
                WHERE c.tenant_id = w.tenant_id AND c.job_id = w.job_id AND c.key = w.context_space);

-- Steps have no overlay of their own: the steps of an effective procedure version.
CREATE VIEW eff_procedure_steps WITH (security_invoker = true) AS
SELECT s.*
  FROM procedure_steps s
 WHERE EXISTS (SELECT 1 FROM eff_procedures p
                WHERE p.tenant_id = s.tenant_id AND p.job_id = s.job_id AND p.origin = s.origin
                  AND p.id = s.procedure_id);

-- Read-only for the runtime role (0007's default privileges would also grant writes on a view).
DO $$
DECLARE
    v text;
BEGIN
    FOREACH v IN ARRAY ARRAY['eff_context_spaces', 'eff_ontology_entities', 'eff_ontology_attributes',
                             'eff_ontology_relations', 'eff_procedures', 'eff_procedure_steps', 'eff_connectors',
                             'eff_connector_field_mappings', 'eff_knowledge_stores'] LOOP
        EXECUTE format('REVOKE ALL ON %I FROM carebus_app', v);
        EXECUTE format('GRANT SELECT ON %I TO carebus_app', v);
    END LOOP;
END $$;

-- Base write guard (defence in depth against a route bug; the owner pool of migrations and fixtures bypasses RLS).
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['context_spaces', 'ontology_entities', 'ontology_attributes', 'ontology_relations',
                             'procedures', 'procedure_steps', 'connectors', 'connector_field_mappings',
                             'knowledge_stores'] LOOP
        EXECUTE format('CREATE POLICY base_guard_ins ON %I AS RESTRICTIVE FOR INSERT '
                       'WITH CHECK (origin = ''tenant'' OR app_release_apply())', t);
        EXECUTE format('CREATE POLICY base_guard_upd ON %I AS RESTRICTIVE FOR UPDATE '
                       'USING (origin = ''tenant'' OR app_release_apply()) '
                       'WITH CHECK (origin = ''tenant'' OR app_release_apply())', t);
        EXECUTE format('CREATE POLICY base_guard_del ON %I AS RESTRICTIVE FOR DELETE '
                       'USING (origin = ''tenant'' OR app_release_apply())', t);
    END LOOP;
END $$;

-- The release ledger is written only by the release apply as well.
CREATE POLICY release_guard_ins ON job_releases AS RESTRICTIVE FOR INSERT WITH CHECK (app_release_apply());
CREATE POLICY release_guard_upd ON job_releases AS RESTRICTIVE FOR UPDATE
    USING (app_release_apply()) WITH CHECK (app_release_apply());
CREATE POLICY release_guard_del ON job_releases AS RESTRICTIVE FOR DELETE USING (app_release_apply());
