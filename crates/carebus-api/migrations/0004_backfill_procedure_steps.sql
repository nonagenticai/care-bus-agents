-- Backfill PDDL step rows for procedure versions stored without any (the seeded return@1.4.0-beta canary, and any
-- version created before POST /api/procedures required the steps to mirror the plan). Such a version listed a step
-- count of 0 and showed no steps in its detail although its plan has steps.
-- A version gets the step rows of the newest other version of the SAME name whose plan has the identical
-- (phase, worker, action) sequence, so every copied row describes exactly the plan step it sits next to.
-- A version with no such sibling is left as it is: its PDDL annotations cannot be derived from the plan alone.
WITH shape AS (
    SELECT p.id, p.name, p.version_sort,
           (SELECT jsonb_agg(jsonb_build_array(ph.i - 1, st.s->>'worker', st.s->>'action') ORDER BY ph.i, st.j)
              FROM jsonb_array_elements(COALESCE(p.plan->'phases', '[]'::jsonb)) WITH ORDINALITY AS ph(v, i),
                   jsonb_array_elements(COALESCE(ph.v->'steps', '[]'::jsonb)) WITH ORDINALITY AS st(s, j)) AS sig,
           EXISTS (SELECT 1 FROM procedure_steps s WHERE s.procedure_id = p.id) AS has_rows
      FROM procedures p
),
donor AS (
    SELECT DISTINCT ON (t.id) t.id AS target, d.id AS source
      FROM shape t
      JOIN shape d ON d.name = t.name AND d.id <> t.id AND d.has_rows AND d.sig = t.sig
     WHERE NOT t.has_rows AND t.sig IS NOT NULL
     ORDER BY t.id, d.version_sort DESC, d.id DESC
)
INSERT INTO procedure_steps (procedure_id, step_number, phase, name, action_type, worker, mcp_tool, performed_by,
                             preconditions, postconditions, parameters, gate)
SELECT donor.target, s.step_number, s.phase, s.name, s.action_type, s.worker, s.mcp_tool, s.performed_by,
       s.preconditions, s.postconditions, s.parameters, s.gate
  FROM donor JOIN procedure_steps s ON s.procedure_id = donor.source;
