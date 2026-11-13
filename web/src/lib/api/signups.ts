// The early-access signup queue (care-bus 17), super_admin only. Types mirror crates/carebus-api
// src/routes/signups.rs (`item`, `step_json`, `list`) and signup/store.rs `Approval`; every route answers 503
// `signup_not_configured` while the feature is off. Pure helpers only, so the onboarding page logic is unit tested
// directly (tests/unit/signups.test.ts).
import { ApiError } from './client';

/** signup/mod.rs `SIGNUP_LOCALES`: the languages the set-password email ships in. */
export const SIGNUP_LOCALES = ['ar', 'de', 'en', 'es', 'fr', 'he', 'nl', 'pl'] as const;
export type SignupLocale = (typeof SIGNUP_LOCALES)[number];
export const LOCALE_NAMES: Record<SignupLocale, string> = {
	ar: 'Arabic',
	de: 'German',
	en: 'English',
	es: 'Spanish',
	fr: 'French',
	he: 'Hebrew',
	nl: 'Dutch',
	pl: 'Polish',
};

export type SignupStatus = 'pending' | 'approved' | 'rejected' | 'removed';
export type StepStatus = 'pending' | 'ok' | 'failed' | 'skipped';

/** One row of the provisioning journal (`step_json`). */
export interface SignupStep {
	step: string;
	status: StepStatus;
	code: string | null;
	message: string | null;
	attempts: number;
	at: string | null;
	next_attempt_at: string | null;
}

export interface Signup {
	id: string;
	email: string;
	name: string;
	company: string | null;
	message: string | null;
	/** `web` | `chat` (intake) | `invite` (a super_admin invited them; `invited_by` says who). */
	source: string;
	locale: string | null;
	trust: string | null;
	intake_client: string | null;
	invited_by: string | null;
	status: SignupStatus;
	tenant_id: string | null;
	created_at: string;
	decided_at: string | null;
	decided_by: string | null;
	decided_via: string | null;
	reason: string | null;
	/** An approve holds the claim right now; a second approve gets 409 `approval_in_progress`. */
	approving: boolean;
	/** What a reject undid of an aborted approve (codes only), e.g. `{tenant: "suspended", login: "deleted"}`. */
	reject_repair: Record<string, string> | null;
	suggested_tenant_name: string;
	steps: SignupStep[];
	/** The removal of a once-approved signup (signup/removal.rs); null while it was never removed. */
	removal_state?: RemovalState | null;
	removal_requested_by?: string | null;
	removal_requested_at?: string | null;
	removal_done_at?: string | null;
	/** `GET …/{id}` only: the workspace slug a removal is confirmed with (approved / removed signups). */
	tenant_slug?: string | null;
	/** `GET …/{id}` only: the removal and its journal. */
	removal?: Removal | null;
}

export type RemovalState = 'pending' | 'blocked' | 'failed' | 'done';
export type RemovalStepStatus = 'pending' | 'ok' | 'skipped' | 'failed' | 'blocked';

/** One row of the removal journal (`removal::view`): codes and licence ids, no personal data. */
export interface RemovalStep {
	step: string;
	status: RemovalStepStatus;
	code: string | null;
	message: string | null;
	attempts: number;
	at: string | null;
	result: Record<string, unknown>;
}

export interface Removal {
	state: RemovalState;
	requested_by: string;
	requested_at: string;
	done_at: string | null;
	steps: RemovalStep[];
}

export interface SignupPage {
	items: Signup[];
	next: string | null;
	queue_cap: number;
	pending: number;
	counts: { received: number; approved: number; rejected: number; removed?: number };
	as_of: string;
}

/** The list's `status` query; `decided` = approved and rejected. */
export type SignupListStatus = SignupStatus | 'decided' | 'all';

export interface ApproveResult {
	signup: Signup;
	tenant: { id: string; name: string; status: string };
	steps: SignupStep[];
}

export interface InviteResult extends ApproveResult {
	/** An earlier pending request for this address was approved instead of a new row. */
	reused_pending: boolean;
}

export interface RetryResult {
	requeued: number;
	steps: SignupStep[];
}

/** `POST /api/signup-approvals/invite` (strict body: unknown fields are refused, empty strings too). */
export interface InviteInput {
	email: string;
	name: string;
	company?: string;
	tenantName?: string;
	locale?: SignupLocale;
}

// ---- steps ------------------------------------------------------------------------------------------------------

/** provisioning.rs `STEPS`, in order, as the page names them. */
export const STEP_LABELS: Record<string, string> = {
	keycloak_user: 'Login',
	tenant: 'Workspace',
	membership: 'Admin seat',
	base_release: 'Base content',
	licences_tenant: 'Workspace licence',
	member_licence: 'Member licence',
	invite_email: 'Set-password email',
};
export const stepLabel = (step: string) => STEP_LABELS[step] ?? step;

