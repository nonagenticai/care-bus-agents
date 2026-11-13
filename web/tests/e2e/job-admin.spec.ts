// W3 plan 14: job-scoped admin pages, the job picker, base vs tenant values, release conflicts, the super-admin
// tenant picker and user management.
import { expect, test } from '@playwright/test';
import type { Page } from '@playwright/test';
import { E2E_JOB, E2E_TENANT, apiUrl, mockApi } from './mock-api';

const TWO_JOBS = [
	{ key: E2E_JOB, label: 'Customer care' },
	{ key: 'second_job', label: 'Second job' },
];

test('an old admin URL lands on the same page of the current job, query kept', async ({ page }) => {
	await mockApi(page);
	await page.goto('/admin/buses?tab=dead');
	await expect(page).toHaveURL(new RegExp(`/admin/jobs/${E2E_JOB}/buses\\?tab=dead$`));
	await page.goto('/admin');
	await expect(page).toHaveURL(new RegExp(`/admin/jobs/${E2E_JOB}$`));
	await expect(page.getByText('Cases today')).toBeVisible();
});

test('a job the tenant does not have shows the job list and asks nothing of it', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/jobs/nope/agents');
	await expect(page.getByRole('heading', { name: 'Unknown job' })).toBeVisible();
	await expect(page.getByTestId('job-list').getByRole('link', { name: 'Customer care' })).toBeVisible();
	expect(sent.filter((s) => s.job === 'nope')).toEqual([]);
});

test('the job picker keeps the page and the next requests act in the picked job', async ({ page }) => {
	const sent = await mockApi(page, { jobs: TWO_JOBS });
	await page.goto(`/admin/jobs/${E2E_JOB}/ontology`);
	await expect(page.getByRole('heading', { name: 'Ontology' })).toBeVisible();
	await page.getByRole('combobox', { name: 'Job' }).selectOption('second_job');
	await expect(page).toHaveURL(/\/admin\/jobs\/second_job\/ontology$/);
	await expect
		.poll(() => sent.some((s) => s.job === 'second_job' && s.path === '/api/context-spaces'))
		.toBe(true);
});

const crm = (base_url: string, over: Record<string, unknown> = {}) => ({
	id: 'crm-core',
	name: 'CRM',
	system: 'CRM',
	base_url,
	auth_type: 'api_key',
	secret_ref: null,
	timeout_ms: 3000,
	rate_limit: null,
	health: 'unknown',
	health_checked_at: null,
	mappings: [],
	origin: 'base',
	overridden: false,
	hidden: false,
	...over,
});
const BASE_URL = 'https://crm.release.example/api';
const MY_URL = 'https://crm.acme.example/api';

/** Connector layers that behave like routes::connectors + refine::revert: revert drops the tenant row. */
async function connectorLayers(page: Page) {
	let refined = true;
	const reverts: unknown[] = [];
	const list = (items: unknown[]) => ({ items, next: null });
	await page.route(
		(u) => u.href.startsWith(apiUrl('/api/connectors')) && !u.pathname.endsWith('/test'),
		(route) => {
			const layer = new URL(route.request().url()).searchParams.get('layer');
			const mine = crm(MY_URL, { origin: 'tenant', overridden: true });
			const body =
				layer === 'base'
					? list([crm(BASE_URL)])
					: layer === 'tenant'
						? list(refined ? [mine] : [])
						: list([refined ? mine : crm(BASE_URL)]);
			return route.fulfill({ contentType: 'application/json', body: JSON.stringify(body) });
		},
	);
	await page.route(apiUrl('/api/refinements/revert'), (route) => {
		reverts.push(route.request().postDataJSON());
		refined = false;
		return route.fulfill({ status: 204 });
	});
	await page.route(apiUrl('/api/release-conflicts?status=open'), (route) =>
		route.fulfill({ contentType: 'application/json', body: JSON.stringify({ items: [], next: null }) }),
	);
	return reverts;
}

