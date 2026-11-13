import { expect, test, type Page, type Route } from '@playwright/test';
import { E2E_TENANT, mockApi } from './mock-api';

const API = 'http://api.e2e.test';
const SUPER = {
	sub: 'e2e',
	actor: 'e2e',
	super_admin: true,
	release_writer: false,
	tenant: null,
	active_tenant: E2E_TENANT,
};
const A = '0195f48b-5000-7000-8000-00000000000a';
const B = '0195f48b-5000-7000-8000-00000000000b';
const C = '0195f48b-5000-7000-8000-00000000000c';
const STEPS = ['keycloak_user', 'tenant', 'membership', 'licences_tenant', 'member_licence', 'invite_email'];
const step = (s: string, status: string, code: string | null = null) => ({
	step: s,
	status,
	code,
	message: code ? 'no answer from Keycloak within 10 s' : null,
	attempts: 1,
	at: '2026-10-22T10:05:00Z',
	next_attempt_at: null,
});
const row = (id: string, over: Record<string, unknown> = {}) => ({
	id,
	email: `${id.slice(-1)}person@example.com`,
	name: `Person ${id.slice(-1).toUpperCase()}`,
	company: `Company ${id.slice(-1).toUpperCase()}`,
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
	suggested_tenant_name: `Company ${id.slice(-1).toUpperCase()}`,
	steps: [] as ReturnType<typeof step>[],
	...over,
});

/** routes::signups, stateful: approve moves a row to approved with its journal, reject to rejected. */
async function mockSignups(page: Page, opts: { off?: boolean; slow?: Record<string, number> } = {}) {
	const rows = new Map<string, ReturnType<typeof row>>([
		[A, row(A)],
		[B, row(B)],
		[
			C,
			row(C, {
				status: 'approved',
				tenant_id: '0195f48b-d000-7000-8000-0000000000cc',
				decided_at: '2026-10-22T10:05:00Z',
				decided_by: 'e2e',
				decided_via: 'ui',
				steps: [
					...STEPS.slice(0, 5).map((s) => step(s, 'ok')),
					step('invite_email', 'failed', 'invite_email_uncertain'),
				],
			}),
		],
	]);
	const sent: { method: string; path: string; body: string | null }[] = [];
	const ok = (route: Route, body: unknown, status = 200) =>
		route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
	await page.route(`${API}/api/signup-approvals**`, async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();
		sent.push({ method, path: url.pathname, body: route.request().postData() });
		if (opts.off)
			return ok(
				route,
				{
					error: {
						code: 'signup_not_configured',
						message: 'early-access signup is not enabled for care-bus',
					},
				},
				503,
			);
		const [, , id, action] = url.pathname.split('/').filter(Boolean);
		if (!id && method === 'GET') {
			const status = url.searchParams.get('status') ?? 'all';
			const items = [...rows.values()].filter(
				(r) => status === 'all' || r.status === status || (status === 'decided' && r.status !== 'pending'),
			);
			const count = (s: string) => [...rows.values()].filter((r) => r.status === s).length;
			return ok(route, {
				items,
				next: null,
				queue_cap: 50,
				pending: count('pending'),
				counts: { received: rows.size, approved: count('approved'), rejected: count('rejected') },
				as_of: '2026-10-22T12:00:00Z',
			});
		}
		if (id === 'invite' && method === 'POST') {
			const b = route.request().postDataJSON();
			if (b.email === 'taken@example.com')
				return ok(
					route,
					{ error: { code: 'identity_exists', message: 'a login for this address exists' } },
					409,
				);
			if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(b.email))
				return ok(route, { error: { code: 'invalid_body', message: 'email must be an email address' } }, 400);
			const r = row(`0195f48b-5000-7000-8000-00000000000d`, {
				...b,
				source: 'invite',
				status: 'approved',
				steps: STEPS.map((s) => step(s, 'ok')),
			});
			rows.set(r.id, r);
			return ok(route, {
				signup: r,
				tenant: { id: 't-new', name: b.tenantName ?? b.company ?? b.name, status: 'active' },
				steps: r.steps,
				reused_pending: false,
			});
		}
		const r = rows.get(id ?? '');
		if (!r) return ok(route, { error: { code: 'signup_not_found', message: `no signup "${id}"` } }, 404);
		const delay = opts.slow?.[r.id];
		if (method === 'GET') {
			if (delay) await new Promise((res) => setTimeout(res, delay));
			return ok(route, r);
		}
		if (action === 'approve') {
			await new Promise((res) => setTimeout(res, 400)); // long enough to see the in-flight label
			const b = route.request().postDataJSON() ?? {};
			Object.assign(r, {
				status: 'approved',
				tenant_id: 't-1',
				decided_at: '2026-10-22T12:00:00Z',
				decided_by: 'e2e',
				decided_via: 'care-bus onboarding page',
				steps: STEPS.map((s) => step(s, 'ok')),
			});
			return ok(route, {
				signup: r,
				tenant: { id: 't-1', name: b.tenantName ?? r.suggested_tenant_name, status: 'active' },
				steps: r.steps,
			});
		}
		if (action === 'reject') {
			Object.assign(r, { status: 'rejected', decided_at: '2026-10-22T12:00:00Z', reject_repair: {} });
			return ok(route, { ...r, steps: [] });
		}
		if (action === 'retry') {
			r.steps = r.steps.map((s) => (s.status === 'failed' ? step(s.step, 'ok') : s));
			return ok(route, { requeued: 1, steps: r.steps });
		}
		return ok(route, { error: { code: 'not_mocked', message: url.pathname } }, 404);
	});
	return sent;
}

