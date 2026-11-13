CREATE TYPE model_use AS ENUM ('generation', 'routing', 'embeddings', 'rerank', 'vision');
CREATE TYPE ontology_scope AS ENUM ('generic', 'industry', 'industry_dept');
CREATE TYPE version_status AS ENUM ('DRAFT', 'CANARY', 'PUBLISHED', 'ARCHIVED', 'DISCARDED');

CREATE TABLE model_routes (
    use_name        model_use PRIMARY KEY,
    provider        text NOT NULL,                 -- e.g. vllm, laya, anthropic, openai, voyageai, strata_rag
    model           text NOT NULL,
    base_url        text,
    secret_ref      text,                          -- name of a k8s Secret key, never a value
    region          text NOT NULL DEFAULT 'eu',
    timeout_ms      integer NOT NULL DEFAULT 60000 CHECK (timeout_ms BETWEEN 100 AND 600000),
    fallback        jsonb NOT NULL DEFAULT '[]',   -- [{provider, model}]
    pii_redaction   boolean NOT NULL DEFAULT true,
    monthly_cap_eur numeric(10,2),
    options         jsonb NOT NULL DEFAULT '{}',   -- per-use extras: dimension, max_image_px, strip_exif …
    updated_at      timestamptz NOT NULL DEFAULT now(),
    updated_by      text
);

