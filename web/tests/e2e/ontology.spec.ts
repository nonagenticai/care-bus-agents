import { expect, test, type Page } from '@playwright/test';
import { apiUrl, mockApi } from './mock-api';

const relationCount = (page: Page) => page.locator('.counts b').nth(1);

async function openAddRelation(page: Page) {
	await page.goto('/admin/ontology');
	await expect(page.locator('[data-entity="Customer"]')).toBeVisible();
	await page.getByRole('button', { name: 'Add relation' }).click();
	return page.getByRole('dialog', { name: 'Add relation' });
}

test('an admin adds a relation and the graph and relations list pick it up', async ({ page }) => {
	const sent = await mockApi(page);
	const drawer = await openAddRelation(page);
	const before = Number(await relationCount(page).textContent());
	expect(before).toBeGreaterThan(0);

	await drawer.getByLabel('Subject').selectOption('Customer');
	await drawer.getByLabel('Predicate').fill('files');
	await drawer.getByLabel('Object').selectOption('WarrantyClaim');
	await drawer.getByLabel('Cardinality').selectOption('1:n');
	await expect(drawer.getByText('Reads as:')).toContainText('Customer');
	await drawer.getByRole('button', { name: 'Create relation' }).click();

	await expect(drawer).toBeHidden();
	await expect(relationCount(page)).toHaveText(String(before + 1));
	const post = sent.find((s) => s.method === 'POST' && s.path === '/api/ontology/relations');
	expect(post!.job).toBe('customer_care_agent');
	expect(post!.tenant).toBeNull(); // a tenant member never names a tenant
	expect(JSON.parse(post!.body!)).toEqual({
		subject: 'Customer',
		predicate: 'files',
		object: 'WarrantyClaim',
		cardinality: '1:n',
	});

	await page.getByRole('button', { name: 'Relations list' }).click();
	const row = page.getByRole('row').filter({ hasText: 'files' });
	await expect(row).toContainText('Customer');
	await expect(row).toContainText('Warranty claim');
});

test('the form explains invalid input before calling the API', async ({ page }) => {
	const sent = await mockApi(page);
	const drawer = await openAddRelation(page);
	await drawer.getByLabel('Subject').selectOption('Customer');
	await drawer.getByLabel('Predicate').fill('Paid By');
	await drawer.getByLabel('Object').selectOption('Order');
	await drawer.getByRole('button', { name: 'Create relation' }).click();
	await expect(drawer.getByRole('alert')).toContainText('lowerCamel');

	// an existing (subject, predicate, object) is caught too
	await drawer.getByLabel('Predicate').fill('places');
	await drawer.getByRole('button', { name: 'Create relation' }).click();
	await expect(drawer.getByRole('alert')).toContainText('Customer places Order already exists.');
	expect(sent.some((s) => s.method === 'POST' && s.path === '/api/ontology/relations')).toBe(false);
});

test('an API rejection is shown inline and the drawer stays open', async ({ page }) => {
	await mockApi(page);
	// Another admin created it between our GET and POST: the server answers 409.
	await page.route(apiUrl('/api/ontology/relations'), (route) =>
		route.request().method() === 'POST'
			? route.fulfill({
					status: 409,
					contentType: 'application/json',
					body: JSON.stringify({
						error: { code: 'relation_exists', message: 'Customer owns Order already exists.' },
					}),
				})
			: route.fallback(),
	);
	const drawer = await openAddRelation(page);
	await drawer.getByLabel('Subject').selectOption('Customer');
	await drawer.getByLabel('Predicate').fill('owns');
	await drawer.getByLabel('Object').selectOption('Order');
	await drawer.getByRole('button', { name: 'Create relation' }).click();
	await expect(drawer.getByRole('alert')).toContainText(
		'Customer owns Order already exists. (relation_exists)',
	);
	await expect(drawer.getByRole('button', { name: 'Create relation' })).toBeEnabled();
});

test('a relation is deleted from the entity drawer after an in-page confirm', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/ontology');
	await expect(page.locator('[data-entity="Customer"]')).toBeVisible();
	const before = Number(await relationCount(page).textContent());
	expect(before).toBeGreaterThan(0);
	await page.locator('[data-entity="Customer"]').click();
	const drawer = page.getByRole('dialog', { name: 'Customer' });
	await drawer.getByRole('button', { name: 'Delete relation Customer places Order' }).click();
	const confirm = drawer.getByRole('group', { name: 'Confirm delete' });
	await expect(confirm).toContainText('Delete Customer places Order?');
	await confirm.getByRole('button', { name: 'Keep' }).click();
	await expect(confirm).toBeHidden();
	expect(sent.some((s) => s.method === 'DELETE')).toBe(false);

	await drawer.getByRole('button', { name: 'Delete relation Customer places Order' }).click();
	await drawer.getByRole('group', { name: 'Confirm delete' }).getByRole('button', { name: 'Delete' }).click();
	await expect(drawer.getByRole('heading', { name: 'Relations out (1)' })).toBeVisible();
	await expect(relationCount(page)).toHaveText(String(before - 1));
	expect(
		sent.some((s) => s.method === 'DELETE' && s.path === '/api/ontology/relations/Customer/places/Order'),
	).toBe(true);
});

test('a viewer gets no relation write controls', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await page.goto('/admin/ontology');
	await expect(page.locator('[data-entity="Customer"]')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Add relation' })).toHaveCount(0);
	await page.locator('[data-entity="Customer"]').click();
	await expect(page.getByRole('button', { name: /^Delete relation/ })).toHaveCount(0);
});
