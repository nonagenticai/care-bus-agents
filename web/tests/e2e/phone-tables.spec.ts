// At 390 px a list table must become labelled rows, not a sideways scroll with columns behind it: the head
// disappears, every cell carries the header text it replaces, and the page itself never overflows.
import { expect, test } from '@playwright/test';
import { E2E_JOB, mockApi } from './mock-api';

const job = (path: string) => `/admin/jobs/${E2E_JOB}${path}`;

const PAGES: [name: string, path: string][] = [
	['cases', job('/cases')],
	['tools', job('/tools')],
	['connectors', job('/backends')],
	['sources', job('/knowledge')],
	['procedures', job('/procedures')],
	['buses', job('/buses')],
	['ingest jobs', '/admin/knowledge/jobs'],
	['audit', '/admin/audit'],
	['providers', '/admin/providers'],
];

for (const [name, path] of PAGES) {
	test(`phone: ${name} stacks its table without a sideways scroll`, async ({ page }) => {
		await page.setViewportSize({ width: 390, height: 844 });
		await mockApi(page);
		await page.goto(path);

		const table = page.locator('table.tbl.stack-phone').first();
		await expect(table).toBeVisible();
		await expect(table.locator('thead')).toBeHidden();

		// A cell without data-label shows no header text on a phone; empty labels are deliberate (actions).
		await expect(table.locator('tbody td:not([data-label])')).toHaveCount(0);

		// The last column of the first row sits inside the 390 px viewport, not behind a horizontal scroll.
		const box = await table.locator('tbody tr').first().locator('td').last().boundingBox();
		expect(box, 'the last cell has a box').not.toBeNull();
		const width = page.viewportSize()!.width;
		expect(box!.x).toBeGreaterThanOrEqual(0);
		expect(box!.x + box!.width).toBeLessThanOrEqual(width + 1);

		const overflow = await page.evaluate(
			() => document.documentElement.scrollWidth - document.documentElement.clientWidth,
		);
		expect(overflow).toBeLessThanOrEqual(1);
	});
}