test('a refined connector shows the base endpoint beside it; revert restores the base', async ({ page }) => {
	await mockApi(page);
	const reverts = await connectorLayers(page);
	await page.goto(`/admin/jobs/${E2E_JOB}/backends`);
	await expect(page.getByText('Refined', { exact: true })).toBeVisible();
	await page.getByRole('button', { name: 'CRM' }).click();
	await expect(page.getByTestId('base-base_url')).toContainText(BASE_URL);
	await page.getByRole('button', { name: 'Revert to base' }).click();
	await expect.poll(() => reverts).toEqual([{ object: 'connector', key: { id: 'crm-core' } }]);
	await expect(page.getByRole('cell', { name: BASE_URL })).toBeVisible();
});

test('the side-by-side page pairs base and tenant values and reverts by natural key', async ({ page }) => {
	await mockApi(page);
	const reverts = await connectorLayers(page);
	await page.goto(`/admin/jobs/${E2E_JOB}/refinements`);
	const row = page.getByTestId('side-by-side-connector').locator('tr[data-key="crm-core"]');
	await expect(row).toContainText(BASE_URL);
	await expect(row).toContainText(MY_URL);
	await row.getByRole('button', { name: 'Revert to base' }).click();
	await expect.poll(() => reverts.length).toBe(1);
	await expect(row.getByRole('button', { name: 'Hide' })).toBeVisible();
});

test('a tenant_user sees base and tenant values with every action disabled', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await connectorLayers(page);
	await page.goto(`/admin/jobs/${E2E_JOB}/refinements`);
	await expect(page.getByText('Read only')).toBeVisible();
	const row = page.getByTestId('side-by-side-connector').locator('tr[data-key="crm-core"]');
	await expect(row.getByRole('button', { name: 'Revert to base' })).toBeDisabled();
});

test('release page lists open conflicts; Keep mine rebases and the list is re-read', async ({ page }) => {
	await mockApi(page);
	const conflict = (id: number, kind: string, key: Record<string, string>, object = 'connector') => ({
		id,
		kind,
		object,
		key,
		tenant_row: 'override',
		base_hash_before: 'sha256:a',
		base_hash_after: 'sha256:b',
		release_id: '2026-10-21.1',
		detected_at: '2026-10-17T10:00:00Z',
		resolved_at: null,
		resolution: null,
		resolved_by: null,
	});
	let open = [
		conflict(2, 'base_changed', { id: 'crm-core' }),
		conflict(1, 'base_removed', { name: 'Carrier' }, 'ontology_entity'),
	];
	const rebases: unknown[] = [];
	await page.route(apiUrl('/api/releases/current'), (route) =>
		route.fulfill({
			contentType: 'application/json',
			body: JSON.stringify({
				release_id: '2026-10-21.1',
				hash: 'sha256:x',
				applied: true,
				previous_release_id: '2026-10-20.1',
				applied_at: '2026-10-17T10:00:00Z',
				applied_by: 'service-account-strata-configurator-release-writer',
				counts: { connectors: 3, context_spaces: 5 },
				conflicts: open,
			}),
		}),
	);
	await page.route(
		(u) => u.href.startsWith(apiUrl('/api/release-conflicts')),
		(route) =>
			route.fulfill({ contentType: 'application/json', body: JSON.stringify({ items: open, next: null }) }),
	);
	await page.route(apiUrl('/api/refinements/rebase'), (route) => {
		rebases.push(route.request().postDataJSON());
		open = open.filter((c) => c.id !== 2);
		return route.fulfill({ status: 204 });
	});
	await page.goto(`/admin/jobs/${E2E_JOB}/release`);
	await expect(page.getByText('2026-10-20.1')).toBeVisible();
	const table = page.getByTestId('release-conflicts');
	await expect(table.locator('tr[data-conflict="1"]').getByRole('button', { name: 'Keep mine' })).toHaveCount(
		0,
	);
	await table.locator('tr[data-conflict="2"]').getByRole('button', { name: 'Keep mine' }).click();
	await expect.poll(() => rebases).toEqual([{ object: 'connector', key: { id: 'crm-core' } }]);
	await expect(table.locator('tr[data-conflict="2"]')).toHaveCount(0);
	await expect(table.locator('tr[data-conflict="1"]')).toContainText('Carrier');
});

const ACME = '7f1c9a2e-0000-4000-8000-00000000fe12';