test('a super_admin finds Onboarding in the nav and approves a pending signup', async ({ page }) => {
	await mockApi(page, { me: SUPER });
	const sent = await mockSignups(page);
	await page.goto('/admin/onboarding');
	await expect(page.getByTestId('signup-row')).toHaveCount(2);
	await expect(page.getByTestId('queue-meta')).toContainText('2 of 50 waiting');
	const rowA = page.locator(`[data-testid="signup-row"][data-id="${A}"]`);
	await rowA.getByRole('button', { name: 'Approve…' }).click();
	const form = page.getByTestId('approve-form');
	await expect(form.getByLabel('Workspace name')).toHaveValue('Company A');
	await form.getByLabel('Workspace name').fill('Company A EU');
	await form.getByRole('button', { name: 'Approve and create workspace' }).click();
	await expect(form.getByRole('button', { name: 'Approving…' })).toBeDisabled();
	await expect(page.getByTestId('onboarding-notice')).toContainText('workspace “Company A EU” created');
	expect(sent.find((s) => s.path.endsWith('/approve'))?.body).toBe('{"tenantName":"Company A EU"}');
	await expect(page.getByTestId('signup-detail').getByTestId('provisioning-step')).toHaveCount(6);
	await expect(page.getByTestId('signup-row')).toHaveCount(1); // A left the pending filter
});

test('Onboarding is not in the nav for a tenant_admin, and the page refuses', async ({ page, isMobile }) => {
	await mockApi(page);
	const sent = await mockSignups(page);
	await page.goto('/admin/onboarding');
	await expect(page.getByText('Only a platform super admin reviews early-access signups.')).toBeVisible();
	if (!isMobile) await expect(page.getByRole('link', { name: 'Onboarding' })).toHaveCount(0);
	expect(sent).toHaveLength(0);
});

test('the feature switched off shows the off state, not an error or an empty queue', async ({ page }) => {
	await mockApi(page, { me: SUPER });
	await mockSignups(page, { off: true });
	await page.goto('/admin/onboarding');
	await expect(page.getByTestId('signups-off')).toContainText('Early access is off');
	await expect(page.getByTestId('signups-off')).toContainText('signup_not_configured');
	await expect(page.getByTestId('invite-open')).toHaveCount(0);
});

test('a ?signup= deep link opens that signup; an uncertain email asks before Retry', async ({ page }) => {
	await mockApi(page, { me: SUPER });
	const sent = await mockSignups(page);
	await page.goto(`/admin/onboarding?signup=${C}`);
	const detail = page.getByTestId('signup-detail');
	await expect(detail).toHaveAttribute('data-id', C);
	await expect(detail.getByText('Email outcome unknown')).toBeVisible();
	// Raw step errors sit behind a toggle.
	const raw = detail.locator('[data-step="invite_email"] details');
	await expect(raw.getByText('invite_email_uncertain')).toBeHidden();
	await raw.locator('summary').click();
	await expect(raw.getByText('invite_email_uncertain')).toBeVisible();
	await detail.getByRole('button', { name: 'Retry…' }).click();
	await expect(page.getByTestId('retry-confirm')).toContainText('the first one stops working');
	expect(sent.some((s) => s.path.endsWith('/retry'))).toBe(false);
	await page.getByRole('button', { name: 'Retry failed steps' }).click();
	await expect(detail.locator('[data-step="invite_email"]')).toHaveAttribute('data-status', 'ok');
});

test('a slow detail of an earlier selection never replaces the current one', async ({ page }) => {
	await mockApi(page, { me: SUPER });
	await mockSignups(page, { slow: { [A]: 1500 } });
	await page.goto('/admin/onboarding');
	await page.getByRole('button', { name: 'Person A' }).click();
	await page.keyboard.press('Escape');
	await page.getByRole('button', { name: 'Person B' }).click();
	const detail = page.getByTestId('signup-detail');
	await expect(detail).toHaveAttribute('data-id', B);
	await page.waitForTimeout(1800); // A's answer has arrived by now
	await expect(detail).toHaveAttribute('data-id', B);
	await expect(page.getByRole('dialog')).toContainText('bperson@example.com');
});

test('invite: locale picker, client checks and a refusal shown on its field', async ({ page }) => {
	await mockApi(page, { me: SUPER });
	const sent = await mockSignups(page);
	await page.goto('/admin/onboarding');
	await page.getByTestId('invite-open').click();
	const form = page.getByTestId('invite-form');
	await form.getByRole('button', { name: 'Create workspace and send invite' }).click();
	await expect(form.getByText('Enter the name.')).toBeVisible();
	expect(sent.some((s) => s.path.endsWith('/invite'))).toBe(false);
	await form.getByLabel('Email', { exact: true }).fill('taken@example.com');
	await form.getByLabel('Name', { exact: true }).fill('Jan Kowalski');
	await form.getByLabel('Email language').selectOption('pl');
	await form.getByRole('button', { name: 'Create workspace and send invite' }).click();
	await expect(form.locator('#inv-err-email')).toContainText('already exists');
	await expect(form.getByLabel('Email', { exact: true })).toHaveAttribute('aria-invalid', 'true');
	await form.getByLabel('Email', { exact: true }).fill('jan@example.pl');
	await form.getByRole('button', { name: 'Create workspace and send invite' }).click();
	await expect(page.getByTestId('onboarding-notice')).toContainText('email in Polish');
	const body = JSON.parse(sent.filter((s) => s.path.endsWith('/invite')).at(-1)!.body!);
	expect(body).toEqual({ email: 'jan@example.pl', name: 'Jan Kowalski', locale: 'pl' });
});
