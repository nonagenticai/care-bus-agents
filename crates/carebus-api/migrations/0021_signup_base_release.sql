-- care-bus 17: a workspace an approved signup founds starts on the base release of its job key, the same base the
-- source job serves (W3 plan 11: base rows + tenant refinements, eff_* views over the job's CURRENT release), instead
-- of an empty job that serves nothing until a release is applied. Numbered 0021: 0020 is the research branch's.
--
-- 1. `signup_provisioning_steps` learns the `base_release` step, run after the decision and before the licences and
--    the email (ord 4; the later steps move up one). A journal written before this migration gets the step pending.
-- 2. `carebus_adopt_base_release(target tenant, job key, source tenant, actor)`: in ONE transaction, the target job
--    gets the source job's current release (its `job_releases` row, every `origin = 'base'` row of that release in
--    the nine overlay tables, procedure ids re-keyed, and the base `settings` when the target has none), i.e. what
--    the release apply did for a job before it was retired (12311bf). Idempotent and never destructive: a target
--    job that already has a current release is left as it is (`already_current`). `no_job` / `no_base_release`
--    write nothing. It reads another tenant's rows (the source's base), so it is SECURITY DEFINER; only base rows
--    are copied, and a base row holds no tenant wiring (no secret_ref, no strata_store_id: 0009 constraints).
--    Guarded inside, so a carebus_app session cannot aim it anywhere else: the TARGET must be the workspace of an
--    APPROVED signup (`signup_approvals.approved_tenant_id`, set by the decision before this step runs; stable,
--    unlike `tenants.updated_by`, which a super_admin re-provision overwrites), and the SOURCE (the app's
--    SIGNUP_BASE_TENANT_ID) must differ from the target and must not itself be a signup's workspace. Anything else
--    raises `insufficient_privilege` and writes nothing.

ALTER TABLE signup_provisioning_steps
    DROP CONSTRAINT signup_provisioning_steps_step_check,
    DROP CONSTRAINT signup_provisioning_steps_ord_check;

-- Two passes: UNIQUE (signup_id, ord) is checked row by row.
UPDATE signup_provisioning_steps SET ord = ord + 100 WHERE ord >= 4;
UPDATE signup_provisioning_steps SET ord = ord - 99 WHERE ord >= 100;

ALTER TABLE signup_provisioning_steps
    ADD CONSTRAINT signup_provisioning_steps_step_check CHECK (step IN ('keycloak_user', 'tenant', 'membership',
        'base_release', 'licences_tenant', 'member_licence', 'invite_email')),
    ADD CONSTRAINT signup_provisioning_steps_ord_check CHECK (ord BETWEEN 1 AND 7);

INSERT INTO signup_provisioning_steps (signup_id, step, ord, status, attempts, data, next_attempt_at)
SELECT DISTINCT signup_id, 'base_release', 4, 'pending', 0, '{}'::jsonb, now()
  FROM signup_provisioning_steps
ON CONFLICT DO NOTHING;

CREATE FUNCTION carebus_adopt_base_release(p_tenant uuid, p_job_key text, p_source_tenant uuid, p_actor text)
    RETURNS TABLE (adopt_outcome text, adopt_release text)
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    v_job      uuid;
    v_src_job  uuid;
    v_current  text;
    v_release  job_releases%ROWTYPE;
    v_table    text;
    v_cols     text;
    v_pcols    text;
    v_scols    text;
    v_old      bigint;
    v_new      bigint;
