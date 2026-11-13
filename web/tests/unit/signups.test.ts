import { describe, expect, it, vi } from 'vitest';
import { ApiError, createClient } from '../../src/lib/api/client';
import {
	Latest,
	applies,
	canRemove,
	canRetry,
	canRetryRemoval,
	emailNote,
	emailProblem,
	emptyInvite,
	inviteFault,
	maskEmail,
	repairLine,
	removalConfirmed,
	replaceRow,
	rowState,
	serverStatus,
	stepSummary,
	toInviteInput,
	uncertainEmail,
	uncertainFailure,
	validateInvite,
	visible,
	type Signup,
	type SignupStep,
} from '../../src/lib/api/signups';

const step = (s: string, status: SignupStep['status'], code: string | null = null): SignupStep => ({
	step: s,
	status,
	code,
	message: code ? `${code} happened` : null,
	attempts: 1,
	at: null,
	next_attempt_at: null,
});
const ALL_OK = [
	'keycloak_user',
	'tenant',
	'membership',
	'licences_tenant',
	'member_licence',
	'invite_email',
].map((s) => step(s, 'ok'));
const signup = (over: Partial<Signup> = {}): Signup => ({
	id: '0195f48b-5000-7000-8000-000000000001',
	email: 'anna@example.com',
	name: 'Anna de Vries',
	company: 'Acme',
	message: null,
	source: 'web',
	locale: 'nl',
	trust: 'intake',
	intake_client: 'care-bus-signup-intake',
	invited_by: null,
	status: 'pending',
	tenant_id: null,
	created_at: '2026-10-22T10:00:00Z',
	decided_at: null,
	decided_by: null,
	decided_via: null,
	reason: null,
	approving: false,
	reject_repair: null,
	suggested_tenant_name: 'Acme',
	steps: [],
	...over,
});
const withSteps = (steps: SignupStep[]) => signup({ status: 'approved', steps });
const uncertain = [...ALL_OK.slice(0, 5), step('invite_email', 'failed', 'invite_email_uncertain')];

describe('row state', () => {
	it('derives pending / approving / approved / provisioning / failed / rejected', () => {
		expect(rowState(signup())).toBe('pending');
		expect(rowState(signup({ approving: true }))).toBe('approving');
		expect(rowState(withSteps(ALL_OK))).toBe('approved');
		expect(rowState(withSteps([...ALL_OK.slice(0, 4), step('member_licence', 'pending')]))).toBe(
			'provisioning',
		);
		expect(rowState(withSteps(uncertain))).toBe('failed');
		expect(rowState(signup({ status: 'rejected' }))).toBe('rejected');
	});
	it('offers Retry only for an approved signup with a failed step', () => {
		expect(canRetry(withSteps(uncertain))).toBe(true);
		expect(canRetry(withSteps(ALL_OK))).toBe(false);
		expect(canRetry(signup({ steps: uncertain }))).toBe(false);
	});
	it('flags an email whose outcome is unknown, not an ordinary email failure', () => {
		expect(uncertainEmail(withSteps(uncertain))).toBe(true);
		const failed = [...ALL_OK.slice(0, 5), step('invite_email', 'failed', 'invite_email_failed')];
		expect(uncertainEmail(withSteps(failed))).toBe(false);
		expect(emailNote(uncertain)).toMatch(/unknown/);
		expect(emailNote(failed)).toMatch(/Retry/);
		expect(emailNote(ALL_OK)).toBe('set-password email sent');
		expect(emailNote([])).toBe('no email step yet');
	});
	it('summarises the journal', () => {
		const s = stepSummary(uncertain);
		expect([s.done, s.total, s.failed.map((x) => x.step)]).toEqual([5, 6, ['invite_email']]);
	});
});

describe('filters', () => {
	it('asks approved for Failed and keeps only the rows with a failed step', () => {
		expect(serverStatus('failed')).toBe('approved');
		expect(serverStatus('all')).toBe('all');
		const ok = { ...withSteps(ALL_OK), id: 'a' };
		const bad = { ...withSteps(uncertain), id: 'b' };
		expect(visible('failed', [ok, bad]).map((x) => x.id)).toEqual(['b']);
		expect(visible('approved', [ok, bad])).toHaveLength(2);
	});
});

