import { expect, test } from '@playwright/test';
import { E2E_TENANT, mockApi, overlay } from './mock-api';

test('dashboard shows an unavailable KPI as "—" with its reason, not 0', async ({ page }) => {
	const zero = { cases: 0, completed: 0, not_eligible: 0, failed: 0, open: 0, median_ms: null };
	await mockApi(page, {
		override: { '/api/overview': { today: zero, week_ago: zero, health: [], attention: [] } },
	});
	await page.goto('/admin');
	await expect(page.getByText('Cases today')).toBeVisible();
	await expect(page.getByText('Unavailable: no resolved cases yet')).toBeVisible();
	const median = page.locator('.kpi').filter({ hasText: 'Median time to answer' });
	await expect(median.locator('.kv-val')).toHaveText('—');
	await expect(median).not.toContainText('0');
});

test('dashboard tiles come from today vs week_ago and health shows status words', async ({ page }) => {
	await mockApi(page, { override: { '/api/overview': overlay('overview') } });
	await page.goto('/admin');
	await expect(page.getByText('412', { exact: true })).toBeVisible();
	await expect(page.getByText('Not configured', { exact: true })).toBeVisible(); // mcp: unconfigured
	await expect(page.getByText('Down', { exact: true })).toBeVisible(); // strata_rag: down
});

test('a dependency outage renders Unavailable with the reason', async ({ page }) => {
	await mockApi(page, { down: ['/api/overview'] });
	await page.goto('/admin');
	await expect(page.getByText('case_history is unreachable')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Try again' })).toBeVisible();
});

test('buses page flags the lagging tool-executor group, lists dead letters and opens the stream drawer', async ({
	page,
}) => {
	await mockApi(page, {
		override: { '/api/streams': overlay('streams'), '/api/dead-letters': overlay('dead_letters') },
	});
	await page.goto('/admin/buses');
	await expect(page.getByRole('cell', { name: 'bus:tool:delivery' })).toBeVisible();
	await expect(page.getByText('Behind', { exact: true })).toBeVisible();
	await page.getByRole('tab', { name: /Dead letters/ }).click();
	await expect(page.getByText('timeout after 3000 ms')).toBeVisible();
	await page.getByRole('tab', { name: 'Streams' }).click();
	await page.getByRole('cell', { name: 'bus:tool:delivery' }).getByRole('button').click();
	await expect(page.getByRole('button', { name: 'Claim stuck' })).toBeEnabled();
});

test('a tenant_user sees write controls disabled with the role hint', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user', override: { '/api/streams': overlay('streams') } });
	await page.goto('/admin/buses');
	await page.getByRole('cell', { name: 'bus:tool:delivery' }).getByRole('button').click();
	const claim = page.getByRole('button', { name: 'Claim stuck' });
	await expect(claim).toBeDisabled();
	await expect(claim).toHaveAttribute('title', 'Needs the tenant_admin role');
});

test('an account with no care-bus tenant sees the not-provisioned state, no pages', async ({ page }) => {
	const sent = await mockApi(page, {
		me: {
			sub: 'e2e',
			actor: 'e2e',
			super_admin: false,
			release_writer: false,
			tenant: null,
			active_tenant: null,
		},
	});
	await page.goto('/admin');
	await expect(page.getByText('Your account has no care-bus tenant yet')).toBeVisible();
	await expect(page.getByRole('navigation')).toHaveCount(0);
	expect(sent.map((r) => r.path)).toEqual(['/api/me']);
});

test('a super_admin without a tenant picks one; every call then names it', async ({ page }) => {
	const sent = await mockApi(page, {
		me: {
			sub: 'e2e',
			actor: 'e2e',
			super_admin: true,
			release_writer: false,
			tenant: null,
			active_tenant: null,
		},
	});
	// routes::tenancy::me echoes a known X-Active-Tenant-Id as active_tenant.
	await page.route('**/api/me', (route) => {
		const t = route.request().headers()['x-active-tenant-id'] ?? null;
		return route.fulfill({
			contentType: 'application/json',
			body: JSON.stringify({
				sub: 'e2e',
				actor: 'e2e',
				super_admin: true,
				release_writer: false,
				tenant: null,
				active_tenant: t === E2E_TENANT ? t : null,
			}),
		});
	});
	await page.goto('/admin');
	await expect(page.getByText('Choose a tenant to act in')).toBeVisible();
	// the first screen a super_admin sees must let them leave
	await expect(page.getByRole('button', { name: 'Sign out' })).toBeVisible();
	await page.getByLabel('Tenant id').fill(E2E_TENANT);
	await page.getByRole('button', { name: 'Act in tenant' }).click();
	await expect(page.getByText('Cases today')).toBeVisible();
	const overview = sent.find((r) => r.path === '/api/overview');
	expect(overview?.tenant).toBe(E2E_TENANT);
	expect(overview?.job).toBe('customer_care_agent');
});
