import type { Api } from './api/client';
import { ApiError, DEFAULT_JOB } from './api/client';
import type { JobSummary, Me, TenantSummary } from './api/types';
import { session, type User } from './auth/session.svelte';
import { toast } from './ui/toaster.svelte';
import { CHECKING_TEXT, classifyWriteError, type WriteOutcome } from './write-outcome';

/**
 * Why the app cannot show its pages for this caller: `not_provisioned` (no care-bus tenant, not a super_admin),
 * `tenant_suspended`, or `pick_tenant` (a super_admin with no tenant to act in). `null` = ready.
 */
export type Blocked = 'not_provisioned' | 'tenant_suspended' | 'pick_tenant' | null;

/**
 * `job` is the job the job-scoped routes act in; `activeTenant` is a super_admin's chosen tenant, sent as
 * `X-Active-Tenant-Id` (null for everyone else: a person always acts in their home tenant). On a job page
 * (`/admin/jobs/{job}/…`) the job layout makes `job` the URL's job before the page renders (plan 14).
 */
export const app = $state<{
	api: Api | null;
	ready: boolean;
	job: string;
	jobs: JobSummary[];
	activeTenant: string | null;
	blocked: Blocked;
	/** Every tenant, for a super_admin's tenant picker (empty for everyone else, or when the list failed). */
	tenants: TenantSummary[];
}>({ api: null, ready: false, job: DEFAULT_JOB, jobs: [], activeTenant: null, blocked: null, tenants: [] });

export function api(): Api {
	if (!app.api) throw new Error('API used before authentication finished');
	return app.api;
}

const JOB_KEY = 'carebus-job';
const TENANT_KEY = 'carebus-active-tenant';
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function readStored(key: string): string | null {
	try {
		return sessionStorage.getItem(key);
	} catch {
		return null;
	}
}
function writeStored(key: string, value: string | null) {
	try {
		if (value === null) sessionStorage.removeItem(key);
		else sessionStorage.setItem(key, value);
	} catch {
		/* storage blocked: the choice lasts for this page only */
	}
}

/** A bare, non-nil uuid, as carebus-api accepts in X-Active-Tenant-Id. */
export function isTenantId(s: string): boolean {
	return UUID.test(s) && !/^[0-]+$/.test(s);
}

function blockedFor(me: Me): Blocked {
	if (me.super_admin) return me.active_tenant ? null : 'pick_tenant';
	if (!me.tenant) return 'not_provisioned';
	if (me.tenant.status === 'suspended') return 'tenant_suspended';
	return null;
}

/** The stored job when the tenant has it, else the first job, else the bootstrap job. */
export function pickJob(jobs: JobSummary[], stored: string | null): string {
	if (stored && jobs.some((j) => j.key === stored)) return stored;
	return jobs[0]?.key ?? DEFAULT_JOB;
}

/** The job the job-scoped pages, the console and the demo act in from now on (kept for this browser tab). */
export function selectJob(key: string) {
	app.job = key;
	writeStored(JOB_KEY, key);
}

async function loadJobs(a: Api) {
	try {
		app.jobs = (await a.jobs()).items;
	} catch (e) {
		if (!(e instanceof ApiError)) throw e;
		if (e.code === 'not_provisioned' || e.code === 'tenant_suspended') app.blocked = e.code;
		else if (e.code === 'active_tenant_required') app.blocked = 'pick_tenant';
		else throw e;
		app.jobs = [];
		return;
	}
	app.job = pickJob(app.jobs, readStored(JOB_KEY));
	writeStored(JOB_KEY, app.job);
}

/** A super_admin's tenant list (`GET /api/tenants`). Resolves an error message instead of throwing. */
export async function loadTenants(): Promise<string | null> {
	try {
		app.tenants = (await api().tenants()).items;
		return null;
	} catch (e) {
		return e instanceof Error ? e.message : String(e);
	}
}

/**
 * After sign-in: who the caller is (`GET /api/me`), which tenant a super_admin acts in (the one kept in
 * sessionStorage, if it still exists), and which job (`GET /api/jobs`). Sets `app.blocked` instead of throwing
 * for the three states a caller can be in without access.
 */
export async function bootstrapScope(a: Api, user: User): Promise<void> {
	app.activeTenant = null;
	let me = await a.me();
	const stored = readStored(TENANT_KEY);
	if (me.super_admin && stored && isTenantId(stored)) {
		app.activeTenant = stored;
		me = await a.me();
		if (me.active_tenant !== stored) {
			app.activeTenant = null;
			writeStored(TENANT_KEY, null);
		}
	}
	session.set(user, me);
	app.blocked = blockedFor(me);
	if (me.super_admin) await loadTenants();
	if (!app.blocked) await loadJobs(a);
}

/**
 * A super_admin acts in `tenantId` from now on. Resolves an error message when the id is malformed or names no
 * tenant (the previous tenant stays active), else null.
 */
export async function switchTenant(tenantId: string): Promise<string | null> {
	const id = tenantId.trim().toLowerCase();
	if (!isTenantId(id)) return 'A tenant id is a uuid such as 0195f48b-d000-7000-8000-000000000099.';
	const a = api();
	const prev = app.activeTenant;
	app.activeTenant = id;
	let me: Me;
	try {
		me = await a.me();
	} catch (e) {
		app.activeTenant = prev;
		return (e as Error).message;
	}
	if (me.active_tenant !== id) {
		app.activeTenant = prev;
		return 'No tenant with that id.';
	}
	writeStored(TENANT_KEY, id);
	session.set(session.user, me);
	app.blocked = blockedFor(me);
	if (!app.blocked) await loadJobs(a);
	return null;
}

const UNCERTAIN_TEXT =
	'The write outcome is uncertain: no answer came back. Reload before trying again; the server may have accepted it.';

/**
 * Runs a write, reports it as a toast and says what happened: `ok`, `rejected` (the server refused it; nothing
 * changed) or `uncertain` (no answer came back; it may have been stored). With `reconcile: true` the caller
 * re-reads the resource itself after an uncertain outcome, so the toast says a check is running instead of
 * asking for a reload.
 */
export async function guardOutcome(
	action: () => Promise<unknown>,
	okMsg: string | (() => string),
	failPrefix = 'Not saved',
	opts: { reconcile?: boolean } = {},
): Promise<WriteOutcome> {
	try {
		await action();
		toast(typeof okMsg === 'function' ? okMsg() : okMsg);
		return { kind: 'ok' };
	} catch (e) {
		if (e instanceof ApiError && e.status === 403) {
			toast('You need the tenant_admin role to do this.', 'err');
			return { kind: 'rejected', error: e };
		}
		if (classifyWriteError(e) === 'uncertain') {
			toast(opts.reconcile ? CHECKING_TEXT : UNCERTAIN_TEXT, 'err');
			return { kind: 'uncertain', error: e };
		}
		toast(`${failPrefix}: ${(e as Error).message}`, 'err');
		return { kind: 'rejected', error: e };
	}
}

/**
 * Boolean form of `guardOutcome`: resolves true only after success, so callers continue (close the drawer, clear
 * the form) only on `ok`; after any error the edits are kept.
 */
export async function guard(
	action: () => Promise<unknown>,
	okMsg: string | (() => string),
	failPrefix = 'Not saved',
): Promise<boolean> {
	return (await guardOutcome(action, okMsg, failPrefix)).kind === 'ok';
}