export const STEP_VARIANT: Record<StepStatus, 'ok' | 'err' | 'warn' | 'muted'> = {
	ok: 'ok',
	failed: 'err',
	pending: 'warn',
	skipped: 'muted',
};

/** The email step whose answer never came (provisioning.rs): it may have been sent, and is never resent by itself. */
export const UNCERTAIN_EMAIL = 'invite_email_uncertain';

export function uncertainEmail(s: Pick<Signup, 'steps'>): boolean {
	return s.steps.some(
		(st) => st.step === 'invite_email' && st.status === 'failed' && st.code === UNCERTAIN_EMAIL,
	);
}

export const UNCERTAIN_EMAIL_TEXT =
	'The set-password email may have been sent: Keycloak did not answer. Retry sends a NEW link and the first one stops working, so check with the requester (or the Keycloak events) first.';

export interface StepSummary {
	done: number;
	total: number;
	failed: SignupStep[];
	pending: SignupStep[];
}

export function stepSummary(steps: SignupStep[]): StepSummary {
	return {
		done: steps.filter((s) => s.status === 'ok' || s.status === 'skipped').length,
		total: steps.length,
		failed: steps.filter((s) => s.status === 'failed'),
		pending: steps.filter((s) => s.status === 'pending'),
	};
}

/**
 * What a row shows as its state. `failed` and `provisioning` are approved signups whose journal is not finished:
 * a failed step stops the chain until Retry; a pending one is finished by the tick.
 */
export type RowState =
	| 'pending'
	| 'approving'
	| 'approved'
	| 'provisioning'
	| 'failed'
	| 'rejected'
	| 'removing'
	| 'removal_stuck'
	| 'removed';

export function rowState(
	s: Pick<Signup, 'status' | 'approving' | 'steps'> & Partial<Pick<Signup, 'removal_state'>>,
): RowState {
	if (s.status === 'rejected') return 'rejected';
	if (s.status === 'removed') {
		if (s.removal_state === 'done') return 'removed';
		return s.removal_state === 'pending' ? 'removing' : 'removal_stuck';
	}
	if (s.status === 'pending') return s.approving ? 'approving' : 'pending';
	const sum = stepSummary(s.steps);
	if (sum.failed.length) return 'failed';
	if (sum.pending.length) return 'provisioning';
	return 'approved';
}

export const ROW_STATE: Record<
	RowState,
	{ label: string; variant: 'ok' | 'err' | 'warn' | 'muted' | 'accent' }
> = {
	pending: { label: 'Pending', variant: 'accent' },
	approving: { label: 'Approving…', variant: 'warn' },
	approved: { label: 'Approved', variant: 'ok' },
	provisioning: { label: 'Provisioning', variant: 'warn' },
	failed: { label: 'Step failed', variant: 'err' },
	rejected: { label: 'Rejected', variant: 'muted' },
	removing: { label: 'Removing…', variant: 'warn' },
	removal_stuck: { label: 'Removal stopped', variant: 'err' },
	removed: { label: 'Removed', variant: 'muted' },
};

/** Retry is offered for an approved signup with a failed step (routes::signups `retry` answers 409 otherwise). */
export const canRetry = (s: Pick<Signup, 'status' | 'steps'>) =>
	s.status === 'approved' && s.steps.some((st) => st.status === 'failed');

// ---- removal ---------------------------------------------------------------------------------------------------

/** removal.rs `STEPS`, in order, as the page names them. */
export const REMOVAL_STEP_LABELS: Record<string, string> = {
	suspend: 'Suspend workspace',
	members: 'Delete members',
	licences: 'Revoke licences',
	identity: 'Delete login',
	licences_tenant: 'Archive licences tenant',
};
export const removalStepLabel = (step: string) => REMOVAL_STEP_LABELS[step] ?? step;

export const REMOVAL_STEP_VARIANT: Record<RemovalStepStatus, 'ok' | 'err' | 'warn' | 'muted'> = {
	ok: 'ok',
	failed: 'err',
	blocked: 'err',
	pending: 'warn',
	skipped: 'muted',
};

/** Remove is offered for an approved signup with a workspace (the route answers 409 otherwise). */
export const canRemove = (s: Pick<Signup, 'status' | 'tenant_id'>) =>
	s.status === 'approved' && !!s.tenant_id;

/** A removal that is not finished can be retried (failed and blocked steps run again). */
export const canRetryRemoval = (s: Pick<Signup, 'status' | 'removal_state'>) =>
	s.status === 'removed' && !!s.removal_state && s.removal_state !== 'done';

/** The typed confirmation matches the workspace slug (the server trims and compares exactly too). */
export const removalConfirmed = (typed: string, slug: string | null | undefined) =>
	!!slug && typed.trim() === slug;

