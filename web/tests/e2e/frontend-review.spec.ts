import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { apiUrl, mockApi, overlay } from './mock-api';
const fixture = (name: string) => JSON.parse(readFileSync(`src/lib/api/fixtures/${name}.json`, 'utf8'));

test('open filter is sent to server and empty pages retain continuation and URL state', async ({ page }) => {
	await mockApi(page);
	await page.route(apiUrl('/api/cases?*'), async (route) => {
		const url = new URL(route.request().url());
		expect(url.searchParams.get('outcome')).toBe('open');
		await route.fulfill({
			json: url.searchParams.has('before')
				? { items: [{ ...overlay('case').case, case_id: 'OLDER-OPEN', outcome: null }], next: null }
				: { items: [], next: 'cursor-2' },
		});
	});
	await page.goto('/admin/cases?outcome=open');
	await expect(page.getByText('No cases match these filters.')).toBeVisible();
	await page.getByRole('button', { name: 'Older results', exact: true }).click();
	await expect(page.getByRole('row', { name: /OLDER-OPEN/ })).toBeVisible();
	await page.reload();
	await expect(page.getByRole('row', { name: /OLDER-OPEN/ })).toBeVisible();
	await page.getByRole('button', { name: 'Newer results', exact: true }).click();
	await expect(page.getByRole('button', { name: 'Clear filters' })).toBeVisible();
});

for (const [path, endpoint] of [
	['/admin/audit', 'audit'],
	['/admin/knowledge', 'sources'],
]) {
	test(`${endpoint} can traverse older pages and return after reload`, async ({ page }) => {
		await mockApi(page);
		await page.route(apiUrl(`/api/${endpoint}?*`), async (route) => {
			const data = fixture(endpoint!);
			const older = new URL(route.request().url()).searchParams.has('before');
			await route.fulfill({ json: { items: older ? [] : data.items, next: older ? null : 'older-page' } });
		});
		await page.goto(path!);
		await page.getByRole('button', { name: 'Older results', exact: true }).click();
		await expect(page).toHaveURL(/before=older-page/);
		await page.reload();
		// An empty older page offers the way back: audit shows only "Newest results", sources the full pager.
		const back = page.getByRole('button', {
			name: endpoint === 'audit' ? 'Newest results' : 'Newer results',
			exact: true,
		});
		await expect(back).toBeVisible();
		await expect(page.getByRole('button', { name: 'Older results', exact: true })).toHaveCount(
			endpoint === 'audit' ? 0 : 1,
		);
		await back.click();
		await expect(page).not.toHaveURL(/before=/);
		await expect(page.getByRole('button', { name: 'Older results', exact: true })).toBeEnabled();
	});
}

test('approve and publish are separate: an approval is recorded, publish unlocks at two reviewers', async ({
	page,
}) => {
	// The mock is stateful like routes::procedures: return@1.4.0-beta is CANARY with one reviewer (a.bakker), and the
	// fake auth acts as 'e2e', a distinct second reviewer.
	const sent = await mockApi(page);
	await page.goto('/admin/procedures');
	await page.getByRole('row', { name: /1\.4\.0-beta/ }).click();
	const dialog = page.getByRole('dialog');
	// Approvals live on the Versions tab.
	await dialog.getByRole('tab', { name: /^Versions/ }).click();
	const approve = dialog.getByRole('button', { name: 'Approve', exact: true });
	const publish = dialog.getByRole('button', { name: 'Publish', exact: true });
	await expect(publish).toBeDisabled();
	await expect(dialog.getByText('Needs 1 more approval', { exact: true })).toBeVisible();
	await approve.click();
	await expect(dialog.getByRole('status').filter({ hasText: 'Your approval was recorded.' })).toBeVisible();
	await expect(dialog.getByText('You approved this version.')).toBeVisible();
	await expect(dialog).toContainText('Approved by a.bakker and e2e.');
	await expect(approve).toBeDisabled();
	await expect(page.getByText(/Not saved|Not approved/)).toHaveCount(0);
	// Two distinct reviewers: publishing is its own action now.
	await expect(dialog.getByText(/more approval/)).toHaveCount(0);
	await expect(publish).toBeEnabled();
	await publish.click();
	await expect(
		page.getByRole('status').filter({ hasText: 'return@1.4.0-beta is now PUBLISHED.' }),
	).toBeVisible();
	await expect(dialog).toHaveCount(0);
	const writes = sent.filter((r) => r.method === 'POST' && r.path.startsWith('/api/procedures/'));
	expect(writes.map((r) => [r.path, r.body])).toEqual([
		['/api/procedures/return/versions/1.4.0-beta/approve', null],
		['/api/procedures/return/versions/1.4.0-beta/status', JSON.stringify({ to: 'PUBLISHED' })],
	]);
});

test('rejected send stays as a not-sent bubble across reload and labels retry explicitly', async ({
	page,
}) => {
	await mockApi(page, {
		override: { '/api/conversations/conv-1': { conversation_id: 'conv-1', messages: [], case_ids: [] } },
	});
	let sends = 0;
	await page.route(apiUrl('/api/conversations/conv-1/messages'), (route) => {
		sends++;
		return route.fulfill({ status: 422, json: { error: { code: 'invalid', message: 'Message rejected' } } });
	});
	await page.goto('/console');
	await expect(page.getByRole('button', { name: 'Send', exact: true })).toBeEnabled();
	await page.getByLabel('Customer message').fill('Keep my draft');
	await page.getByRole('button', { name: 'Send', exact: true }).click();
	// The rejected message is not lost: it stays as a "not sent" bubble; the composer is not refilled.
	const bubble = page.locator('.bub.pending', { hasText: 'Keep my draft' });
	await expect(bubble).toContainText('not sent');
	await expect(page.getByLabel('Customer message')).toHaveValue('');
	await expect(page.getByRole('button', { name: 'Retry message' })).toBeEnabled();
	await expect(page.getByRole('button', { name: 'Discard', exact: true })).toBeVisible();
	await page.reload();
	await expect(bubble).toContainText('not sent');
	await expect(page.getByRole('button', { name: 'Retry message' })).toBeEnabled();
	expect(sends).toBe(1);
	await page.getByRole('button', { name: 'Discard', exact: true }).click();
	await expect(bubble).toHaveCount(0);
	expect(sends).toBe(1);
});

