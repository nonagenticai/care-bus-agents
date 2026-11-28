/**
 * Live, login-gated check of the admin case page — the surface the owner reported
 * unreadable. Host-run on purpose: the super-admin password (environment, from the
 * gitops SOPS secret) never enters a pod that executes LLM-generated test code.
 *
 * Skips (not fails) without CAREBUS_FRTEST_PASSWORD, so CI never needs the credential.
 */
import { test, expect, type Page } from '@playwright/test';
import { mkdirSync, writeFileSync } from 'node:fs';

const TENANT = 'abc57b6f-2ef5-4dbd-b96a-a4e4c88cc441';
const JOB = 'website_chat';
const PASSWORD = process.env.CAREBUS_FRTEST_PASSWORD ?? '';
const OUT = '/tmp/carebus-proof-live';
const MARKER = /\[\d{1,3}(?:\s*,\s*\d{1,3})*\]/;

test.skip(!PASSWORD, 'set CAREBUS_FRTEST_PASSWORD from the gitops fr-test SOPS secret');

async function login(page: Page) {
	await page.goto('/');
	const auth = page.getByLabel(/Username or email/i);
	await auth.waitFor({ state: 'visible', timeout: 90_000 }).catch(() => {});
	if (await auth.isVisible().catch(() => false)) {
		await auth.fill('fr-test');
		await page.getByLabel(/^Password$/i).fill(PASSWORD);
		await page.getByRole('button', { name: /^Sign in$/i }).click();
	}
	// Back on the app: the admin nav renders once keycloak-js has processed the redirect.
	await expect(page.locator('a[href*="/admin/"]').first()).toBeVisible({ timeout: 90_000 });
}

async function openNewestFaqCase(page: Page) {
	await page.goto(`/admin/jobs/${JOB}/cases?request_type=faq`);
	const rows = page.locator('tbody tr');
	await expect(rows.first()).toBeVisible({ timeout: 90_000 });
	const listed = await rows.count();
	await rows.first().click();
	await page.waitForURL(new RegExp(`/admin/jobs/${JOB}/cases/CASE-`), { timeout: 60_000 });
	return { url: page.url(), listed };
}

async function checkAt(page: Page, width: number, height: number, label: string) {
	await page.setViewportSize({ width, height });
	await page.reload({ waitUntil: 'domcontentloaded' });

	const answer = page.locator('p.answer');
	await expect(answer).toBeVisible({ timeout: 60_000 });
	const text = (await answer.innerText()).trim();
	expect(text.length, 'the stored answer is shown as prose').toBeGreaterThan(40);
	expect(text, 'no citation markers in the visitor-facing answer').not.toMatch(MARKER);
	expect(text, 'no literal \\n in the visitor-facing answer').not.toContain('\\n');

	const trace = page.locator('.trace-card');
	await expect(trace).toBeVisible();
	const answerBox = await answer.boundingBox();
	const traceBox = await trace.boundingBox();
	expect(answerBox!.y, 'the answer comes before the trace').toBeLessThan(traceBox!.y);

	const rows = page.locator('.seq-row');
	const rowCount = await rows.count();
	expect(rowCount, 'the trace has events').toBeGreaterThan(0);

	const wrap = page.locator('.seq-wrap');
	await expect(wrap).toBeVisible();
	const natural = await wrap.evaluate((el) => ({ client: el.clientHeight, scroll: el.scrollHeight }));
	await wrap.evaluate((el) => {
		el.scrollTop = el.scrollHeight;
	});
	const rest = await wrap.evaluate((el) => ({
		top: el.scrollTop,
		max: el.scrollHeight - el.clientHeight,
	}));
	expect(rest.top, 'the trace scroller reachable end is its real end').toBeGreaterThanOrEqual(
		Math.max(0, rest.max - 2),
	);
	const last = await rows.last().boundingBox();
	const box = await wrap.boundingBox();
	expect(last!.y + last!.height, 'the last event is inside the scroller').toBeLessThanOrEqual(
		box!.y + box!.height + 2,
	);

	const hash = page.locator('details.band.raw');
	expect(await hash.evaluate((el) => el.hasAttribute('open')), 'the raw case hash starts collapsed').toBe(
		false,
	);

	const overflow = await page.evaluate(
		() => document.documentElement.scrollWidth - document.documentElement.clientWidth,
	);
	expect(overflow, 'no horizontal page scroll').toBeLessThanOrEqual(1);

	await page.screenshot({ path: `${OUT}/case-${label}.png`, fullPage: true });
	return { label, width, answer_chars: text.length, rows: rowCount, natural, rest, overflow };
}

test('the admin case page is readable at 1440 and 390', async ({ page }) => {
	mkdirSync(OUT, { recursive: true });
	await page.addInitScript((tenant) => {
		sessionStorage.setItem('carebus-active-tenant', tenant);
	}, TENANT);

	await login(page);
	const opened = await openNewestFaqCase(page);
	const desktop = await checkAt(page, 1440, 900, 'desktop');
	const phone = await checkAt(page, 390, 844, 'phone');

	writeFileSync(
		`${OUT}/admin-case.json`,
		JSON.stringify(
			{ case_url: opened.url, listed: opened.listed, desktop, phone, checked_at: new Date().toISOString() },
			null,
			2,
		),
	);
});
