-- Spec 2026-10-18 §3: Laya (Jev System-1) gates. Questions per (tenant, job) asked at three points of a turn
-- (`request`: customer message → bus, `tool`: worker → side-effect tool, `response`: resolution → customer), all
-- questions of one point in ONE laya call, and the verdict of every evaluation. RLS and the GUC column defaults as
-- 0007 (tenant isolation + the restrictive job scope); grants come from 0007's default privileges.

CREATE TABLE gate_questions (
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    key          text NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]{0,63}$'),
    point        text NOT NULL CHECK (point IN ('request', 'tool', 'response')),
    "type"       text NOT NULL CHECK ("type" IN ('noul', 'choice', 'score')),
    instructions text NOT NULL CHECK (length(btrim(instructions)) BETWEEN 1 AND 2000),
    criteria     jsonb NOT NULL CHECK (
        ("type" = 'noul' AND jsonb_typeof(criteria) = 'object')
        OR ("type" = 'choice' AND jsonb_typeof(criteria) = 'object')
        OR ("type" = 'score' AND jsonb_typeof(criteria) = 'array' AND jsonb_array_length(criteria) BETWEEN 2 AND 10)),
    fire_when    text NOT NULL CHECK (fire_when IN ('gte', 'lt', 'in')),
    threshold    double precision,
    fire_values  jsonb CHECK (fire_values IS NULL OR jsonb_typeof(fire_values) = 'array'),
    action       text NOT NULL CHECK (action IN ('handoff', 'block', 'annotate')),
    message      text CHECK (message IS NULL OR length(btrim(message)) BETWEEN 1 AND 1000),
    fail         text NOT NULL CHECK (fail IN ('open', 'closed')),
    tools        jsonb CHECK (tools IS NULL OR jsonb_typeof(tools) = 'array'),
    enabled      boolean NOT NULL DEFAULT true,
    ordinal      integer NOT NULL DEFAULT 0 CHECK (ordinal BETWEEN 0 AND 10000),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text NOT NULL,
    PRIMARY KEY (tenant_id, job_id, key),
    CONSTRAINT gate_questions_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    -- `in` names options and has no threshold; `gte`/`lt` compare a threshold and name no options.
    CONSTRAINT gate_questions_rule CHECK (
        (fire_when = 'in' AND "type" = 'choice' AND threshold IS NULL AND fire_values IS NOT NULL)
        OR (fire_when IN ('gte', 'lt') AND threshold IS NOT NULL AND fire_values IS NULL)),
    -- only tool-point questions name tools (NULL there = the side-effect tools)
    CONSTRAINT gate_questions_tools CHECK (point = 'tool' OR tools IS NULL)
);
CREATE INDEX gate_questions_point ON gate_questions (tenant_id, job_id, point, ordinal);