test('lost send response reconciles without replaying a submitted action', async ({ page }) => {
	await mockApi(page);
	let sends = 0,
		accepted = false;
	await page.route(apiUrl('/api/conversations/conv-1'), (route) =>
		route.fulfill({
			json: {
				conversation_id: 'conv-1',
				case_ids: [],
				messages: accepted
					? [
							{ conversation_id: 'conv-1', role: 'customer', text: 'Track it', at_ms: 1 },
							{ conversation_id: 'conv-1', role: 'agent', text: 'Checking the order', at_ms: 2 },
						]
					: [],
			},
		}),
	);
	await page.route(apiUrl('/api/conversations/conv-1/messages'), (route) => {
		sends++;
		return route.abort();
	});
	await page.goto('/console');
	await expect(page.getByRole('button', { name: 'Send', exact: true })).toBeEnabled();
	await page.getByLabel('Customer message').fill('Track it');
	await page.getByRole('button', { name: 'Send', exact: true }).click();
	await expect(page.getByText(/Send outcome unknown/)).toBeVisible();
	await expect(page.locator('.bub.pending', { hasText: 'Track it' })).toContainText('outcome unknown');
	await expect(page.getByLabel('Customer message')).toHaveValue('');
	await expect(page.getByRole('button', { name: 'Resend anyway' })).toBeVisible();
	await expect(page.getByRole('button', { name: 'Send', exact: true })).toBeDisabled();
	await page.reload();
	await expect(page.getByText(/Send outcome unknown/)).toBeVisible();
	accepted = true;
	await page.getByRole('button', { name: 'Reload conversation' }).click();
	await expect(page.getByText('Checking the order')).toBeVisible();
	await expect(page.getByLabel('Customer message')).toHaveValue('');
	await expect(page.getByRole('button', { name: 'Send', exact: true })).toBeEnabled();
	expect(sends).toBe(1);
});

test('keyboard drawer traps focus, protects dirty fields and restores trigger focus', async ({ page }) => {
	await mockApi(page);
	await page.goto('/admin/providers');
	const trigger = page.getByRole('button', { name: 'Embeddings', exact: true });
	await trigger.focus();
	await page.keyboard.press('Enter');
	const dialog = page.getByRole('dialog');
	await expect(dialog).toBeVisible();
	for (let i = 0; i < 18; i++) {
		await page.keyboard.press('Tab');
		expect(await page.evaluate(() => !!document.activeElement?.closest('[role="dialog"]'))).toBe(true);
	}
	await page.getByRole('textbox', { name: 'Model', exact: true }).fill('draft-model');
	// Dirty: Escape asks inside the drawer (no window.confirm); keeping the edit leaves the draft intact.
	await page.keyboard.press('Escape');
	const confirm = page.getByRole('alertdialog', { name: 'Discard changes?' });
	await expect(confirm).toBeVisible();
	await page.getByRole('button', { name: 'Keep editing' }).click();
	await expect(confirm).toHaveCount(0);
	await expect(dialog).toBeVisible();
	await expect(page.getByRole('textbox', { name: 'Model', exact: true })).toHaveValue('draft-model');
	await page.keyboard.press('Escape');
	await page.getByRole('button', { name: 'Discard', exact: true }).click();
	await expect(dialog).toHaveCount(0);
	await expect(trigger).toBeFocused();
});

test('tabs implement roving focus and panel labels', async ({ page }) => {
	await mockApi(page);
	await page.goto('/admin/buses');
	await page.getByRole('tab', { name: 'Streams', exact: true }).focus();
	await page.keyboard.press('ArrowRight');
	const tab = page.getByRole('tab', { name: /Dead letters/ });
	await expect(tab).toBeFocused();
	await expect(tab).toHaveAttribute('aria-selected', 'true');
	await expect(page.getByRole('tabpanel')).toHaveAttribute(
		'aria-labelledby',
		(await tab.getAttribute('id'))!,
	);
	await page.keyboard.press('Home');
	await expect(page.getByRole('tab', { name: 'Streams', exact: true })).toBeFocused();
});

test('long trace stays bounded to the last 300 events per lane and says so', async ({ page }, testInfo) => {
	const detail = overlay('case');
	detail.trace = Array.from({ length: 350 }, (_, i) => ({
		...detail.trace[0],
		entry_id: `${1792065600000 + i}-0`,
		envelope: { ...detail.trace[0].envelope, published_at_ms: 1792065600000 + i },
	}));
	await mockApi(page, {
		override: { '/api/cases/CASE-1': detail, '/api/conversations/conv-1': overlay('conversation') },
	});
	await page.goto('/console?conversation=conv-1');
	await expect(page.getByLabel('customer-bus').locator('.msg')).toHaveCount(300);
	// Phone-width console shows one panel at a time; the trace bar lives on the Activity view.
	const activityTab = page.getByRole('tab', { name: 'Activity', exact: true });
	if (await activityTab.isVisible()) await activityTab.click();
	await expect(page.getByText('Showing the last 300 events per bus; 50 older not shown')).toBeVisible();
	expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
	await page.screenshot({ path: testInfo.outputPath('long-trace.png'), fullPage: true });
});