describe('display', () => {
	it('masks an address and lines up a reject repair', () => {
		expect(maskEmail('anna@example.com')).toBe('an…@example.com');
		expect(maskEmail('a@x.io')).toBe('a…@x.io');
		expect(repairLine({ tenant: 'suspended', login: 'deleted' })).toBe('tenant suspended, login deleted');
		expect(repairLine({})).toBeNull();
		expect(repairLine(null)).toBeNull();
	});
	it('replaces only the matching row', () => {
		const a = signup({ id: 'a' });
		const b = signup({ id: 'b' });
		const next = replaceRow([a, b], { ...b, status: 'rejected' });
		expect(next.map((x) => x.status)).toEqual(['pending', 'rejected']);
	});
});

describe('stale answers', () => {
	it('drops a detail answer after another row was selected or a newer read started', () => {
		const l = new Latest();
		const t1 = l.next();
		expect(applies(l, t1, 'a', 'a')).toBe(true);
		expect(applies(l, t1, 'a', 'b')).toBe(false); // selection moved on
		const t2 = l.next();
		expect(applies(l, t1, 'a', 'a')).toBe(false); // superseded by t2
		expect(applies(l, t2, 'a', 'a')).toBe(true);
		expect(applies(l, t2, 'a', null)).toBe(false); // drawer closed
	});
});

describe('invite form', () => {
	it('validates like signup/mod.rs and omits empty optional fields', () => {
		expect(emailProblem('anna@example.com')).toBe('');
		for (const bad of ['', 'anna', 'a@b', 'a@@b.c', 'a b@c.d', '@c.d', 'a@.c', 'a@c.'])
			expect(emailProblem(bad)).not.toBe('');
		const d = { ...emptyInvite(), email: ' anna@example.com ', name: ' Anna ', tenantName: 'x' };
		expect(validateInvite(d)).toEqual({ tenantName: '2 to 120 characters.' });
		expect(validateInvite(emptyInvite())).toMatchObject({
			email: expect.any(String),
			name: 'Enter the name.',
		});
		expect(toInviteInput({ ...d, tenantName: '' })).toEqual({ email: 'anna@example.com', name: 'Anna' });
		expect(toInviteInput({ ...d, company: ' Acme ', tenantName: 'Acme EU', locale: 'de' })).toEqual({
			email: 'anna@example.com',
			name: 'Anna',
			company: 'Acme',
			tenantName: 'Acme EU',
			locale: 'de',
		});
	});
	it('names the field of a refusal from error.field, the code, or the message', () => {
		expect(inviteFault(new ApiError(422, 'invalid_locale', 'locale must be one of ar, de')).field).toBe(
			'locale',
		);
		expect(inviteFault(new ApiError(400, 'invalid_body', 'email must be an email address')).field).toBe(
			'email',
		);
		expect(
			inviteFault(new ApiError(400, 'invalid_body', 'tenantName must be 2 to 120 characters')).field,
		).toBe('tenantName');
		expect(
			inviteFault(new ApiError(422, 'invalid_input', 'bad', { code: 'invalid_input', field: 'company' }))
				.field,
		).toBe('company');
		const conflict = inviteFault(new ApiError(409, 'identity_exists', 'raw'));
		expect(conflict.field).toBe('email');
		expect(conflict.message).toMatch(/already exists/);
		expect(inviteFault(new ApiError(503, 'signup_not_configured', 'off')).field).toBeNull();
	});
	it('keeps the journal of a 502 step failure', () => {
		const steps = [step('keycloak_user', 'ok'), step('tenant', 'failed', 'tenant_insert_failed')];
		const f = inviteFault(new ApiError(502, 'tenant_insert_failed', 'x', { steps }));
		expect(f.steps).toHaveLength(2);
		expect(f.field).toBeNull();
	});
});

describe('uncertain outcomes', () => {
	it('treats no answer, 504, a foreign body and a bare 502 as uncertain; a route 502 with steps is not', () => {
		expect(uncertainFailure(new ApiError(0, 'unavailable', 'x'))).toBe(true);
		expect(uncertainFailure(new ApiError(504, 'http_504', 'x'))).toBe(true);
		expect(uncertainFailure(new ApiError(200, 'bad_response', 'x'))).toBe(true);
		expect(uncertainFailure(new ApiError(502, 'http_502', 'x'))).toBe(true);
		expect(uncertainFailure(new ApiError(502, 'tenant_failed', 'x', { steps: [] }))).toBe(false);
		expect(uncertainFailure(new ApiError(409, 'signup_not_pending', 'x'))).toBe(false);
		expect(uncertainFailure(new Error('x'))).toBe(false);
	});
});