test('a super admin picks a tenant from the list and every request then names it', async ({ page }) => {
	const sent = await mockApi(page);
	await page.route(apiUrl('/api/me'), (route) => {
		const t = route.request().headers()['x-active-tenant-id'] ?? null;
		const known = [E2E_TENANT, ACME];
		return route.fulfill({
			contentType: 'application/json',
			body: JSON.stringify({
				sub: 'owner',
				actor: 'owner',
				super_admin: true,
				release_writer: false,
				tenant: null,
				active_tenant: t && known.includes(t) ? t : null,
			}),
		});
	});
	await page.route(apiUrl('/api/tenants'), (route) =>
		route.fulfill({
			contentType: 'application/json',
			body: JSON.stringify({
				items: [
					{ id: ACME, name: 'Acme Retail', status: 'active' },
					{ id: E2E_TENANT, name: 'Bootstrap tenant', status: 'active' },
				],
				next: null,
			}),
		}),
	);
	await page.goto('/admin');
	const picker = page.getByTestId('tenant-picker');
	await picker.getByRole('searchbox').fill('acme');
	await expect(picker.getByRole('button', { name: /Bootstrap tenant/ })).toHaveCount(0);
	await picker.getByRole('button', { name: /Acme Retail/ }).click();
	await expect(page.getByRole('button', { name: 'Tenant: Acme Retail' })).toBeVisible();
	await expect.poll(() => sent.some((s) => s.path === '/api/overview' && s.tenant === ACME)).toBe(true);
});

/** routes::tenancy user list and upsert for the bootstrap tenant: one tenant_admin (e2e) and one tenant_user. */
async function tenantUsers(page: Page) {
	const puts: { sub: string; body: unknown }[] = [];
	const users = [
		{
			sub: 'e2e',
			tenant_role: 'tenant_admin',
			display_name: 'E. Twee',
			updated_at: '2026-10-17T09:00:00Z',
			updated_by: 'owner',
		},
		{
			sub: 'u-user',
			tenant_role: 'tenant_user',
			display_name: 'U. Jansen',
			updated_at: '2026-10-17T09:00:00Z',
			updated_by: 'owner',
		},
	];
	await page.route(apiUrl(`/api/tenants/${E2E_TENANT}/users`), (route) =>
		route.fulfill({ contentType: 'application/json', body: JSON.stringify({ items: users, next: null }) }),
	);
	await page.route(
		(u) => u.href.startsWith(apiUrl(`/api/tenants/${E2E_TENANT}/users/`)),
		(route) => {
			const sub = decodeURIComponent(new URL(route.request().url()).pathname.split('/').pop()!);
			const body = route.request().postDataJSON();
			puts.push({ sub, body });
			users.push({ sub, updated_at: '2026-10-17T10:00:00Z', updated_by: 'e2e', ...body });
			return route.fulfill({ status: 201, contentType: 'application/json', body: JSON.stringify(body) });
		},
	);
	return puts;
}

test('a tenant admin adds a user; the last tenant_admin cannot be removed', async ({ page }) => {
	await mockApi(page);
	const puts = await tenantUsers(page);
	await page.goto('/admin/users');
	const table = page.getByTestId('tenant-users');
	await expect(table.locator('tr[data-sub="e2e"]').getByRole('button', { name: 'Remove' })).toBeDisabled();
	await expect(table.locator('tr[data-sub="u-user"]').getByRole('button', { name: 'Remove' })).toBeEnabled();
	await page.getByLabel('Keycloak subject (sub)').fill('kc-42');
	await page.getByLabel('Display name').fill('K. Veertig');
	await page.getByRole('button', { name: 'Add user' }).click();
	await expect
		.poll(() => puts)
		.toEqual([{ sub: 'kc-42', body: { tenant_role: 'tenant_user', display_name: 'K. Veertig' } }]);
	await expect(table.locator('tr[data-sub="kc-42"]')).toContainText('K. Veertig');
});

test('a tenant_user reads the users with every control disabled', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await tenantUsers(page);
	await page.goto('/admin/users');
	await expect(page.getByText('Read only')).toBeVisible();
	const row = page.getByTestId('tenant-users').locator('tr[data-sub="u-user"]');
	await expect(row.getByRole('button', { name: 'Remove' })).toBeDisabled();
	await expect(row.getByRole('combobox')).toBeDisabled();
	await expect(page.getByRole('button', { name: 'Add user' })).toBeDisabled();
});
