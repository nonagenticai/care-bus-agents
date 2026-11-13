// Laya gates page (spec 2026-10-18 §3): questions by point, the editor, the playground and the verdict log, against
// a stateful mock of `/api/jobs/{job}/gates*` laid over mockApi (later routes win in Playwright).
import { expect, test } from '@playwright/test';
import type { Page, Route } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { E2E_JOB, mockApi } from './mock-api';

// Kept beside the spec, not in fixtures/: those overlays must each mirror a generated types.ts fixture
// (tests/unit/contract.test.ts), and the gates routes have none until carebus-api emits them.
const fixture = () => JSON.parse(readFileSync('tests/e2e/gates.fixture.json', 'utf8'));
const GATES = `/admin/jobs/${E2E_JOB}/gates`;
const BASE = `http://api.e2e.test/api/jobs/${E2E_JOB}/gates`;

type Call = { method: string; path: string; body: unknown };

/** Stateful like routes::gates: a PUT upserts (201 when new), DELETE removes, evaluate returns the fixture. */
async function mockGates(page: Page, opts: { bound?: boolean } = {}): Promise<Call[]> {
	const fx = fixture();
	const questions: Record<string, unknown>[] = structuredClone(fx.questions);
	const calls: Call[] = [];
	const ok = (route: Route, body: unknown, status = 200) =>
		route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
	await page.route(`${BASE}**`, (route) => {
		const req = route.request();
		const url = new URL(req.url());
		const path = url.pathname.slice(`/api/jobs/${E2E_JOB}/gates`.length);
		const body = req.postData() ? req.postDataJSON() : null;
		calls.push({ method: req.method(), path: path + url.search, body });
		if (path === '' && req.method() === 'GET')
			return ok(route, {
				questions,
				provider: opts.bound === false ? { bound: false, model: null } : fx.provider,
			});
		if (path === '/evaluate' && req.method() === 'POST')
			return ok(
				route,
				opts.bound === false
					? {
							outcome: 'skipped',
							action: null,
							message: null,
							fired_keys: [],
							answers: null,
							latency_ms: 0,
						}
					: fx.evaluation,
			);
		if (path === '/verdicts' && req.method() === 'GET') return ok(route, fx.verdicts);
		const key = decodeURIComponent(path.slice(1));
		const i = questions.findIndex((q) => q.key === key);
		if (req.method() === 'PUT') {
			const row = { ...body, key, updated_at: '2026-10-18T11:00:00Z', updated_by: 'e2e' };
			if (i >= 0) questions[i] = row;
			else questions.push(row);
			return ok(route, row, i >= 0 ? 200 : 201);
		}
		if (req.method() === 'DELETE') {
			if (i < 0) return ok(route, { error: { code: 'not_found', message: `no gate ${key}` } }, 404);
			questions.splice(i, 1);
			return route.fulfill({ status: 204 });
		}
		return ok(route, { error: { code: 'not_mocked', message: path } }, 404);
	});
	return calls;
}

test('questions are grouped by point in flow order with their rule and the bound provider', async ({
	page,
}) => {
	await mockApi(page);
	await mockGates(page);
	await page.goto(GATES);
	await expect(page.getByRole('heading', { name: 'Laya gates' })).toBeVisible();
	await expect(page.getByTestId('gates-provider')).toContainText('laya-jev-1');
	const request = page.getByTestId('point-request');
	await expect(request.getByRole('row', { name: /emergency/ })).toContainText(
		'fires when noul ≥ 0.7 → handoff, fail-closed',
	);
	await expect(request.getByRole('row', { name: /frustration/ })).toContainText(
		'fires when score ≥ 3 (angry) → handoff, fail-open',
	);
	await expect(request.getByRole('row', { name: /topic/ })).toContainText(
		'fires when choice is warranty → annotate, fail-open',
	);
	await expect(request.getByLabel('Enabled topic')).not.toBeChecked();
	const tool = page.getByTestId('point-tool');
	await expect(tool.getByRole('row', { name: /customer_consent/ })).toContainText('noul < 0.5 → block');
	await expect(tool).toContainText('order_issue_refund, order_create_rma');
	await expect(page.getByTestId('point-response').getByRole('row', { name: /unbacked_claim/ })).toBeVisible();
	const nodes = page.locator('.flow .node');
	await expect(nodes).toHaveText([
		'Customer message',
		'bus → workers',
		'worker step',
		'tools',
		'reply',
		'customer',
	]);
});

test('with no laya binding the page says gates are off and links to Providers', async ({ page }) => {
	await mockApi(page);
	await mockGates(page, { bound: false });
	await page.goto(GATES);
	await expect(page.getByText('Gates are off')).toBeVisible();
	const callout = page.locator('.callout', { hasText: 'Gates are off' });
	await expect(callout.getByRole('link', { name: 'Providers' })).toHaveAttribute('href', '/admin/providers');
	await expect(page.getByTestId('gates-provider')).toHaveCount(0);
});

