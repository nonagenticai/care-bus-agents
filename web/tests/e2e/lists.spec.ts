// List search and sort are the server's: the request carries q/sort/dir/request_type and its answer is what the
// table shows. The mock filters and orders like the server, so fewer rows prove the server, not the client, did it.
import { readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import { apiUrl, E2E_JOB, mockApi } from './mock-api';

// Sorting lives in the table head, which the phone layout deliberately replaces with per-row labels; run this
// file at a wide viewport in both projects (the stacked phone layout is covered by phone-tables.spec.ts).
test.use({ viewport: { width: 1280, height: 900 } });

const job = (path: string) => `/admin/jobs/${E2E_JOB}${path}`;

const caseRow = (id: string, customer: string, work: number, ms: number, request_type = 'return') => ({
	case_id: id,
	conversation_id: null,
	customer_id: customer,
	request_type,
	area: 'order-support',
	context_space: 'retail:order-support:returns',
	outcome: 'completed',
	reason: null,
	work_items: work,
	retries: 0,
	started_at: '2026-10-15T10:00:01.000000Z',
	resolved_at: '2026-10-15T10:00:02.000000Z',
	duration_ms: ms,
	procedure: null,
});

test('the cases list searches and sorts on the server', async ({ page }) => {
	// The cases route shadows mockApi's, so these assertions read a local request log (mockApi's Sent[] never sees
	// them): `asked.at(-1)` is the query the list just answered.
	const asked: URL[] = [];
	const cases = [
		caseRow('CASE-1', 'C-1', 9, 4000),
		caseRow('CASE-2', 'C-2', 1, 1000),
		caseRow('CASE-3', 'C-3', 5, 2000, 'warranty_claim'),
	];
	await mockApi(page);
	await page.route(apiUrl('/api/cases?*'), async (route) => {
		const url = new URL(route.request().url());
		asked.push(url);
		const q = (url.searchParams.get('q') ?? '').toLowerCase();
		const type = url.searchParams.get('request_type') ?? '';
		const key = url.searchParams.get('sort');
		const dir = url.searchParams.get('dir') === 'desc' ? -1 : 1;
		const value = (c: (typeof cases)[number]) =>
			key === 'time' ? c.duration_ms : key === 'work' ? c.work_items : key === 'case' ? c.case_id : null;
		const items = cases
			.filter((c) => !type || c.request_type === type)
			.filter((c) => !q || [c.case_id, c.customer_id].some((v) => v.toLowerCase().includes(q)));
		if (key)
			items.sort((a, b) => {
				const x = value(a);
				const y = value(b);
				const d = typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y));
				return dir * d;
			});
		await route.fulfill({ json: { items, next: null } });
	});
	await page.goto(job('/cases'));
	const rows = page.locator('tbody tr');
	await expect(rows).toHaveCount(3);

	// Typing narrows through the server: the request carries the text and the answer holds one row.
	const search = page.getByLabel('Search cases');
	await search.fill('case-2');
	await expect(rows).toHaveCount(1);
	await expect(rows.first()).toContainText('CASE-2');
	expect(asked.at(-1)?.searchParams.get('q')).toBe('case-2');

	// Clearing sends no q and gives the three rows back.
	await search.fill('');
	await expect(rows).toHaveCount(3);
	expect(asked.at(-1)?.searchParams.has('q')).toBe(false);

	// A request that matches nothing is the server's empty page, and Clear filters resets the search again.
	await search.fill('nothing-matches');
	await expect(page.getByText('No cases match these filters.')).toBeVisible();
	expect(asked.at(-1)?.searchParams.get('q')).toBe('nothing-matches');
	await page.getByRole('button', { name: 'Clear filters' }).click();
	await expect(search).toHaveValue('');
	await expect(rows).toHaveCount(3);

	// A header click asks the server for the order; the header shows the direction it asked for.
	const time = page.getByRole('columnheader', { name: 'Time' });
	await page.getByRole('button', { name: 'Time' }).click();
	await expect(time).toHaveAttribute('aria-sort', 'ascending');
	await expect(rows.first()).toContainText('CASE-2');
	expect(asked.at(-1)?.searchParams.get('sort')).toBe('time');
	expect(asked.at(-1)?.searchParams.get('dir')).toBe('asc');
	await page.getByRole('button', { name: 'Time' }).click();
	await expect(time).toHaveAttribute('aria-sort', 'descending');
	await expect(rows.first()).toContainText('CASE-1');
	expect(asked.at(-1)?.searchParams.get('dir')).toBe('desc');

	// ?request_type filters on the server and shows as a chip; removing the chip removes the param.
	asked.length = 0;
	await page.goto(job('/cases?request_type=return'));
	const chip = page.getByRole('button', { name: /Request: Returns/ });
	await expect(chip).toBeVisible();
	await expect(rows).toHaveCount(2);
	await expect.poll(() => asked.at(-1)?.searchParams.get('request_type')).toBe('return');
	await chip.click();
	await expect(page).not.toHaveURL(/request_type=/);
	await expect(chip).toBeHidden();
	await expect(rows).toHaveCount(3);
	await expect.poll(() => asked.at(-1)?.searchParams.has('request_type')).toBe(false);
});