CREATE TABLE connectors (
    id          text PRIMARY KEY,                  -- e.g. crm-core, oms, carrier-postnl, psp
    name        text NOT NULL,
    system      text NOT NULL,
    base_url    text NOT NULL,
    auth_type   text NOT NULL,                     -- oauth2_client_credentials | api_key | mtls_api_key | none
    secret_ref  text,
    timeout_ms  integer NOT NULL DEFAULT 3000,
    rate_limit  text,
    health      text NOT NULL DEFAULT 'unknown' CHECK (health IN ('unknown', 'ok', 'down')),  -- result of the last POST /test
    health_checked_at timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE connector_field_mappings (
    connector_id  text NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
    backend_field text NOT NULL,
    attribute     text NOT NULL,                   -- Entity.attribute, e.g. Order.order_id
    PRIMARY KEY (connector_id, backend_field)
);

CREATE TABLE context_spaces (
    key          text PRIMARY KEY,                 -- retail:order-support:returns
    area         text NOT NULL,                    -- order-support | logistics | payments | faq
    label        text NOT NULL,
    request_type text NOT NULL UNIQUE,
    required     text[] NOT NULL,
    entities     text[] NOT NULL,
    procedure    text NOT NULL
);

CREATE TABLE ontology_entities (
    name       text PRIMARY KEY,
    scope      ontology_scope NOT NULL,
    industry   text,
    department text,
    label      text NOT NULL,
    synonyms   text[] NOT NULL DEFAULT '{}',
    hidden     boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE ontology_attributes (
    entity    text NOT NULL REFERENCES ontology_entities(name) ON DELETE CASCADE,
    name      text NOT NULL,
    data_type text NOT NULL,
    pii       boolean NOT NULL DEFAULT false,
    synonyms  text[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (entity, name)
);
CREATE TABLE ontology_relations (
    subject     text NOT NULL REFERENCES ontology_entities(name) ON DELETE CASCADE,
    predicate   text NOT NULL,
    object      text NOT NULL REFERENCES ontology_entities(name) ON DELETE CASCADE,
    cardinality text NOT NULL,
    PRIMARY KEY (subject, predicate, object)
);

-- Mirrors procedure_management agentic_procedures / agentic_procedure_steps.
CREATE TABLE procedures (
    id                 bigserial PRIMARY KEY,
    name               text NOT NULL,
    version            text NOT NULL,
    version_status     version_status NOT NULL DEFAULT 'DRAFT',
    context_space      text NOT NULL REFERENCES context_spaces(key),
    domain             text NOT NULL DEFAULT 'customer_care',
    description        text,
    pddl_domain        text,
    tool_requirements  text[] NOT NULL DEFAULT '{}',
    success_criteria   text[] NOT NULL DEFAULT '{}',
    reviewers          text[] NOT NULL DEFAULT '{}',
    plan               jsonb NOT NULL DEFAULT '{"phases":[]}',   -- carebus::domain::Plan
    -- numeric ordering key: '1.4.0-beta' -> {1,4,0}; the pre-release suffix is ignored. Sort with ORDER BY version_sort DESC, id DESC.
    version_sort       int[] GENERATED ALWAYS AS (string_to_array(regexp_replace(version, '-.*$', ''), '.')::int[]) STORED,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (name, version)
);
CREATE UNIQUE INDEX procedures_one_published ON procedures (name) WHERE version_status = 'PUBLISHED';
-- At most one PUBLISHED procedure per context space: HttpProcedures serves items[0] of that space.
CREATE UNIQUE INDEX procedures_one_published_per_space ON procedures (context_space) WHERE version_status = 'PUBLISHED';
CREATE TABLE procedure_steps (
    procedure_id    bigint NOT NULL REFERENCES procedures(id) ON DELETE CASCADE,
    step_number     integer NOT NULL,
    phase           integer NOT NULL,
    name            text NOT NULL,
    action_type     text NOT NULL,                -- PDDL action, e.g. create-rma
    worker          text NOT NULL,
    mcp_tool        text NOT NULL,                -- implementedByTool, e.g. order_create_rma
    performed_by    text NOT NULL,                -- performedByRole
    preconditions   text[] NOT NULL DEFAULT '{}',
    postconditions  text[] NOT NULL DEFAULT '{}',
    parameters      jsonb NOT NULL DEFAULT '{}',
    gate            boolean NOT NULL DEFAULT false,
    PRIMARY KEY (procedure_id, step_number)
);
CREATE TABLE procedure_executions (
    case_id      text PRIMARY KEY,
    procedure_id bigint NOT NULL REFERENCES procedures(id),
    started_at   timestamptz NOT NULL,
    finished_at  timestamptz,
    outcome      text
);

-- Same columns as enterprise_mcp_server audit_logs, except actor_id is the Keycloak subject text.
CREATE TABLE audit_events (
    id            bigserial PRIMARY KEY,
    timestamp     timestamptz NOT NULL DEFAULT now(),
    actor_id      text NOT NULL,
    actor_type    text NOT NULL CHECK (actor_type IN ('human', 'ai_agent', 'system')),
    action_type   text NOT NULL,
    resource_type text NOT NULL,
    resource_id   text NOT NULL,
    status        text NOT NULL CHECK (status IN ('success', 'failure')),
    details       jsonb NOT NULL DEFAULT '{}',
    request_id    text,
    ip_address    text
);
CREATE INDEX audit_events_ts ON audit_events (timestamp DESC, id DESC);
CREATE INDEX audit_events_tool ON audit_events (resource_type, resource_id, timestamp DESC);
-- one tool-execution audit row per request_id; the recorder inserts with ON CONFLICT DO NOTHING
CREATE UNIQUE INDEX audit_events_execute_request_id ON audit_events (request_id) WHERE action_type = 'execute';

CREATE TABLE case_history (
    case_id         text PRIMARY KEY,
    conversation_id text,
    customer_id     text NOT NULL,
    request_type    text NOT NULL,
    area            text NOT NULL,
    context_space   text NOT NULL,
    outcome         text,                          -- completed | not_eligible | failed; NULL while open
    reason          text,
    work_items      integer NOT NULL DEFAULT 0,
    retries         integer NOT NULL DEFAULT 0,
    started_at      timestamptz NOT NULL,
    resolved_at     timestamptz,
    duration_ms     bigint
);
CREATE INDEX case_history_started ON case_history (started_at DESC, case_id DESC);