test('editing a question PUTs the normalised body; the toggle flips enabled', async ({ page }) => {
	await mockApi(page);
	const calls = await mockGates(page);
	await page.goto(GATES);
	await page.getByRole('button', { name: 'emergency' }).click();
	const drawer = page.getByRole('dialog', { name: 'emergency' });
	await drawer.getByLabel('Threshold', { exact: true }).fill('0.8');
	await drawer.getByLabel('Action').selectOption('block');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer).toBeHidden();
	const put = calls.find((c) => c.method === 'PUT' && c.path === '/emergency');
	expect(put!.body).toMatchObject({
		point: 'request',
		type: 'noul',
		criteria: { true: 'someone is in danger', false: 'no danger' },
		fire_when: 'gte',
		threshold: 0.8,
		fire_values: null,
		action: 'block',
		fail: 'closed',
		tools: null,
		enabled: true,
	});
	expect(put!.body).not.toHaveProperty('key');
	await expect(page.getByTestId('point-request').getByRole('row', { name: /emergency/ })).toContainText(
		'noul ≥ 0.8 → block',
	);

	await page.getByLabel('Enabled topic').check();
	await expect
		.poll(() => calls.find((c) => c.method === 'PUT' && c.path === '/topic')?.body)
		.toMatchObject({ enabled: true, fire_values: ['warranty'], threshold: null });
});

test('a new choice question is validated, then saved with its options; a question can be deleted', async ({
	page,
}) => {
	await mockApi(page);
	const calls = await mockGates(page);
	await page.goto(GATES);
	await page.getByRole('button', { name: 'Add response question' }).click();
	const drawer = page.getByRole('dialog', { name: 'New gate question' });
	await drawer.getByLabel('Key').fill('tone');
	await drawer.getByLabel('Instructions').fill('What tone does the reply take?');
	await drawer.getByLabel('Type').selectOption('choice');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer.getByText('Every option needs a description.')).toBeVisible();
	expect(calls.some((c) => c.method === 'PUT')).toBe(false);
	await expect(drawer.getByRole('button', { name: 'Remove option 1' })).toBeDisabled();

	await drawer.getByLabel('Option 1 name').fill('polite');
	await drawer.getByLabel('Option 1 description').fill('courteous');
	await drawer.getByLabel('Option 2 name').fill('rude');
	await drawer.getByLabel('Option 2 description').fill('dismissive');
	await drawer.getByRole('button', { name: 'Add option' }).click();
	await drawer.getByLabel('Option 3 name').fill('rude');
	await drawer.getByLabel('Option 3 description').fill('again');
	await expect(drawer.getByText('Option names must be distinct.')).toBeVisible();
	await drawer.getByRole('button', { name: 'Remove option 3' }).click();
	await drawer.getByLabel('Fires when').selectOption('in');
	await drawer.getByRole('checkbox', { name: 'rude' }).check();
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer).toBeHidden();
	expect(calls.find((c) => c.method === 'PUT' && c.path === '/tone')!.body).toMatchObject({
		point: 'response',
		type: 'choice',
		criteria: { polite: 'courteous', rude: 'dismissive' },
		fire_when: 'in',
		fire_values: ['rude'],
		threshold: null,
	});
	await expect(page.getByTestId('point-response').getByRole('row', { name: /tone/ })).toBeVisible();

	await page.getByRole('button', { name: 'unbacked_claim' }).click();
	const del = page.getByRole('dialog', { name: 'unbacked_claim' });
	await del.getByRole('button', { name: 'Delete…' }).click();
	await del.getByRole('button', { name: 'Delete', exact: true }).click();
	await expect(del).toBeHidden();
	expect(calls.some((c) => c.method === 'DELETE' && c.path === '/unbacked_claim')).toBe(true);
	await expect(page.getByRole('row', { name: /unbacked_claim/ })).toHaveCount(0);
});

test('a score question edits its ordered levels', async ({ page }) => {
	await mockApi(page);
	const calls = await mockGates(page);
	await page.goto(GATES);
	await page.getByRole('button', { name: 'frustration' }).click();
	const drawer = page.getByRole('dialog', { name: 'frustration' });
	await drawer.getByRole('button', { name: 'Move level 0 down' }).click();
	await expect(drawer.getByRole('textbox', { name: 'Level 0', exact: true })).toHaveValue('mild');
	await drawer.getByLabel('Threshold', { exact: true }).selectOption('4');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer).toBeHidden();
	expect(calls.find((c) => c.method === 'PUT' && c.path === '/frustration')!.body).toMatchObject({
		criteria: ['mild', 'calm', 'annoyed', 'angry', 'furious'],
		threshold: 4,
	});
});

