-- Laya gates: what each question reads (`input`) and a cheap pattern that must match before laya is asked
-- (`precheck`). Why: the calibrated credentials_shared of 0017 blocked harmless turns live — verdicts 25–27 answered
-- `yes` at 0.87 / 0.81 on "where is my order?". A live request turn sends laya the WRAPPED request ("Customer message:
-- …", "First message of this request: …", "Request type: …", "Details: {slots JSON}"), whose order numbers and
-- emails read as codes, while 0017 was calibrated on the bare message in the playground; and laya's answers are
-- deterministic per input, so the same turn blocks every time. 0017 also dropped 0016's exclusion sentence.
--
-- input: `request` (default, every existing row) = the request text as before; `message` = only what the customer
-- wrote this turn. Only request-point questions can read the message (the tool and response points have no customer
-- turn of their own).
-- precheck: a case-insensitive regex (Rust `regex` syntax, validated by the API on save); when set and it does not
-- match the question's text, the question is not asked and its answer is recorded as skipped (not fired).
ALTER TABLE gate_questions
    ADD COLUMN input text NOT NULL DEFAULT 'request' CHECK (input IN ('message', 'request')),
    ADD COLUMN precheck text NULL CHECK (precheck IS NULL OR length(precheck) BETWEEN 1 AND 500),
    ADD CONSTRAINT gate_questions_input CHECK (point = 'request' OR input = 'request');

-- The retail demo's credentials_shared, re-seeded by key and unconditionally like 0017 (care-bus-agents is a
-- hard-coded demo: an admin's edit of this row is overwritten on purpose). It reads the customer's own words only,
-- is asked only when they contain something that could carry a secret (a run of 4+ digits, possibly spaced or
-- dashed, or a password/code word in English or Dutch — the demo serves both), and its instructions restore 0016's
-- exclusion.
UPDATE gate_questions q
   SET input = 'message',
       precheck = '(?i)\d(?:[ -]?\d){3,}|\b(?:password|passcode|wachtwoord|pincode|pin|cvv|cvc|security code|beveiligingscode|otp|one-time|eenmalige|verification code|verificatiecode|login code|inlogcode|2fa|tan)',
       instructions = 'Does this customer message contain a password, PIN, one-time code, full card number or card security code? An order number, an email address, a phone number, the last four digits of a card or an IBAN given for a refund are not secrets.',
       updated_at = now(),
       updated_by = 'migration:0018'
  FROM (SELECT tenant_id, id AS job_id FROM jobs
         WHERE tenant_id = '0195f48b-d000-7000-8000-000000000099' AND key = 'customer_care_agent') d
 WHERE q.tenant_id = d.tenant_id AND q.job_id = d.job_id AND q.key = 'credentials_shared';
