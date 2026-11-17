-- Laya gates calibrated against laya-multilingual (2026-10-18). The seeded questions of 0016 misfired live; this
-- re-seeds the retail demo's rows for the bootstrap tenant's customer_care_agent job, by key and unconditionally
-- (care-bus-agents is a hard-coded demo: an admin's edit of these rows is overwritten on purpose).

-- `in` may now carry a threshold: the minimum confidence in the chosen option (NULL = the option alone fires).
ALTER TABLE gate_questions DROP CONSTRAINT gate_questions_rule;
ALTER TABLE gate_questions ADD CONSTRAINT gate_questions_rule CHECK (
    (fire_when = 'in' AND "type" = 'choice' AND fire_values IS NOT NULL
     AND (threshold IS NULL OR threshold BETWEEN 0 AND 1))
    OR (fire_when IN ('gte', 'lt') AND threshold IS NOT NULL AND fire_values IS NULL));

-- A yes/no/unsure choice with a confidence floor separates secrets far better than a noul did.
UPDATE gate_questions q
   SET "type" = 'choice',
       instructions = 'Does this customer message contain a password, PIN, one-time code, full card number or card security code?',
       criteria = '{"yes": "the message contains such a secret", "no": "the message contains no such secret", "unsure": "cannot tell"}'::jsonb,
       fire_when = 'in',
       fire_values = '["yes"]'::jsonb,
       threshold = 0.6,
       action = 'block',
       fail = 'closed',
       enabled = true,
       updated_at = now(),
       updated_by = 'migration:0017'
  FROM (SELECT tenant_id, id AS job_id FROM jobs
         WHERE tenant_id = '0195f48b-d000-7000-8000-000000000099' AND key = 'customer_care_agent') d
 WHERE q.tenant_id = d.tenant_id AND q.job_id = d.job_id AND q.key = 'credentials_shared';

-- Asked of the customer's own words only (the worker no longer sends the tool's name and arguments as state).
UPDATE gate_questions q
   SET "type" = 'noul',
       instructions = 'Did the customer explicitly ask for a refund or a return?',
       criteria = '{"true": "the customer asked for a refund or return", "false": "the customer did not ask for a refund or return"}'::jsonb,
       fire_when = 'lt',
       fire_values = NULL,
       threshold = 0.5,
       enabled = true,
       updated_at = now(),
       updated_by = 'migration:0017'
  FROM (SELECT tenant_id, id AS job_id FROM jobs
         WHERE tenant_id = '0195f48b-d000-7000-8000-000000000099' AND key = 'customer_care_agent') d
 WHERE q.tenant_id = d.tenant_id AND q.job_id = d.job_id AND q.key = 'customer_consent';

UPDATE gate_questions q
   SET threshold = 0.75, updated_at = now(), updated_by = 'migration:0017'
  FROM (SELECT tenant_id, id AS job_id FROM jobs
         WHERE tenant_id = '0195f48b-d000-7000-8000-000000000099' AND key = 'customer_care_agent') d
 WHERE q.tenant_id = d.tenant_id AND q.job_id = d.job_id AND q.key = 'emergency';

-- Laya cannot separate bereavement reliably: annotate (visible in the demo), never stop a turn on it.
UPDATE gate_questions q
   SET action = 'annotate', fail = 'open', updated_at = now(), updated_by = 'migration:0017'
  FROM (SELECT tenant_id, id AS job_id FROM jobs
         WHERE tenant_id = '0195f48b-d000-7000-8000-000000000099' AND key = 'customer_care_agent') d
 WHERE q.tenant_id = d.tenant_id AND q.job_id = d.job_id AND q.key = 'bereavement';

-- Laya System-1 cannot compare a reply with the tool outcomes it was written from, so these response questions
-- misfire either way; kept as rows (editable, re-enable when a model can), but off.
UPDATE gate_questions q
   SET enabled = false, updated_at = now(), updated_by = 'migration:0017'
  FROM (SELECT tenant_id, id AS job_id FROM jobs
         WHERE tenant_id = '0195f48b-d000-7000-8000-000000000099' AND key = 'customer_care_agent') d
 WHERE q.tenant_id = d.tenant_id AND q.job_id = d.job_id AND q.key IN ('unbacked_claim', 'pii_leak');