describe('client routes', () => {
	const json = (body: unknown) =>
		new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } });
	it('calls the signup-approvals routes with the documented bodies', async () => {
		const f = vi.fn().mockImplementation(async () => json({}));
		const a = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: f });
		await a.signups({ status: 'approved', before: '123.abc' });
		await a.signup('id 1');
		await a.approveSignup('i');
		await a.approveSignup('i', 'Acme EU');
		await a.rejectSignup('i');
		await a.rejectSignup('i', 'spam');
		await a.retrySignup('i');
		await a.inviteSignup({ email: 'a@b.co', name: 'A', locale: 'pl' });
		const calls = f.mock.calls.map(([url, init]) => [init.method, url, init.body ?? null]);
		expect(calls).toEqual([
			['GET', 'https://api.test/api/signup-approvals?status=approved&before=123.abc', null],
			['GET', 'https://api.test/api/signup-approvals/id%201', null],
			['POST', 'https://api.test/api/signup-approvals/i/approve', '{}'],
			['POST', 'https://api.test/api/signup-approvals/i/approve', '{"tenantName":"Acme EU"}'],
			['POST', 'https://api.test/api/signup-approvals/i/reject', '{}'],
			['POST', 'https://api.test/api/signup-approvals/i/reject', '{"reason":"spam"}'],
			['POST', 'https://api.test/api/signup-approvals/i/retry', null],
			['POST', 'https://api.test/api/signup-approvals/invite', '{"email":"a@b.co","name":"A","locale":"pl"}'],
		]);
	});
});

describe('removal of an approved signup', () => {
	const approved = signup({
		status: 'approved',
		tenant_id: '03533460-c986-4000-8000-0000deadbeef',
		steps: ALL_OK,
	});

	it('is offered for an approved signup with a workspace only', () => {
		expect(canRemove(approved)).toBe(true);
		expect(canRemove({ ...approved, tenant_id: null })).toBe(false);
		expect(canRemove(signup())).toBe(false);
		expect(canRemove({ ...approved, status: 'rejected' })).toBe(false);
		expect(canRemove({ ...approved, status: 'removed' })).toBe(false);
	});

	it('shows the removal state and offers Retry until it is done', () => {
		const removed = (removal_state: Signup['removal_state']) =>
			({ ...approved, status: 'removed', removal_state }) as Signup;
		expect(rowState(removed('pending'))).toBe('removing');
		expect(rowState(removed('failed'))).toBe('removal_stuck');
		expect(rowState(removed('blocked'))).toBe('removal_stuck');
		expect(rowState(removed('done'))).toBe('removed');
		expect(canRetryRemoval(removed('failed'))).toBe(true);
		expect(canRetryRemoval(removed('blocked'))).toBe(true);
		expect(canRetryRemoval(removed('pending'))).toBe(true);
		expect(canRetryRemoval(removed('done'))).toBe(false);
		expect(canRetryRemoval(approved)).toBe(false);
		expect(serverStatus('removed')).toBe('removed');
	});

	it('is confirmed only by the exact workspace slug', () => {
		expect(removalConfirmed(' acme-deadbeef ', 'acme-deadbeef')).toBe(true);
		expect(removalConfirmed('acme', 'acme-deadbeef')).toBe(false);
		expect(removalConfirmed('', '')).toBe(false);
		expect(removalConfirmed('acme-deadbeef', null)).toBe(false);
	});

	it('posts the confirmation and the retry to the removal routes', async () => {
		const calls: [string, string, unknown][] = [];
		const f = vi.fn(async (url: string, init?: RequestInit) => {
			calls.push([init?.method ?? 'GET', url, init?.body ? JSON.parse(init.body as string) : undefined]);
			return new Response(JSON.stringify({ ...approved, status: 'removed', removal_state: 'done' }), {
				status: 200,
				headers: { 'content-type': 'application/json' },
			});
		});
		const c = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: f as never });
		await c.removeSignup(approved.id, 'acme-deadbeef');
		await c.retryRemoval(approved.id);
		expect(calls.map(([m, u]) => [m, u.replace('https://api.test', '')])).toEqual([
			['POST', `/api/signup-approvals/${approved.id}/remove`],
			['POST', `/api/signup-approvals/${approved.id}/removal/retry`],
		]);
		expect(calls[0]![2]).toEqual({ confirm: 'acme-deadbeef' });
	});
});
