-- Tenant licence-set reconcile (0022) on a tenant care-bus does not OWN in licences. Two exist live: the bootstrap
-- tenant (owner `platform`, care-bus a grantee: the bundle ensure answers 403 `tenant_owner_only`) and a foreign
-- test tenant (owner another app, no grant: the register answers 409 `tenant_owned_by_other_app`). Neither is a
-- failure care-bus can retry or fix: the reconcile now settles such a tenant as `ok` when the owner already ensured
-- its care-bus bundle (read through GET /licenses/active), else as the terminal `not_owner`
-- (crate::signup::provisioning::reconcile_bundles).
--
-- 1. `not_owner` is a status. Like `ok` and `failed` it is terminal: carebus_bundle_reconcile_due (0022) returns
--    only `pending` rows, so it is never selected again, unchanged.
-- 2. The rows that failed for exactly those two reasons before this logic existed go back to `pending`, due now, so
--    the next tick re-evaluates them. Run as the owner role at startup: the table has RLS ENABLEd but not FORCEd, so
--    the owner's UPDATE sees every tenant's row.
ALTER TABLE tenant_bundle_reconcile DROP CONSTRAINT tenant_bundle_reconcile_status_check;
ALTER TABLE tenant_bundle_reconcile ADD CONSTRAINT tenant_bundle_reconcile_status_check
    CHECK (status IN ('pending', 'ok', 'failed', 'not_owner'));

UPDATE tenant_bundle_reconcile
   SET status = 'pending', attempts = 0, next_attempt_at = now(), lease_until = NULL, updated_at = now()
 WHERE status = 'failed'
   AND (message LIKE '%tenant_owner_only%' OR message LIKE '%tenant_owned_by_other_app%');