// ---- filters ----------------------------------------------------------------------------------------------------

/** The page's filter. `failed` is not a server status: it is the approved rows with a failed step. */
export type Filter = 'pending' | 'approved' | 'rejected' | 'removed' | 'failed' | 'all';
export const FILTERS: [Filter, string][] = [
	['pending', 'Pending'],
	['approved', 'Approved'],
	['rejected', 'Rejected'],
	['removed', 'Removed'],
	['failed', 'Failed'],
	['all', 'All'],
];

export function isFilter(v: string | null): v is Filter {
	return FILTERS.some(([f]) => f === v);
}

/** The list query for a filter. */
export function serverStatus(f: Filter): SignupListStatus {
	return f === 'failed' ? 'approved' : f;
}

/** The rows of a page the filter shows. */
export function visible(f: Filter, items: Signup[]): Signup[] {
	return f === 'failed' ? items.filter((s) => rowState(s) === 'failed') : items;
}

// ---- display ----------------------------------------------------------------------------------------------------

/** `an…@example.com`: the list shows addresses masked; the detail drawer shows them in full. */
export function maskEmail(email: string): string {
	const at = email.lastIndexOf('@');
	if (at < 1) return email;
	const local = email.slice(0, at);
	return `${local.slice(0, Math.min(2, local.length))}…${email.slice(at)}`;
}

export function localeLabel(l: string | null): string {
	if (!l) return 'English (not told)';
	return l in LOCALE_NAMES ? `${LOCALE_NAMES[l as SignupLocale]} (${l})` : l;
}

/** `reject_repair` as one line: what an aborted approve had founded and what the reject did to it. */
export function repairLine(r: Record<string, string> | null | undefined): string | null {
	if (!r) return null;
	const parts = Object.entries(r).map(([k, v]) => `${k} ${v}`);
	return parts.length ? parts.join(', ') : null;
}

/** What the requester was told, from the journal (provisioning.rs `email_note`). */
export function emailNote(steps: SignupStep[]): string {
	const r = steps.find((s) => s.step === 'invite_email');
	if (!r) return 'no email step yet';
	if (r.status === 'ok') return 'set-password email sent';
	if (r.status === 'failed')
		return r.code === UNCERTAIN_EMAIL
			? 'email outcome unknown (no answer from Keycloak); check before Retry'
			: 'email failed: use Retry';
	if (r.status === 'skipped') return 'no email (skipped)';
	return 'email follows once the earlier steps are done';
}

/** Conflicts the approve/invite/reject routes answer with, said for the person deciding. */
export const CONFLICT_TEXT: Record<string, string> = {
	signup_not_pending: 'This signup was decided already. The list has been reloaded.',
	approval_in_progress:
		'An approve is running for this signup right now (screen or Telegram). Reload in a moment.',
	approval_claim_lost:
		'The approve lost its claim (another approver took over, or it ran too long). Nothing further was done; reload and look again.',
	identity_exists:
		'A login for this address already exists and was not created by this signup, so it is never written to or mailed.',
	user_belongs_to_another_workspace: 'This person already belongs to another care-bus workspace.',
	signup_not_approved: 'Only an approved signup has steps to retry or can be removed.',
	removal_exists: 'This signup is being removed or was removed already. The list has been reloaded.',
	removal_done: 'This removal is finished; there is nothing to retry.',
	removal_in_progress: 'This removal is running right now. Reload in a moment.',
	signup_not_removed: 'Only a removal can be retried.',
	signup_no_workspace: "The signup's workspace no longer exists.",
	tenant_mismatch:
		"The signup's workspace is not the one its approve founded, so it is never removed from here.",
	provisioning_in_progress:
		'A provisioning step of this signup is running right now. Try again in a few minutes.',
	removal_confirm_mismatch: 'Type the workspace slug exactly as shown to confirm.',
	signup_not_found: 'No such signup.',
	signup_not_configured: 'Early-access signup is not enabled on this deployment.',
};

// ---- invite form ------------------------------------------------------------------------------------------------

export interface InviteDraft {
	email: string;
	name: string;
	company: string;
	tenantName: string;
	locale: SignupLocale | '';
}
export type InviteField = 'email' | 'name' | 'company' | 'tenantName' | 'locale';
export const INVITE_FIELDS: readonly InviteField[] = ['email', 'name', 'company', 'tenantName', 'locale'];

export const emptyInvite = (): InviteDraft => ({
	email: '',
	name: '',
	company: '',
	tenantName: '',
	locale: '',
});

/** signup/mod.rs `email_address`: one `@`, no whitespace, a dotted domain, at most 254 characters. */
export function emailProblem(raw: string): string {
	const e = raw.trim();
	const [local, domain, extra] = e.split('@');
	const ok =
		e.length > 0 &&
		e.length <= 254 &&
		!/[\s\p{Cc}]/u.test(e) &&
		extra === undefined &&
		!!local &&
		!!domain &&
		domain.includes('.') &&
		!domain.startsWith('.') &&
		!domain.endsWith('.');
	return ok ? '' : 'Enter an email address such as anna@example.com.';
}

