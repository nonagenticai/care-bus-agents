-- W3 plan 10 Task 1: tenants, users, jobs, and a tenant_id + job_id on every table.
-- care-bus never mints tenant ids: a tenant arrives by provision-down. Existing rows are adopted into the bootstrap
-- tenant 0195f48b-d000-7000-8000-000000000099 (the CAREBUS_TENANT_ID value strata_rag/ingest already hold care-bus's
-- documents under) and the bootstrap job `customer_care_agent`, so the live flow keeps working without a reseed.
-- RLS, the runtime role and the GUC column defaults follow in 0007.

CREATE TABLE tenants (
    id         uuid PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    name       text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 120),
    status     text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by text NOT NULL
);

-- PK on `sub` alone: a Keycloak subject belongs to exactly one tenant.
CREATE TABLE users (
    sub          text PRIMARY KEY CHECK (btrim(sub) <> ''),
    tenant_id    uuid NOT NULL REFERENCES tenants (id),
    tenant_role  text NOT NULL CHECK (tenant_role IN ('tenant_admin', 'tenant_user')),
    display_name text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text NOT NULL
);
CREATE INDEX users_tenant ON users (tenant_id);

-- The external job id is `key`; `id` is care-bus's own. UNIQUE (tenant_id, id) is the target of every composite FK.
CREATE TABLE jobs (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES tenants (id),
    key        text NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]{0,62}$'),
    label      text NOT NULL CHECK (length(btrim(label)) BETWEEN 1 AND 120),
    settings   jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(settings) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, key),
    UNIQUE (tenant_id, id)
);

-- A super_admin's active tenant (X-Active-Tenant-Id), read and written only through a SECURITY DEFINER function (0007).
CREATE TABLE super_admin_active (
    sub         text PRIMARY KEY CHECK (btrim(sub) <> ''),
    tenant_id   uuid NOT NULL REFERENCES tenants (id),
    switched_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO tenants (id, name, updated_by)
VALUES ('0195f48b-d000-7000-8000-000000000099', 'Bootstrap (adopted)', 'migration:0006');
INSERT INTO jobs (tenant_id, key, label)
VALUES ('0195f48b-d000-7000-8000-000000000099', 'customer_care_agent', 'Customer care');

-- Adopt every existing row into the bootstrap pair. audit_events.job_id stays nullable: tenant-level actions
-- (tenant switch, user provisioning) have no job.
DO $$
DECLARE
    boot_tenant constant uuid := '0195f48b-d000-7000-8000-000000000099';
    boot_job uuid := (SELECT id FROM jobs WHERE tenant_id = boot_tenant AND key = 'customer_care_agent');
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['model_routes', 'connectors', 'connector_field_mappings', 'context_spaces',
                             'ontology_entities', 'ontology_attributes', 'ontology_relations', 'procedures',
                             'procedure_steps', 'procedure_executions', 'audit_events', 'case_history',
                             'sources', 'source_runs'] LOOP
        EXECUTE format('ALTER TABLE %I ADD COLUMN tenant_id uuid, ADD COLUMN job_id uuid', t);
        EXECUTE format('UPDATE %I SET tenant_id = $1, job_id = $2', t) USING boot_tenant, boot_job;
        EXECUTE format('ALTER TABLE %I ALTER COLUMN tenant_id SET NOT NULL', t);
        IF t <> 'audit_events' THEN
            EXECUTE format('ALTER TABLE %I ALTER COLUMN job_id SET NOT NULL', t);
        END IF;
        EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)',
                       t, t || '_job_fkey');
    END LOOP;
END $$;
-- (tenant_id, NULL) skips the composite FK (MATCH SIMPLE), so the tenant is checked on its own.
ALTER TABLE audit_events ADD CONSTRAINT audit_events_tenant_fkey FOREIGN KEY (tenant_id) REFERENCES tenants (id);

-- Child-to-parent FKs first: they depend on the global keys being replaced below.
ALTER TABLE connector_field_mappings DROP CONSTRAINT connector_field_mappings_connector_id_fkey;
ALTER TABLE ontology_attributes DROP CONSTRAINT ontology_attributes_entity_fkey;
ALTER TABLE ontology_relations DROP CONSTRAINT ontology_relations_subject_fkey,
                               DROP CONSTRAINT ontology_relations_object_fkey;
ALTER TABLE procedures DROP CONSTRAINT procedures_context_space_fkey;
ALTER TABLE procedure_steps DROP CONSTRAINT procedure_steps_procedure_id_fkey;
ALTER TABLE procedure_executions DROP CONSTRAINT procedure_executions_procedure_id_fkey;
ALTER TABLE source_runs DROP CONSTRAINT source_runs_source_id_fkey;

-- Keys scoped to (tenant_id, job_id).
ALTER TABLE model_routes DROP CONSTRAINT model_routes_pkey,
                         ADD PRIMARY KEY (tenant_id, job_id, use_name);

ALTER TABLE connectors DROP CONSTRAINT connectors_pkey,
                       ADD PRIMARY KEY (tenant_id, job_id, id);
