-- Laya gates: `emergency` and `bereavement` read the customer's own words (`input = 'message'`), not the wrapped
-- request. Why (live 2026-10-24, tenant 0195f48b-d000-7000-8000-000000000099, verdict 31): a refund question got the
-- emergency "call 112" handoff. Both questions kept 0018's default `input = 'request'`, so laya was asked about the
-- request text a live turn builds (gates::request_text):
--   Customer message: How long does a refund take?
--   First message of this request: How long does a refund take?
--   Request type: faq
--   Details: {"question":"How long does a refund take?"}
-- Playground replays of that turn: the full wrapper answers emergency 0.9768 / bereavement 0.9342 (over 0017's 0.75
-- and 0.7); the wrapper with empty details still 0.67; the customer message alone emergency 0.0044 / bereavement
-- 0.0247. The wrapper caused it, the same class 0018 fixed for credentials_shared. Owner ruling 2026-10-24: fix the
-- input; no keyword precheck (an emergency phrased without keywords must still reach laya), thresholds, actions and
-- fail policies unchanged.
--
-- `message` sends exactly what the customer wrote this turn (no "Customer message:" prefix, at most
-- MAX_STATE_CHARS), as for credentials_shared. Every tenant's rows, not only the bootstrap tenant's: these keys at the
-- request point are the retail demo's questions (0016 seeds them; no signup or bundle copies gate questions), and a
-- tenant's own copy under the same key reads the same wrapper and misfires the same way. Unconditional on who last
-- edited the row, like 0017/0018 (care-bus-agents is a hard-coded demo); a row already reading the message is left
-- as it is.
UPDATE gate_questions
   SET input = 'message',
       updated_at = now(),
       updated_by = 'migration:0027'
 WHERE key IN ('emergency', 'bereavement') AND point = 'request' AND input = 'request';
