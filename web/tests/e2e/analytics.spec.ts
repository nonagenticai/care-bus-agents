// The analytics chart says what its bars mean (an axis) and a bar reaches the cases behind it.
import { expect, test } from '@playwright/test';
import { mockApi } from './mock-api';

test('the request-type bars carry a value axis and open the filtered case list', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/jobs/customer_care_agent/analytics');
	await expect(page.getByRole('heading', { name: 'Analytics' })).toBeVisible();

	// The scale the bars are drawn against: the fixture's largest bar is 1, so the axis ends at 1.
	const axis = page.locator('.barsh-axis');
	await expect(axis).toBeVisible();
	await expect(axis).toContainText('0');

	// The fixture has one request type (`return`); clicking its bar opens the case list filtered to it.
	await page.locator('button.bar-row').first().click();
	await expect(page).toHaveURL(/\/cases\?request_type=return$/);
	await expect
		.poll(
			() => sent.filter((r) => r.path === '/api/cases' && r.search.includes('request_type=return')).length,
		)
		.toBeGreaterThan(0);
});