ALTER TABLE connector_field_mappings DROP CONSTRAINT connector_field_mappings_pkey,
    ADD PRIMARY KEY (tenant_id, job_id, connector_id, backend_field),
    ADD CONSTRAINT connector_field_mappings_connector_fkey FOREIGN KEY (tenant_id, job_id, connector_id)
        REFERENCES connectors (tenant_id, job_id, id) ON DELETE CASCADE;

ALTER TABLE context_spaces DROP CONSTRAINT context_spaces_pkey,
                           DROP CONSTRAINT context_spaces_request_type_key,
                           ADD PRIMARY KEY (tenant_id, job_id, key),
                           ADD CONSTRAINT context_spaces_request_type_key UNIQUE (tenant_id, job_id, request_type);

ALTER TABLE ontology_entities DROP CONSTRAINT ontology_entities_pkey,
                              ADD PRIMARY KEY (tenant_id, job_id, name);
ALTER TABLE ontology_attributes DROP CONSTRAINT ontology_attributes_pkey,
    ADD PRIMARY KEY (tenant_id, job_id, entity, name),
    ADD CONSTRAINT ontology_attributes_entity_fkey FOREIGN KEY (tenant_id, job_id, entity)
        REFERENCES ontology_entities (tenant_id, job_id, name) ON DELETE CASCADE;
ALTER TABLE ontology_relations DROP CONSTRAINT ontology_relations_pkey,
    ADD PRIMARY KEY (tenant_id, job_id, subject, predicate, object),
    ADD CONSTRAINT ontology_relations_subject_fkey FOREIGN KEY (tenant_id, job_id, subject)
        REFERENCES ontology_entities (tenant_id, job_id, name) ON DELETE CASCADE,
    ADD CONSTRAINT ontology_relations_object_fkey FOREIGN KEY (tenant_id, job_id, object)
        REFERENCES ontology_entities (tenant_id, job_id, name) ON DELETE CASCADE;

-- procedures keeps its bigserial id PK; children reference (tenant_id, job_id, id) so a row cannot point across scopes.
ALTER TABLE procedures DROP CONSTRAINT procedures_name_version_key,
    ADD CONSTRAINT procedures_scope_id_key UNIQUE (tenant_id, job_id, id),
    ADD CONSTRAINT procedures_name_version_key UNIQUE (tenant_id, job_id, name, version),
    ADD CONSTRAINT procedures_context_space_fkey FOREIGN KEY (tenant_id, job_id, context_space)
        REFERENCES context_spaces (tenant_id, job_id, key);
DROP INDEX procedures_one_published;
DROP INDEX procedures_one_published_per_space;
CREATE UNIQUE INDEX procedures_one_published ON procedures (tenant_id, job_id, name)
    WHERE version_status = 'PUBLISHED';
CREATE UNIQUE INDEX procedures_one_published_per_space ON procedures (tenant_id, job_id, context_space)
    WHERE version_status = 'PUBLISHED';
ALTER TABLE procedure_steps ADD CONSTRAINT procedure_steps_procedure_fkey FOREIGN KEY (tenant_id, job_id, procedure_id)
    REFERENCES procedures (tenant_id, job_id, id) ON DELETE CASCADE;
ALTER TABLE procedure_executions ADD CONSTRAINT procedure_executions_procedure_fkey
    FOREIGN KEY (tenant_id, job_id, procedure_id) REFERENCES procedures (tenant_id, job_id, id);

ALTER TABLE sources DROP CONSTRAINT sources_name_key,
                    ADD CONSTRAINT sources_name_key UNIQUE (tenant_id, job_id, name),
                    ADD CONSTRAINT sources_scope_id_key UNIQUE (tenant_id, job_id, id);
ALTER TABLE source_runs ADD CONSTRAINT source_runs_source_fkey FOREIGN KEY (tenant_id, job_id, source_id)
    REFERENCES sources (tenant_id, job_id, id) ON DELETE CASCADE;

-- One execute audit row per request_id within a tenant; list/window indexes lead with the scope.
DROP INDEX audit_events_execute_request_id;
CREATE UNIQUE INDEX audit_events_execute_request_id ON audit_events (tenant_id, request_id)
    WHERE action_type = 'execute';
DROP INDEX audit_events_ts;
CREATE INDEX audit_events_ts ON audit_events (tenant_id, timestamp DESC, id DESC);
DROP INDEX case_history_started;
CREATE INDEX case_history_started ON case_history (tenant_id, job_id, started_at DESC, case_id DESC);
DROP INDEX case_history_procedure;
CREATE INDEX case_history_procedure ON case_history (tenant_id, job_id, procedure_name, procedure_version, started_at DESC)
    WHERE procedure_name IS NOT NULL;
DROP INDEX sources_target_store;
CREATE INDEX sources_target_store ON sources (tenant_id, job_id, target_store_id);
DROP INDEX source_runs_by_source;
CREATE INDEX source_runs_by_source ON source_runs (tenant_id, job_id, source_id, started_at DESC, id DESC);