CREATE TABLE gate_verdicts (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    id              bigserial PRIMARY KEY,
    point           text NOT NULL CHECK (point IN ('request', 'tool', 'response')),
    conversation_id text,
    case_id         text,
    outcome         text NOT NULL CHECK (outcome IN ('pass', 'fired', 'failed_open', 'failed_closed', 'skipped')),
    fired_keys      jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(fired_keys) = 'array'),
    action          text CHECK (action IS NULL OR action IN ('handoff', 'block', 'annotate')),
    answers         jsonb,
    latency_ms      integer,
    error           text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT gate_verdicts_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE INDEX gate_verdicts_newest ON gate_verdicts (tenant_id, job_id, created_at DESC, id DESC);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['gate_questions', 'gate_verdicts'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
    END LOOP;
END $$;

-- The retail demo's questions for the bootstrap tenant's customer_care_agent job.
INSERT INTO gate_questions (tenant_id, job_id, key, point, "type", instructions, criteria, fire_when, threshold,
                            fire_values, action, message, fail, tools, enabled, ordinal, updated_by)
SELECT j.tenant_id, j.id, q.key, q.point, q."type", q.instructions, q.criteria::jsonb, q.fire_when, q.threshold,
       q.fire_values::jsonb, q.action, q.message, q.fail, q.tools::jsonb, true, q.ordinal, 'migration:0016'
  FROM jobs j
 CROSS JOIN (VALUES
    ('emergency', 'request', 'noul',
     'A customer of an online electronics and home-goods shop wrote this message. Decide whether it describes an '
     'emergency: a danger to someone''s life or health or a risk of fire, electric shock, gas, smoke, overheating, '
     'or a swollen or leaking battery, happening now or about to happen. A broken or late product on its own is not '
     'an emergency.',
     '{"true": "The message describes a present or imminent danger to people or property (fire, smoke, shock, burns, injury, a swelling or leaking battery, someone unwell because of a product).", "false": "The message describes an ordinary service issue with no danger to anyone."}',
     'gte', 0.7, NULL, 'handoff',
     'This sounds urgent, so I am connecting you with a colleague right now. If anyone is in danger, unplug the device if it is safe to do so and call 112.',
     'closed', NULL, 0),
    ('credentials_shared', 'request', 'noul',
     'Decide whether the customer''s message contains a secret that must never be sent in a chat: a password or '
     'PIN, a one-time or verification code, a full payment card number, a card security code (CVV/CVC), or full '
     'online-banking login details. An order number, an email address, the last four digits of a card or an IBAN '
     'given for a refund are not secrets.',
     '{"true": "The message contains a password, PIN, one-time code, full card number, card security code or banking login.", "false": "The message contains no such secret."}',
     'gte', 0.6, NULL, 'block',
     'For your security, please never share passwords, codes or full card details in this chat. I have not used that message. Please send your question again without them.',
     'closed', NULL, 1),
    ('bereavement', 'request', 'noul',
     'Decide whether the customer says that someone close to them has died or is dying, for example a request about '
     'an order, account or subscription of a family member who passed away, or a gift that is no longer needed '
     'because of a death.',
     '{"true": "The customer mentions a death or terminal illness of someone close to them.", "false": "No death or bereavement is mentioned."}',
     'gte', 0.7, NULL, 'handoff',
     'I am very sorry for your loss. A colleague will personally take care of this for you and get in touch with you shortly.',
     'closed', NULL, 2),
    ('frustration', 'request', 'score',
     'Rate how frustrated or upset the customer is in this message, judging tone, wording, capitals, threats to '
     'cancel or complain, and mentions of having asked before. Rate the emotion, not whether the complaint is fair.',
     '["Calm: neutral or friendly", "Mildly irritated: some impatience", "Frustrated: clearly annoyed, mentions repeated contact or delays", "Angry: hostile wording, threatens to cancel, complain or leave a bad review", "Furious: abusive, shouting, or threatening legal action"]',
     'gte', 3, NULL, 'handoff',
     'I am sorry this has been so frustrating. I am bringing in a colleague who can sort this out with you directly.',
     'open', NULL, 3),
    ('topic', 'request', 'choice',
     'Classify what the customer''s message is mainly about, as a customer-care agent of an online shop would route it.',
     '{"returns": "Sending a product back for a refund or exchange, or a return label.", "warranty": "A product that is broken or defective and needs repair or replacement under warranty.", "delivery": "Where an order is, a late, lost or damaged delivery, or changing a delivery.", "payment": "Charges, double payments, refunds not received, invoices or declined payments.", "faq": "A general question about products, opening hours, policies or how something works.", "other": "Anything else, or not a customer-care request."}',
     'gte', 0.5, NULL, 'annotate', NULL, 'open', NULL, 4),
    ('customer_consent', 'tool', 'noul',
     'An agent is about to run the tool below for this customer. Decide whether the customer''s own request explicitly '
     'asks for what this tool does: a refund for order_issue_refund, a return (RMA) for order_create_rma. Asking only '
     'where an order is, or asking a question, is not consent to a refund or a return.',
     '{"true": "The customer explicitly asked for this refund or return.", "false": "The customer did not ask for this refund or return."}',
     'lt', 0.5, NULL, 'block', NULL, 'closed', '["order_issue_refund", "order_create_rma"]', 0),
    ('unbacked_claim', 'response', 'noul',
     'Below is a reply about to be sent to a customer and the tool outcomes it was written from. Decide whether the '
     'reply states a fact or makes a promise that the tool outcomes do not support: an amount, date, refund, label, '
     'claim number, delivery status or approval that does not appear in them.',
     '{"true": "The reply contains at least one claim or promise not backed by the tool outcomes.", "false": "Every fact and promise in the reply is backed by the tool outcomes."}',
     'gte', 0.5, NULL, 'handoff',
     'I want to be sure you get accurate information, so a colleague will check this and reply to you by email today.',
     'closed', NULL, 0),
    ('pii_leak', 'response', 'noul',
     'Decide whether the reply below discloses personal data that the customer did not provide and does not need: '
     'another person''s name, address, phone number or email, a full payment card or bank account number, or '
     'another customer''s order details.',
     '{"true": "The reply discloses personal data it should not.", "false": "The reply discloses no personal data beyond what the customer needs."}',
     'gte', 0.6, NULL, 'handoff',
     'A colleague will follow up on this with you directly by email.',
     'closed', NULL, 1)
 ) AS q(key, point, "type", instructions, criteria, fire_when, threshold, fire_values, action, message, fail, tools,
        ordinal)
 WHERE j.tenant_id = '0195f48b-d000-7000-8000-000000000099' AND j.key = 'customer_care_agent';
