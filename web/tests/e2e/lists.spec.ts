// List search and sort: a filter narrows the loaded rows and says how many it is filtering, a header click
// orders them and flips aria-sort, and clearing gives the list back.
import { expect, test } from '@playwright/test';
import { E2E_JOB, mockApi } from './mock-api';

// Sorting lives in the table head, which the phone layout deliberately replaces with per-row labels; run this
// file at a wide viewport in both projects (the stacked phone layout is covered by phone-tables.spec.ts).
test.use({ viewport: { width: 1280, height: 900 } });

const job = (path: string) => `/admin/jobs/${E2E_JOB}${path}`;

const caseRow = (id: string, customer: string, work: number, ms: number) => ({
	case_id: id,
	conversation_id: null,
	customer_id: customer,
	request_type: 'return',
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

test('the cases list filters the loaded rows and sorts by a header', async ({ page }) => {
	await mockApi(page, {
		override: {
			'/api/cases': {
				items: [
					caseRow('CASE-1', 'C-1', 9, 4000),
					caseRow('CASE-2', 'C-2', 1, 1000),
					caseRow('CASE-3', 'C-3', 5, 2000),
				],
				next: null,
			},
		},
	});
	await page.goto(job('/cases'));
	const rows = page.locator('tbody tr');
	await expect(rows).toHaveCount(3);

	const search = page.getByLabel('Search cases');
	await search.fill('case-2');
	await expect(rows).toHaveCount(1);
	await expect(rows.first()).toContainText('CASE-2');
	await expect(page.locator('.filter-count')).toHaveText('1 of 3 loaded');

	await search.fill('nothing-matches');
	await expect(page.getByText('No loaded cases match “nothing-matches”.')).toBeVisible();
	await page.getByRole('button', { name: 'Clear search' }).click();
	await expect(search).toHaveValue('');
	await expect(rows).toHaveCount(3);

	const header = page.getByRole('button', { name: 'Work items' });
	await header.click();
	await expect(page.getByRole('columnheader', { name: 'Work items' })).toHaveAttribute(
		'aria-sort',
		'ascending',
	);
	await expect(rows.first()).toContainText('CASE-2');
	await header.click();
	await expect(page.getByRole('columnheader', { name: 'Work items' })).toHaveAttribute(
		'aria-sort',
		'descending',
	);
	await expect(rows.first()).toContainText('CASE-1');
	// The cells carry their own column label once the phone layout stacks them.
	await expect(rows.first().locator('td').first()).toHaveAttribute('data-label', 'Case');
});

test('the tools list searches name and description', async ({ page }) => {
	await mockApi(page);
	await page.goto(job('/tools'));
	const rows = page.locator('tbody tr');
	await expect(rows.first()).toBeVisible();

	const search = page.getByLabel('Search tools');
	await search.fill('order');
	await expect(rows.first()).toContainText('order_get_order');
	await search.fill('zzz-no-such-tool');
	await expect(page.getByText('No tools match “zzz-no-such-tool”.')).toBeVisible();
	await page.getByRole('button', { name: 'Clear search' }).click();
	await expect(rows.first()).toBeVisible();
});
