import { expect, test } from '@playwright/test';
import { randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { apiUrl, mockApi } from './mock-api';

test('sources list shows each source with its last run, and a failed run shows its error', async ({
	page,
}) => {
	await mockApi(page);
	await page.goto('/admin/knowledge');
	await expect(page.getByRole('cell', { name: 'Customer API' })).toBeVisible();
	await expect(page.getByRole('cell', { name: 'Help centre' })).toBeVisible();
	await expect(page.getByText('Failed', { exact: true })).toBeVisible();
	await expect(page.getByText(/the source answered HTTP 401/).first()).toBeVisible();
	await expect(page.getByText('Completed', { exact: true })).toBeVisible();
});

test('adding an API source sends the secret once; afterwards it only shows that one is set', async ({
	page,
}) => {
	const sent = await mockApi(page);
	// Built at runtime so no secret-looking literal is committed.
	const secret = `e2e-${randomBytes(12).toString('hex')}`;
	await page.goto('/admin/knowledge');
	await page.getByRole('button', { name: 'Add source' }).click();
	await page.getByRole('radio', { name: /^API/ }).check();
	await page.getByLabel('Name', { exact: true }).fill('Customer API');
	await page.getByLabel('Endpoint URL').fill('https://api.example.nl/v2/customers');
	await page.getByLabel('Authentication').selectOption('bearer');
	await page.getByLabel('Credentials secret').fill(secret);
	await page.getByLabel('Target store').selectOption({ label: 'returns' });
	await page.getByRole('button', { name: 'Add source' }).last().click();
	// Wait for the response to land (the "set" chip) before reading what was sent.
	await expect(page.locator('.drawer .chip', { hasText: /^set$/ })).toBeVisible();
	const post = sent.find((r) => r.method === 'POST' && r.path === '/api/sources');
	expect(post, 'the create request').toBeTruthy();
	expect(JSON.parse(post!.body!).credentials).toEqual({ secret });
	await expect(page.getByLabel('Credentials secret')).toHaveValue('');
	expect(await page.content()).not.toContain(secret);
	expect(await page.locator('body').innerText()).not.toContain(secret);
});

test('an unsupported run says so in the run history, with its error', async ({ page }) => {
	const id = '00000000-0000-4000-8000-000000000001';
	const runs = JSON.parse(readFileSync('src/lib/api/fixtures/source_runs.json', 'utf8'));
	runs.items[0].status = 'unsupported';
	runs.items[0].error = 'strata_ingest refused the request (HTTP 400): unknown field `source`';
	await mockApi(page, { override: { [`/api/sources/${id}/runs`]: runs } });
	await page.goto('/admin/knowledge');
	await page.getByRole('cell', { name: 'Customer API' }).getByRole('button').click();
	const history = page.locator('.drawer section.runs');
	await expect(history.getByText('Not supported by strata_ingest yet')).toBeVisible();
	await expect(history.getByText(/unknown field `source`/)).toBeVisible();
});

test('mapping rows pick Entity.attribute from the ontology', async ({ page }) => {
	await mockApi(page);
	await page.goto('/admin/knowledge');
	await page.getByRole('cell', { name: 'Customer API' }).getByRole('button').click();
	const targets = page.getByRole('combobox', { name: 'Ontology attribute' });
	await expect(targets.first()).toHaveValue('Customer.customer_id');
	await expect(targets.first().locator('option', { hasText: 'Customer.email (PII)' })).toHaveCount(1);
	await page.getByRole('button', { name: 'Add mapping' }).click();
	await expect(targets).toHaveCount(3);
});

test('delete keeps the documents unless the purge box is ticked', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/knowledge');
	await page.getByRole('cell', { name: 'Customer API' }).getByRole('button').click();
	await page.getByRole('button', { name: 'Delete', exact: true }).click();
	const dialog = page.getByRole('alertdialog', { name: 'Delete source' });
	await expect(dialog).toContainText('Its documents stay in strata_rag.');
	await dialog.getByRole('button', { name: 'Delete source' }).click();
	// qs() keeps a boolean false, so the default delete sends purge=false, never purge=true.
	await expect
		.poll(() => sent.find((r) => r.method === 'DELETE' && r.path.startsWith('/api/sources/'))?.search)
		.toBe('?purge=false');
	await expect(dialog).toHaveCount(0);
	sent.length = 0;
	await page.getByRole('cell', { name: 'Customer API' }).getByRole('button').click();
	await page.getByRole('button', { name: 'Delete', exact: true }).click();
	await dialog.getByLabel('Also delete its documents').check();
	await expect(dialog).toContainText('are deleted from strata_rag too');
	await dialog.getByRole('button', { name: 'Delete source' }).click();
	await expect
		.poll(() => sent.find((r) => r.method === 'DELETE' && r.path.startsWith('/api/sources/'))?.search)
		.toBe('?purge=true');
});