BEGIN
    IF p_tenant IS NULL OR p_source_tenant IS NULL OR p_source_tenant = p_tenant THEN
        RAISE EXCEPTION 'carebus_adopt_base_release: the source must be a tenant other than the target'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM signup_approvals s
                    WHERE s.status = 'approved' AND s.approved_tenant_id = p_tenant) THEN
        RAISE EXCEPTION 'carebus_adopt_base_release: the target is not the workspace of an approved signup'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF EXISTS (SELECT 1 FROM signup_approvals s WHERE s.approved_tenant_id = p_source_tenant) THEN
        RAISE EXCEPTION 'carebus_adopt_base_release: the source is a signup workspace, not the base'
            USING ERRCODE = 'insufficient_privilege';
    END IF;

    SELECT j.id INTO v_job FROM jobs j WHERE j.tenant_id = p_tenant AND j.key = p_job_key FOR UPDATE;
    IF v_job IS NULL THEN
        RETURN QUERY SELECT 'no_job'::text, NULL::text;
        RETURN;
    END IF;
    SELECT r.release_id INTO v_current
      FROM job_releases r WHERE r.tenant_id = p_tenant AND r.job_id = v_job AND r.is_current;
    IF v_current IS NOT NULL THEN
        RETURN QUERY SELECT 'already_current'::text, v_current;
        RETURN;
    END IF;
    SELECT j.id INTO v_src_job FROM jobs j WHERE j.tenant_id = p_source_tenant AND j.key = p_job_key;
    SELECT r.* INTO v_release
      FROM job_releases r WHERE r.tenant_id = p_source_tenant AND r.job_id = v_src_job AND r.is_current;
    IF v_src_job IS NULL OR v_release.release_id IS NULL THEN
        RETURN QUERY SELECT 'no_base_release'::text, NULL::text;
        RETURN;
    END IF;

    INSERT INTO job_releases (tenant_id, job_id, release_id, hash, bundle, counts, is_current, applied_by)
    VALUES (p_tenant, v_job, v_release.release_id, v_release.hash, v_release.bundle, v_release.counts, true,
            coalesce(nullif(btrim(p_actor), ''), 'carebus_adopt_base_release'));

    -- Parents before children (the base FKs of 0009). Every stored column is copied except the scope; generated
    -- columns (fk_origin, version_sort) are computed again.
    FOREACH v_table IN ARRAY ARRAY['context_spaces', 'ontology_entities', 'ontology_attributes',
                                   'ontology_relations', 'connectors', 'connector_field_mappings',
                                   'knowledge_stores'] LOOP
        SELECT string_agg(quote_ident(a.attname), ', ' ORDER BY a.attnum) INTO v_cols
          FROM pg_attribute a
         WHERE a.attrelid = v_table::regclass AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated = ''
           AND a.attname NOT IN ('tenant_id', 'job_id');
        EXECUTE format('INSERT INTO %I (tenant_id, job_id, %s) SELECT $1, $2, %s FROM %I '
                       'WHERE tenant_id = $3 AND job_id = $4 AND origin = ''base'' AND release_id = $5',
                       v_table, v_cols, v_cols, v_table)
            USING p_tenant, v_job, p_source_tenant, v_src_job, v_release.release_id;
    END LOOP;

    -- Procedures keep their bigserial id unique across scopes, so each version gets a new id and its steps follow.
    SELECT string_agg(quote_ident(a.attname), ', ' ORDER BY a.attnum) INTO v_pcols
      FROM pg_attribute a
     WHERE a.attrelid = 'procedures'::regclass AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated = ''
       AND a.attname NOT IN ('tenant_id', 'job_id', 'id');
    SELECT string_agg(quote_ident(a.attname), ', ' ORDER BY a.attnum) INTO v_scols
      FROM pg_attribute a
     WHERE a.attrelid = 'procedure_steps'::regclass AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated = ''
       AND a.attname NOT IN ('tenant_id', 'job_id', 'procedure_id');
    FOR v_old IN
        SELECT p.id FROM procedures p
         WHERE p.tenant_id = p_source_tenant AND p.job_id = v_src_job AND p.origin = 'base'
           AND p.release_id = v_release.release_id
         ORDER BY p.id
    LOOP
        EXECUTE format('INSERT INTO procedures (tenant_id, job_id, %s) SELECT $1, $2, %s FROM procedures '
                       'WHERE tenant_id = $3 AND job_id = $4 AND id = $5 RETURNING id', v_pcols, v_pcols)
            INTO v_new
            USING p_tenant, v_job, p_source_tenant, v_src_job, v_old;
        EXECUTE format('INSERT INTO procedure_steps (tenant_id, job_id, procedure_id, %s) SELECT $1, $2, $6, %s '
                       'FROM procedure_steps WHERE tenant_id = $3 AND job_id = $4 AND procedure_id = $5 '
                       'AND origin = ''base''', v_scols, v_scols)
            USING p_tenant, v_job, p_source_tenant, v_src_job, v_old, v_new;
    END LOOP;

    -- The base settings belong to the release too; a target job with settings of its own keeps them.
    UPDATE jobs t SET settings = s.settings, updated_at = now()
      FROM jobs s
     WHERE t.id = v_job AND s.id = v_src_job AND t.settings = '{}'::jsonb;

    RETURN QUERY SELECT 'adopted'::text, v_release.release_id;
END
$$;

REVOKE ALL ON FUNCTION carebus_adopt_base_release(uuid, text, uuid, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_adopt_base_release(uuid, text, uuid, text) TO carebus_app;
