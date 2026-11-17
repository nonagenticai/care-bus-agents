-- W3 plan 11 Task 1: base vs refinement. Every definitional config row carries `origin` ('base' | 'tenant'). Base
-- rows belong to the job's release (`release_id`, `content_hash`) and are replaced only by the release apply; tenant
-- rows refine them by natural key (override / extension / tombstone, `base_hash` = the base row's content_hash when
-- the refinement was written). Natural keys grow `origin`, so a base row and its refinement coexist. The effective
-- views and the base write guard follow in 0010.
--
-- Bootstrap split: in the bootstrap job, rows whose natural key comes from the seed migrations 0002/0004 become base
-- rows of the synthetic release `bootstrap-0009` (release 1 replaces them by natural key); every other row stays
-- tenant (admin-created wiring and extensions survive release 1). A seeded entity that is hidden today becomes a
-- visible base row plus a tenant tombstone.

-- Releases applied to a job. `bundle` is NULL only for the synthetic bootstrap release.
CREATE TABLE job_releases (
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant(),
    job_id           uuid NOT NULL DEFAULT app_current_job(),
    release_id       text NOT NULL CHECK (release_id ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    hash             text NOT NULL,
    bundle           jsonb,
    counts           jsonb NOT NULL DEFAULT '{}',
    is_current       boolean NOT NULL,
    first_applied_at timestamptz NOT NULL DEFAULT now(),
    last_applied_at  timestamptz NOT NULL DEFAULT now(),
    applied_by       text NOT NULL,
    PRIMARY KEY (tenant_id, job_id, release_id),
    CONSTRAINT job_releases_bundle_check CHECK (bundle IS NOT NULL OR release_id = 'bootstrap-0009'),
    CONSTRAINT job_releases_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE UNIQUE INDEX job_releases_one_current ON job_releases (tenant_id, job_id) WHERE is_current;

-- Child-to-parent FKs first: they reference the keys that grow `origin` below.
ALTER TABLE connector_field_mappings DROP CONSTRAINT connector_field_mappings_connector_fkey;
ALTER TABLE ontology_attributes DROP CONSTRAINT ontology_attributes_entity_fkey;
ALTER TABLE ontology_relations DROP CONSTRAINT ontology_relations_subject_fkey,
                               DROP CONSTRAINT ontology_relations_object_fkey;
ALTER TABLE procedures DROP CONSTRAINT procedures_context_space_fkey;
ALTER TABLE procedure_steps DROP CONSTRAINT procedure_steps_procedure_fkey;
ALTER TABLE procedure_executions DROP CONSTRAINT procedure_executions_procedure_fkey;

-- Overlay columns. `origin` defaults to 'tenant': an INSERT that does not name it never creates a base row.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['context_spaces', 'ontology_entities', 'ontology_attributes', 'ontology_relations',
                             'procedures', 'procedure_steps', 'connectors', 'connector_field_mappings'] LOOP
        EXECUTE format('ALTER TABLE %I ADD COLUMN origin text NOT NULL DEFAULT ''tenant'' '
                       'CONSTRAINT %I CHECK (origin IN (''base'', ''tenant'')), '
                       'ADD COLUMN release_id text, ADD COLUMN content_hash text, ADD COLUMN base_hash text',
                       t, t || '_origin_check');
        IF t <> 'ontology_entities' THEN
            EXECUTE format('ALTER TABLE %I ADD COLUMN hidden boolean NOT NULL DEFAULT false', t);
        END IF;
    END LOOP;
END $$;

-- Keys grow `origin` (every row is 'tenant' at this point, so the new keys hold).
ALTER TABLE context_spaces DROP CONSTRAINT context_spaces_pkey,
                           DROP CONSTRAINT context_spaces_request_type_key,
                           ADD PRIMARY KEY (tenant_id, job_id, origin, key),
                           ADD CONSTRAINT context_spaces_request_type_key UNIQUE (tenant_id, job_id, origin, request_type);
ALTER TABLE ontology_entities DROP CONSTRAINT ontology_entities_pkey,
                              ADD PRIMARY KEY (tenant_id, job_id, origin, name);
ALTER TABLE ontology_attributes DROP CONSTRAINT ontology_attributes_pkey,
                                ADD PRIMARY KEY (tenant_id, job_id, origin, entity, name);
ALTER TABLE ontology_relations DROP CONSTRAINT ontology_relations_pkey,
                               ADD PRIMARY KEY (tenant_id, job_id, origin, subject, predicate, object);
-- procedures keeps its bigserial id PK and procedures_scope_id_key (procedure_executions references it).
ALTER TABLE procedures DROP CONSTRAINT procedures_name_version_key,
                       ADD CONSTRAINT procedures_name_version_key UNIQUE (tenant_id, job_id, origin, name, version),
                       ADD CONSTRAINT procedures_origin_id_key UNIQUE (tenant_id, job_id, origin, id);
DROP INDEX procedures_one_published;
DROP INDEX procedures_one_published_per_space;
CREATE UNIQUE INDEX procedures_one_published ON procedures (tenant_id, job_id, origin, name)
    WHERE version_status = 'PUBLISHED';
CREATE UNIQUE INDEX procedures_one_published_per_space ON procedures (tenant_id, job_id, origin, context_space)
    WHERE version_status = 'PUBLISHED';
ALTER TABLE procedure_steps DROP CONSTRAINT procedure_steps_pkey,
                            ADD PRIMARY KEY (tenant_id, job_id, procedure_id, step_number);
ALTER TABLE connectors DROP CONSTRAINT connectors_pkey,
                       ADD PRIMARY KEY (tenant_id, job_id, origin, id);
ALTER TABLE connector_field_mappings DROP CONSTRAINT connector_field_mappings_pkey,
                                     ADD PRIMARY KEY (tenant_id, job_id, origin, connector_id, backend_field);

-- The synthetic bootstrap release, current for the bootstrap job.
INSERT INTO job_releases (tenant_id, job_id, release_id, hash, bundle, is_current, applied_by)
SELECT tenant_id, id, 'bootstrap-0009', 'sha256:bootstrap-0009', NULL, true, 'migration:0009'
  FROM jobs
 WHERE tenant_id = '0195f48b-d000-7000-8000-000000000099' AND key = 'customer_care_agent';

CREATE TEMP TABLE boot_scope ON COMMIT DROP AS
SELECT tenant_id, job_id FROM job_releases WHERE release_id = 'bootstrap-0009';

-- The natural keys of the seed migrations 0002/0004 (keys only, copied from their VALUES lists).
CREATE TEMP TABLE seed_keys (object text NOT NULL, key jsonb NOT NULL) ON COMMIT DROP;
INSERT INTO seed_keys (object, key)
SELECT 'context_space', jsonb_build_object('key', k)
  FROM unnest(ARRAY['retail:order-support:returns', 'retail:order-support:warranty',
                    'retail:logistics:delivery-status', 'retail:payments:payment-errors', 'retail:faq:general']) k;
INSERT INTO seed_keys (object, key)
SELECT 'ontology_entity', jsonb_build_object('name', k)
  FROM unnest(ARRAY['Customer', 'Communication', 'Policy', 'Order', 'Product', 'Payment', 'Shipment',
                    'ReturnAuthorization', 'WarrantyClaim', 'Carrier', 'Refund']) k;
INSERT INTO seed_keys (object, key)
SELECT 'ontology_attribute', jsonb_build_object('entity', e, 'name', n)
  FROM (VALUES
    ('Customer','customer_id'),('Customer','name'),('Customer','email'),('Customer','tier'),
    ('Communication','channel'),('Communication','subject'),('Communication','at'),
    ('Policy','code'),('Policy','text'),
    ('Order','order_id'),('Order','placed'),('Order','amount'),('Order','status'),('Order','shipment_id'),
    ('Product','sku'),('Product','name'),('Product','category'),
    ('Payment','payment_id'),('Payment','method'),('Payment','amount'),('Payment','status'),('Payment','at'),
    ('Shipment','shipment_id'),('Shipment','carrier'),('Shipment','status'),('Shipment','eta'),
    ('ReturnAuthorization','rma_id'),('ReturnAuthorization','reason'),('ReturnAuthorization','deadline'),
    ('WarrantyClaim','claim_id'),('WarrantyClaim','defect'),('WarrantyClaim','warranty_until'),
    ('Carrier','code'),('Carrier','name'),
    ('Refund','refund_id'),('Refund','amount'),('Refund','eta')) v(e, n);
INSERT INTO seed_keys (object, key)
SELECT 'ontology_relation', jsonb_build_object('subject', s, 'predicate', p, 'object', o)
  FROM (VALUES
    ('Customer','places','Order'),('Order','contains','Product'),('Order','paidBy','Payment'),
    ('Order','shippedVia','Shipment'),('Shipment','carriedBy','Carrier'),('ReturnAuthorization','for','Order'),
    ('WarrantyClaim','covers','Product'),('Refund','reverses','Payment'),('Customer','receives','Communication'),
    ('Policy','governs','ReturnAuthorization')) v(s, p, o);
INSERT INTO seed_keys (object, key)
SELECT 'procedure', jsonb_build_object('name', n, 'version', v)
  FROM (VALUES ('return','1.3.0'),('return','1.4.0-beta'),('warranty_claim','1.1.0'),('delivery_status','2.0.0'),
               ('duplicate_charge_refund','1.0.2'),('faq_answer','2.0.0')) x(n, v);

UPDATE context_spaces t SET origin = 'base', release_id = 'bootstrap-0009', content_hash = 'bootstrap'
  FROM boot_scope b
 WHERE t.tenant_id = b.tenant_id AND t.job_id = b.job_id
   AND EXISTS (SELECT 1 FROM seed_keys s WHERE s.object = 'context_space' AND s.key->>'key' = t.key);
UPDATE ontology_entities t SET origin = 'base', release_id = 'bootstrap-0009', content_hash = 'bootstrap'
  FROM boot_scope b
 WHERE t.tenant_id = b.tenant_id AND t.job_id = b.job_id
   AND EXISTS (SELECT 1 FROM seed_keys s WHERE s.object = 'ontology_entity' AND s.key->>'name' = t.name);
UPDATE ontology_attributes t SET origin = 'base', release_id = 'bootstrap-0009', content_hash = 'bootstrap'
  FROM boot_scope b
 WHERE t.tenant_id = b.tenant_id AND t.job_id = b.job_id
   AND EXISTS (SELECT 1 FROM seed_keys s WHERE s.object = 'ontology_attribute'
                  AND s.key->>'entity' = t.entity AND s.key->>'name' = t.name);
UPDATE ontology_relations t SET origin = 'base', release_id = 'bootstrap-0009', content_hash = 'bootstrap'
  FROM boot_scope b
 WHERE t.tenant_id = b.tenant_id AND t.job_id = b.job_id
   AND EXISTS (SELECT 1 FROM seed_keys s WHERE s.object = 'ontology_relation'
                  AND s.key->>'subject' = t.subject AND s.key->>'predicate' = t.predicate
                  AND s.key->>'object' = t.object);
UPDATE procedures t SET origin = 'base', release_id = 'bootstrap-0009', content_hash = 'bootstrap'
  FROM boot_scope b
 WHERE t.tenant_id = b.tenant_id AND t.job_id = b.job_id
   AND EXISTS (SELECT 1 FROM seed_keys s WHERE s.object = 'procedure'
                  AND s.key->>'name' = t.name AND s.key->>'version' = t.version);
-- Steps follow their procedure.
UPDATE procedure_steps s SET origin = p.origin, release_id = p.release_id, content_hash = p.content_hash
  FROM procedures p
 WHERE p.tenant_id = s.tenant_id AND p.job_id = s.job_id AND p.id = s.procedure_id AND p.origin = 'base';

-- A seeded entity hidden today: the base row is visible, the hide becomes a tenant tombstone (full copy).
INSERT INTO ontology_entities (tenant_id, job_id, name, scope, industry, department, label, synonyms, hidden,
                               updated_at, origin, base_hash)
SELECT tenant_id, job_id, name, scope, industry, department, label, synonyms, true, updated_at, 'tenant', content_hash
  FROM ontology_entities
 WHERE origin = 'base' AND hidden;
UPDATE ontology_entities SET hidden = false WHERE origin = 'base' AND hidden;

-- Knowledge stores: an overlay table from the start. strata_store_id is tenant wiring, never part of a base.
CREATE TABLE knowledge_stores (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    key             text NOT NULL CHECK (key ~ '^[a-z][a-z0-9-]{0,62}$'),
    origin          text NOT NULL DEFAULT 'tenant' CONSTRAINT knowledge_stores_origin_check CHECK (origin IN ('base', 'tenant')),
    release_id      text,
    content_hash    text,
    base_hash       text,
    hidden          boolean NOT NULL DEFAULT false,
    label           text NOT NULL,
    parent_key      text,
    kind            text NOT NULL CHECK (kind IN ('group', 'leaf')),
    areas           text[] NOT NULL DEFAULT '{}',
    description     text,
    strata_store_id uuid,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    updated_by      text,
    PRIMARY KEY (tenant_id, job_id, origin, key),
    CONSTRAINT knowledge_stores_store_tenant_only CHECK (origin = 'tenant' OR strata_store_id IS NULL),
    CONSTRAINT knowledge_stores_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);

-- Overlay invariants on the 9 overlay tables. The release FK is MATCH SIMPLE: tenant rows (release_id NULL) skip it.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['context_spaces', 'ontology_entities', 'ontology_attributes', 'ontology_relations',
                             'procedures', 'procedure_steps', 'connectors', 'connector_field_mappings',
                             'knowledge_stores'] LOOP
        EXECUTE format('ALTER TABLE %I '
                       'ADD CONSTRAINT %I CHECK ((origin = ''base'') = (release_id IS NOT NULL)), '
                       'ADD CONSTRAINT %I CHECK ((origin = ''base'') = (content_hash IS NOT NULL)), '
                       'ADD CONSTRAINT %I CHECK (origin = ''tenant'' OR (NOT hidden AND base_hash IS NULL)), '
                       'ADD CONSTRAINT %I FOREIGN KEY (tenant_id, job_id, release_id) '
                       '    REFERENCES job_releases (tenant_id, job_id, release_id)',
                       t, t || '_base_release_check', t || '_base_hash_check', t || '_base_plain_check',
                       t || '_release_fkey');
    END LOOP;
END $$;

-- Children: the parent FK is enforced for base children (a base is complete by construction) and skipped for
-- tenant children (fk_origin NULL, MATCH SIMPLE), which may refine a base parent; the views drop orphans.
ALTER TABLE ontology_attributes
    ADD COLUMN fk_origin text GENERATED ALWAYS AS (CASE WHEN origin = 'base' THEN 'base' END) STORED,
    ADD CONSTRAINT ontology_attributes_entity_fkey FOREIGN KEY (tenant_id, job_id, fk_origin, entity)
        REFERENCES ontology_entities (tenant_id, job_id, origin, name) ON DELETE CASCADE;
ALTER TABLE ontology_relations
    ADD COLUMN fk_origin text GENERATED ALWAYS AS (CASE WHEN origin = 'base' THEN 'base' END) STORED,
    ADD CONSTRAINT ontology_relations_subject_fkey FOREIGN KEY (tenant_id, job_id, fk_origin, subject)
        REFERENCES ontology_entities (tenant_id, job_id, origin, name) ON DELETE CASCADE,
    ADD CONSTRAINT ontology_relations_object_fkey FOREIGN KEY (tenant_id, job_id, fk_origin, object)
        REFERENCES ontology_entities (tenant_id, job_id, origin, name) ON DELETE CASCADE;
ALTER TABLE connector_field_mappings
    ADD COLUMN fk_origin text GENERATED ALWAYS AS (CASE WHEN origin = 'base' THEN 'base' END) STORED,
    ADD CONSTRAINT connector_field_mappings_connector_fkey FOREIGN KEY (tenant_id, job_id, fk_origin, connector_id)
        REFERENCES connectors (tenant_id, job_id, origin, id) ON DELETE CASCADE;
ALTER TABLE procedures
    ADD COLUMN fk_origin text GENERATED ALWAYS AS (CASE WHEN origin = 'base' THEN 'base' END) STORED,
    ADD CONSTRAINT procedures_context_space_fkey FOREIGN KEY (tenant_id, job_id, fk_origin, context_space)
        REFERENCES context_spaces (tenant_id, job_id, origin, key);
-- Steps always share their procedure's origin.
ALTER TABLE procedure_steps
    ADD CONSTRAINT procedure_steps_procedure_fkey FOREIGN KEY (tenant_id, job_id, origin, procedure_id)
        REFERENCES procedures (tenant_id, job_id, origin, id) ON DELETE CASCADE;

-- An execution outlives the version it ran (a release may delete a base version): keep name@version, null the id.
-- SET NULL names the column, or it would null tenant_id/job_id too.
ALTER TABLE procedure_executions
    ALTER COLUMN procedure_id DROP NOT NULL,
    ADD COLUMN procedure_name text,
    ADD COLUMN procedure_version text,
    ADD CONSTRAINT procedure_executions_procedure_fkey FOREIGN KEY (tenant_id, job_id, procedure_id)
        REFERENCES procedures (tenant_id, job_id, id) ON DELETE SET NULL (procedure_id);

-- Connectors: no secret in a base; a base slot may leave base_url to the tenant; a tombstone needs none.
ALTER TABLE connectors
    ALTER COLUMN base_url DROP NOT NULL,
    ADD CONSTRAINT connectors_base_url_check CHECK (origin = 'base' OR hidden OR base_url IS NOT NULL),
    ADD CONSTRAINT connectors_secret_tenant_only CHECK (origin = 'tenant' OR secret_ref IS NULL);

-- Connector health is an observation, not config: POST /connectors/{id}/test must not write a base row.
CREATE TABLE connector_health (
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    connector_id text NOT NULL,
    health       text NOT NULL CHECK (health IN ('unknown', 'ok', 'down')),
    checked_at   timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, job_id, connector_id),
    CONSTRAINT connector_health_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
INSERT INTO connector_health (tenant_id, job_id, connector_id, health, checked_at)
SELECT DISTINCT ON (tenant_id, job_id, id) tenant_id, job_id, id, health, coalesce(health_checked_at, updated_at)
  FROM connectors
 WHERE health <> 'unknown' OR health_checked_at IS NOT NULL
 ORDER BY tenant_id, job_id, id, (origin = 'tenant') DESC;
ALTER TABLE connectors DROP COLUMN health, DROP COLUMN health_checked_at;

-- Tenant-only config: never part of a bundle (BYO / carries credentials).
ALTER TABLE model_routes ADD COLUMN origin text NOT NULL DEFAULT 'tenant'
    CONSTRAINT model_routes_origin_check CHECK (origin = 'tenant');
ALTER TABLE sources ADD COLUMN origin text NOT NULL DEFAULT 'tenant'
    CONSTRAINT sources_origin_check CHECK (origin = 'tenant');

-- Job settings overlay: jobs.settings is the base, settings_override the tenant's
-- {"<setting>": {"value": string|null, "base_hash": "sha256:…"|null}} (value null = hide the base setting).
ALTER TABLE jobs ADD COLUMN settings_override jsonb NOT NULL DEFAULT '{}'
    CONSTRAINT jobs_settings_override_check CHECK (jsonb_typeof(settings_override) = 'object');

-- Conflicts between tenant refinements and the base of a release, recomputed by every apply.
CREATE TABLE release_conflicts (
    id               bigserial PRIMARY KEY,
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant(),
    job_id           uuid NOT NULL DEFAULT app_current_job(),
    release_id       text NOT NULL,
    kind             text NOT NULL CHECK (kind IN ('base_removed', 'base_changed', 'base_added', 'parent_missing',
                                                   'base_shadowed')),
    object           text NOT NULL,
    key              jsonb NOT NULL,
    tenant_row       text NOT NULL CHECK (tenant_row IN ('override', 'extension', 'tombstone')),
    base_hash_before text,
    base_hash_after  text,
    detected_at      timestamptz NOT NULL DEFAULT now(),
    resolved_at      timestamptz,
    resolution       text CHECK (resolution IN ('rebased', 'reverted', 'superseded', 'gone')),
    resolved_by      text,
    CHECK ((resolved_at IS NULL) = (resolution IS NULL)),
    CONSTRAINT release_conflicts_release_fkey FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_releases (tenant_id, job_id, release_id)
);
CREATE INDEX release_conflicts_open ON release_conflicts (tenant_id, job_id) WHERE resolved_at IS NULL;

-- RLS on the new tables exactly as 0007 (grants come from 0007's default privileges).
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['job_releases', 'knowledge_stores', 'connector_health', 'release_conflicts'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
    END LOOP;
END $$;