test('a tenant_user sees re-sync disabled with the role hint', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await page.goto('/admin/knowledge');
	await page.getByRole('cell', { name: 'Help centre' }).getByRole('button').click();
	const resync = page.getByRole('button', { name: 'Re-sync' });
	await expect(resync).toBeDisabled();
	await expect(resync).toHaveAttribute('title', 'Needs the tenant_admin role');
});

test('ingest jobs show steps with provider, model and cost in euros', async ({ page }) => {
	await mockApi(page);
	await page.goto('/admin/knowledge/jobs');
	await page
		.getByRole('row', { name: /help\.example\.nl\/returns/ })
		.first()
		.getByRole('button')
		.click();
	// The list row shows the same total, so every lookup is scoped to the drawer.
	const drawer = page.locator('.drawer');
	await expect(drawer.getByRole('cell', { name: 'vllm' })).toBeVisible();
	await expect(drawer.getByRole('cell', { name: 'local-llm' })).toBeVisible();
	await expect(drawer.getByRole('cell', { name: '€0.0018' })).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Retry' })).toBeDisabled();
});

test('search preview cites each passage by title and pages', async ({ page }) => {
	const query = JSON.parse(readFileSync('src/lib/api/fixtures/knowledge_query.json', 'utf8'));
	const hit = query.results[0];
	const noPages = { ...hit, chunk_id: `${hit.chunk_id}-2`, title: null, page_start: null, page_end: null };
	const onePage = { ...hit, chunk_id: `${hit.chunk_id}-3`, page_start: 7, page_end: 7 };
	await mockApi(page, {
		override: { '/api/knowledge/query': { ...query, results: [hit, noPages, onePage] } },
	});
	await page.goto('/admin/knowledge/stores');
	await page.getByLabel('Query').fill('refund window');
	await page.getByRole('button', { name: 'Search', exact: true }).click();
	const cites = page.getByTestId('hit-citation');
	await expect(cites).toHaveText([
		'Returns policy · p. 2–3',
		'policies/returns.pdf',
		'Returns policy · p. 7',
	]);
});

test('an ingest or strata_rag outage shows Unavailable, never an empty list', async ({ page }) => {
	await mockApi(page, { down: ['/api/ingest', '/api/knowledge'] });
	await page.goto('/admin/knowledge/jobs');
	await expect(page.getByText('Unavailable', { exact: true }).first()).toBeVisible();
	await expect(page.getByText('No ingest jobs match.')).toHaveCount(0);
	await page.goto('/admin/knowledge/stores');
	await expect(page.getByText('Unavailable', { exact: true }).first()).toBeVisible();
	await expect(page.getByText('No stores in strata_rag yet.')).toHaveCount(0);
});

test('a CSV without a header row is previewed with its first row as data', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/knowledge');
	await page.getByRole('button', { name: 'Add source' }).click();
	await page.getByRole('radio', { name: /^Datatable/ }).check();
	const box = page.getByLabel('First row contains column names');
	await expect(box).toBeChecked();
	await box.uncheck();
	await page
		.locator('input[type=file]')
		.first()
		.setInputFiles({
			name: 'klanten.csv',
			mimeType: 'text/csv',
			buffer: Buffer.from('C-1;Anna\nC-2;Bram\n'),
		});
	await expect(page.getByText('Columns read. Nothing was stored.')).toBeVisible();
	const post = sent.find((r) => r.method === 'POST' && r.path === '/api/sources/preview-columns');
	expect(post, 'the preview request').toBeTruthy();
	expect(post!.body).toMatch(/name="header_row"\r\n\r\nfalse/);
});

test('test connection on an unsaved API source tests the form as is and saves nothing', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/knowledge');
	await page.getByRole('button', { name: 'Add source' }).click();
	await page.getByRole('radio', { name: /^API/ }).check();
	const drawer = page.locator('.drawer');
	const testBtn = drawer.getByRole('button', { name: 'Test connection' });
	await expect(testBtn).toBeEnabled();
	// No URL yet: says what is missing, sends nothing.
	await testBtn.click();
	await expect(drawer.getByText('Cannot test yet')).toBeVisible();
	await expect(drawer.getByText('The URL must start with http:// or https://.').first()).toBeVisible();
	expect(sent.some((r) => r.path.endsWith('/test'))).toBe(false);

	// No name and no target store are needed to test.
	await page.getByLabel('Endpoint URL').fill('https://jsonplaceholder.typicode.com/users');
	await expect(drawer.getByText('Cannot test yet')).toHaveCount(0);
	await expect(drawer.getByText('Tests the settings above. Nothing is saved.')).toBeVisible();
	await testBtn.click();
	await expect(drawer.getByText('Connection works')).toBeVisible();
	await expect(drawer.getByText('HTTP 200 · reachable')).toBeVisible();
	await expect(drawer.getByText('Discovered fields: customer_id, email, name')).toBeVisible();
	const tested = sent.filter((r) => r.method === 'POST' && r.path === '/api/sources/test');
	expect(tested).toHaveLength(1);
	expect(JSON.parse(tested[0]!.body!)).toEqual({
		type: 'api',
		config: { url: 'https://jsonplaceholder.typicode.com/users', auth: { type: 'none' } },
	});
	expect(sent.some((r) => r.path === '/api/sources' && r.method === 'POST')).toBe(false);
	expect(
		sent.some((r) => /^\/api\/sources\/[^/]+\/test$/.test(r.path) && r.path !== '/api/sources/test'),
	).toBe(false);

	const before = await drawer.getByRole('combobox', { name: 'Ontology attribute' }).count();
	await drawer.getByRole('button', { name: /^Map all \d+ fields?$/ }).click();
	const targets = drawer.getByRole('combobox', { name: 'Ontology attribute' });
	await expect(targets).not.toHaveCount(before);
	await expect(drawer.getByRole('button', { name: /^Map all/ })).toHaveCount(0);
});

