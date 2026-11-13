-- W3 plan 12 Task 6: an optional per-step side-effect flag (release bundle v1 `steps[].side_effect`,
-- `POST /api/procedures` `steps[].side_effect`). NULL (every existing row): carebus::mcp::SIDE_EFFECT_TOOLS decides
-- whether the step's tool call is side-effecting; true/false overrides that list for this step.
-- Numbered 0014, after the highest existing migration (0013), so a database that already ran 0013 applies it in order.
ALTER TABLE procedure_steps ADD COLUMN side_effect boolean;

-- `s.*` was expanded when 0010 created the view; recreate it so the view carries the new (last) column.
-- CREATE OR REPLACE keeps the view's grants (SELECT only for carebus_app).
CREATE OR REPLACE VIEW eff_procedure_steps WITH (security_invoker = true) AS
SELECT s.*
  FROM procedure_steps s
 WHERE EXISTS (SELECT 1 FROM eff_procedures p
                WHERE p.tenant_id = s.tenant_id AND p.job_id = s.job_id AND p.origin = s.origin
                  AND p.id = s.procedure_id);