test('a choice `in` rule takes an optional minimum confidence, sent and summarised', async ({ page }) => {
	await mockApi(page);
	const calls = await mockGates(page);
	await page.goto(GATES);
	await page.getByRole('button', { name: 'topic' }).click();
	const drawer = page.getByRole('dialog', { name: 'topic' });
	const min = drawer.getByLabel('Minimum confidence');
	await expect(min).toHaveValue('');
	await expect(drawer.getByLabel('Threshold', { exact: true })).toHaveCount(0);
	await min.fill('1.5');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer.getByText('The minimum confidence is between 0 and 1 (blank: any).')).toBeVisible();
	expect(calls.some((c) => c.method === 'PUT')).toBe(false);
	await min.fill('0.6');
	await drawer.getByLabel('Action').selectOption('block');
	await drawer.getByLabel('When laya fails').selectOption('closed');
	await expect(
		drawer.getByText('fires when choice is warranty with confidence ≥ 0.6 → block, fail-closed'),
	).toBeVisible();
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer).toBeHidden();
	expect(calls.find((c) => c.method === 'PUT' && c.path === '/topic')!.body).toMatchObject({
		fire_when: 'in',
		fire_values: ['warranty'],
		threshold: 0.6,
		action: 'block',
		fail: 'closed',
	});
	await expect(page.getByTestId('point-request').getByRole('row', { name: /topic/ })).toContainText(
		'fires when choice is warranty with confidence ≥ 0.6 → block, fail-closed',
	);

	// Leaving `in` needs a threshold again; coming back starts with none.
	await page.getByRole('button', { name: 'topic' }).click();
	const again = page.getByRole('dialog', { name: 'topic' });
	await again.getByLabel('Fires when').selectOption('gte');
	await expect(again.getByLabel('Threshold', { exact: true })).toHaveValue('0.6');
	await again.getByLabel('Fires when').selectOption('in');
	await expect(again.getByLabel('Minimum confidence')).toHaveValue('');
});

test('the playground shows the outcome and each answer, marking what fired', async ({ page }) => {
	await mockApi(page);
	const calls = await mockGates(page);
	await page.goto(GATES);
	await page.getByLabel('Customer message').fill('There is smoke coming out of my airfryer');
	await page.getByRole('button', { name: 'Evaluate' }).click();
	const result = page.getByTestId('pg-result');
	await expect(result).toContainText('Fired');
	await expect(result).toContainText('212 ms');
	await expect(result).toContainText('I am connecting you with a colleague right away.');
	await expect(result.getByTestId('answer-emergency')).toContainText('fired · handoff');
	await expect(result.getByTestId('answer-emergency')).toContainText('0.93');
	await expect(result.getByTestId('answer-frustration')).toContainText('annoyed');
	await expect(result.getByTestId('answer-frustration')).toContainText('no fire');
	await expect(result.getByTestId('answer-topic')).toContainText('delivery');
	await expect(result.getByTestId('answer-topic')).toContainText('80%');
	expect(calls.find((c) => c.path === '/evaluate')!.body).toEqual({
		point: 'request',
		state: 'There is smoke coming out of my airfryer',
	});

	await page.getByRole('button', { name: 'Tool', exact: true }).click();
	await page.getByLabel('Customer request').fill('refund my order');
	await page.getByLabel('Tool', { exact: true }).fill('order_issue_refund');
	await page.getByRole('button', { name: 'Evaluate' }).click();
	await expect
		.poll(() => calls.filter((c) => c.path === '/evaluate').at(-1)?.body)
		.toEqual({
			point: 'tool',
			state: 'refund my order',
			tool: 'order_issue_refund',
		});
});

test('the verdict log lists the latest verdicts and refreshes', async ({ page }) => {
	await mockApi(page);
	const calls = await mockGates(page);
	await page.goto(GATES);
	const log = page.getByTestId('verdicts');
	await expect(log.getByRole('row', { name: /conv-9/ })).toContainText('Failed closed (handoff)');
	await expect(log.getByRole('row', { name: /conv-9/ })).toContainText('CASE-7');
	await expect(log.getByRole('row', { name: /conv-9/ })).toContainText('laya timed out after 1500 ms');
	await expect(log.getByRole('row', { name: /playground/ })).toContainText('emergency');
	expect(calls.find((c) => c.path.startsWith('/verdicts'))!.path).toBe('/verdicts?limit=50');
});

test('a tenant_user sees the gates read-only', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await mockGates(page);
	await page.goto(GATES);
	await expect(page.getByRole('button', { name: 'Add request question' })).toBeDisabled();
	await expect(page.getByLabel('Enabled emergency')).toBeDisabled();
	// the playground calls laya on the tenant's token: admins only
	await expect(page.getByText('Admins only')).toBeVisible();
	await page.getByLabel('Customer message').fill('There is smoke coming out of my airfryer');
	await expect(page.getByRole('button', { name: 'Evaluate' })).toBeDisabled();
	await page.getByRole('button', { name: 'emergency' }).click();
	const drawer = page.getByRole('dialog', { name: 'emergency' });
	await expect(drawer.getByText('Read only')).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Save' })).toBeDisabled();
});