test('an edited saved source is tested with the edits, without saving them', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/knowledge');
	await page.getByRole('button', { name: 'Customer API' }).click();
	const drawer = page.locator('.drawer');
	const id = '00000000-0000-4000-8000-000000000001';
	const testBtn = drawer.getByRole('button', { name: 'Test connection' });

	// Unchanged: the stored settings, no body.
	await testBtn.click();
	await expect(drawer.getByText('Connection works')).toBeVisible();
	const first = sent.filter((r) => r.method === 'POST' && r.path === `/api/sources/${id}/test`);
	expect(first).toHaveLength(1);
	expect(first[0]!.body ?? '').toBe('');

	// A path edit keeps the stored secret: the override carries the config only.
	await page.getByLabel('Endpoint URL').fill('https://oms.example.com/v3/customers');
	await expect(drawer.getByText('Tests your edits without saving them.')).toBeVisible();
	await testBtn.click();
	await expect.poll(() => sent.filter((r) => r.path === `/api/sources/${id}/test`).length).toBe(2);
	const edited = sent.filter((r) => r.path === `/api/sources/${id}/test`)[1]!;
	expect(JSON.parse(edited.body!)).toEqual({
		config: {
			url: 'https://oms.example.com/v3/customers',
			auth: { type: 'header', header_name: 'X-Api-Key' },
		},
	});
	expect(sent.some((r) => r.method === 'PUT')).toBe(false);

	// A different host needs the secret again; nothing is sent until it is typed.
	await page.getByLabel('Endpoint URL').fill('https://elsewhere.example.nl/customers');
	await testBtn.click();
	await expect(drawer.getByText(/Enter the secret again to test a different host/)).toBeVisible();
	expect(sent.filter((r) => r.path === `/api/sources/${id}/test`)).toHaveLength(2);
});

test('a refused test address is shown inline, not swallowed', async ({ page }) => {
	await mockApi(page);
	await page.route(apiUrl('/api/sources/*/test'), (route) =>
		route.fulfill({
			status: 400,
			contentType: 'application/json',
			body: JSON.stringify({
				error: { code: 'address_refused', message: '127.0.0.1 is a loopback address' },
			}),
		}),
	);
	await page.goto('/admin/knowledge');
	await page.getByRole('button', { name: 'Customer API' }).click();
	const drawer = page.locator('.drawer');
	await drawer.getByRole('button', { name: 'Test connection' }).click();
	await expect(drawer.getByText('Test failed')).toBeVisible();
	await expect(drawer.getByText(/127\.0\.0\.1 is a loopback address/)).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Test connection' })).toBeEnabled();
});

test('a lost save answer re-reads the source: unchanged there means not saved, edits kept', async ({
	page,
}) => {
	await mockApi(page);
	const id = '00000000-0000-4000-8000-000000000001';
	let puts = 0;
	await page.route(apiUrl(`/api/sources/${id}`), (route) => {
		if (route.request().method() !== 'PUT') return route.fallback();
		puts++;
		return route.abort('connectionreset');
	});
	await page.goto('/admin/knowledge');
	await page.getByRole('button', { name: 'Customer API' }).click();
	const drawer = page.locator('.drawer');
	await drawer.getByLabel('Schedule (cron)').fill('0 4 * * *');
	await drawer.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(drawer.getByRole('status').filter({ hasText: /^Not saved/ })).toHaveText(
		'Not saved — your changes are still here; you can save again.',
	);
	await expect(drawer.getByLabel('Schedule (cron)')).toHaveValue('0 4 * * *');
	expect(puts, 'never resubmitted').toBe(1);
});

test('a lost save answer that the re-read confirms is reported as saved', async ({ page }) => {
	await mockApi(page);
	const id = '00000000-0000-4000-8000-000000000001';
	await page.route(apiUrl(`/api/sources/${id}`), (route) =>
		route.request().method() === 'PUT' ? route.abort('connectionreset') : route.fallback(),
	);
	await page.goto('/admin/knowledge');
	await page.getByRole('button', { name: 'Customer API' }).click();
	const drawer = page.locator('.drawer');
	await drawer.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(drawer.getByRole('status').filter({ hasText: /^Saved/ })).toHaveText(
		'Saved — confirmed after reloading.',
	);
});