/** Field → message; empty = nothing found. carebus-api stays the authority. */
export function validateInvite(d: InviteDraft): Partial<Record<InviteField, string>> {
	const e: Partial<Record<InviteField, string>> = {};
	const email = emailProblem(d.email);
	if (email) e.email = email;
	if (!d.name.trim()) e.name = 'Enter the name.';
	else if (d.name.trim().length > 160) e.name = 'At most 160 characters.';
	if (d.company.trim().length > 160) e.company = 'At most 160 characters.';
	const t = d.tenantName.trim();
	if (t && (t.length < 2 || t.length > 120)) e.tenantName = '2 to 120 characters.';
	if (d.locale && !SIGNUP_LOCALES.includes(d.locale)) e.locale = 'Choose one of the listed languages.';
	return e;
}

/** The POST body: trimmed, optional fields only when filled (the route refuses an empty company or workspace). */
export function toInviteInput(d: InviteDraft): InviteInput {
	const input: InviteInput = { email: d.email.trim(), name: d.name.trim() };
	if (d.company.trim()) input.company = d.company.trim();
	if (d.tenantName.trim()) input.tenantName = d.tenantName.trim();
	if (d.locale) input.locale = d.locale;
	return input;
}

export interface Fault {
	/** The form field the refusal is about; null = shown above the actions. */
	field: InviteField | null;
	code: string;
	message: string;
	/** A gating step failed (502 `<step code>`): the journal so far. Nothing was decided. */
	steps: SignupStep[];
}

/**
 * The refusal as a fault on the invite form. The envelope names no field, so it is found from `error.field` when
 * the server sends one, else from the code (422 `invalid_locale`, 409 `identity_exists`, …), else from the message
 * of a 400/422 (`"email must be an email address"`, `"tenantName must be 2 to 120 characters"`).
 */
export function inviteFault(e: unknown): Fault {
	if (!(e instanceof ApiError))
		return { field: null, code: 'error', message: (e as Error)?.message ?? String(e), steps: [] };
	const steps = Array.isArray(e.details.steps) ? (e.details.steps as SignupStep[]) : [];
	const message = CONFLICT_TEXT[e.code] && e.status === 409 ? CONFLICT_TEXT[e.code]! : e.message;
	const named = typeof e.details.field === 'string' ? e.details.field : null;
	let field: InviteField | null = INVITE_FIELDS.find((f) => f === named) ?? null;
	if (!field && e.code === 'invalid_locale') field = 'locale';
	if (!field && (e.code === 'identity_exists' || e.code === 'user_belongs_to_another_workspace'))
		field = 'email';
	if (!field && (e.status === 400 || e.status === 422)) {
		const first = /^(\w+)\b/.exec(e.message)?.[1];
		field = INVITE_FIELDS.find((f) => f === first) ?? null;
	}
	return { field, code: e.code, message, steps };
}

// ---- requests in flight -----------------------------------------------------------------------------------------

/**
 * A counter for requests whose answers may arrive out of order: `next()` stamps a request, `current(t)` says
 * whether its answer is still the latest one asked for. A detail that arrives after another row was selected (or
 * after a newer read of the same row) is dropped instead of overwriting what is on screen.
 */
export class Latest {
	#n = 0;
	next(): number {
		return ++this.#n;
	}
	current(t: number): boolean {
		return t === this.#n;
	}
}

/**
 * Whether an answer about signup `id` stamped `token` may be shown: it is the latest request and that signup is
 * still the one selected.
 */
export function applies(latest: Latest, token: number, id: string, selected: string | null): boolean {
	return latest.current(token) && selected === id;
}

/** The selected signup's row replaced in a page of rows (an action's answer), others untouched. */
export function replaceRow(items: Signup[], s: Signup): Signup[] {
	return items.map((x) => (x.id === s.id ? s : x));
}

/**
 * Whether a failed approve / reject / invite / retry may have happened anyway: no answer from carebus-api itself
 * came back (network, 504, a body that is not ours, or a 502 that is not the route's own `<step code>` answer, which
 * always carries `error.steps`). An uncertain action is never repeated by the page: it re-reads the signup instead.
 */
export function uncertainFailure(e: unknown): boolean {
	if (!(e instanceof ApiError)) return false;
	if (e.status === 0 || e.status === 504 || e.code === 'bad_response') return true;
	return e.status === 502 && !Array.isArray(e.details.steps);
}

export const UNCERTAIN_ACTION_TEXT =
	'No answer came back, so it may have been carried out. Nothing is sent again by itself: the signup has been re-read, look at its state before you try again.';