test('the tools list searches name and description on the server', async ({ page }) => {
	const base = JSON.parse(readFileSync('src/lib/api/fixtures/tools.json', 'utf8'));
	const asked: URL[] = [];
	await mockApi(page);
	await page.route(apiUrl('/api/tools') + '**', async (route) => {
		const url = new URL(route.request().url());
		asked.push(url);
		const q = (url.searchParams.get('q') ?? '').toLowerCase();
		const items = base.items.filter(
			(t: { name: string; description?: string }) =>
				!q || [t.name, t.description ?? ''].some((v) => v.toLowerCase().includes(q)),
		);
		await route.fulfill({ json: { items, next: null } });
	});
	await page.goto(job('/tools'));
	const rows = page.locator('tbody tr');
	await expect(rows).toHaveCount(2);

	const search = page.getByLabel('Search tools');
	await search.fill('order');
	await expect(rows).toHaveCount(1);
	await expect(rows.first()).toContainText('order_get_order');
	expect(asked.at(-1)?.searchParams.get('q')).toBe('order');
	await search.fill('zzz-no-such-tool');
	await expect(page.getByText('No tools match “zzz-no-such-tool”.')).toBeVisible();
	await page.getByRole('button', { name: 'Clear search' }).click();
	await expect(rows).toHaveCount(2);
	expect(asked.at(-1)?.searchParams.has('q')).toBe(false);
});

test('the ingest jobs list searches and sorts on the server', async ({ page }) => {
	const base = JSON.parse(readFileSync('src/lib/api/fixtures/ingest_jobs.json', 'utf8'));
	const asked: URL[] = [];
	await mockApi(page);
	await page.route(apiUrl('/api/ingest/jobs') + '**', async (route) => {
		const url = new URL(route.request().url());
		asked.push(url);
		const q = (url.searchParams.get('q') ?? '').toLowerCase();
		const source = (j: { source?: { filename?: string; url?: string } }) =>
			(j.source?.filename ?? j.source?.url ?? '').toLowerCase();
		const items = base.items.filter(
			(j: { id: string; status: string; source?: { filename?: string; url?: string } }) =>
				!q || [j.id, j.status, source(j)].some((v) => v.toLowerCase().includes(q)),
		);
		await route.fulfill({ json: { items, next: null } });
	});
	await page.goto('/admin/knowledge/jobs');
	const rows = page.locator('tbody tr');
	await expect(rows).toHaveCount(2);

	// The search is the server's: the request carries q and the two fixture jobs narrow to one.
	const search = page.getByLabel('Search ingest jobs');
	await search.fill('failed');
	await expect(rows).toHaveCount(1);
	expect(asked.at(-1)?.searchParams.get('q')).toBe('failed');
	await expect(page.getByText(/of \d+ loaded/)).toHaveCount(0);
	await page.getByRole('button', { name: 'Clear', exact: true }).click();
	await expect(rows).toHaveCount(2);

	// A header click asks for the order; the store column has no sort control (no server-side name).
	await expect(page.getByRole('columnheader', { name: 'Store' }).getByRole('button')).toHaveCount(0);
	await page.getByRole('button', { name: 'Created' }).click();
	expect(asked.at(-1)?.searchParams.get('sort')).toBe('created');
	expect(asked.at(-1)?.searchParams.get('dir')).toBe('asc');
});
