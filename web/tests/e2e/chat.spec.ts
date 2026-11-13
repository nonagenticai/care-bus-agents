import { expect, test, type Page } from '@playwright/test';
import { apiUrl, mockApi, overlay, type Sent } from './mock-api';

const REPLY = 'Your Philips Airfryer XXL (ORD-58790) is on its way with PostNL.';
const LOOK_KEY = 'carebus.chat.look';
const LOOKS = [
	['teams', 'Teams-style'],
	['slack', 'Slack-style'],
	['whatsapp', 'WhatsApp-style'],
] as const;
const posts = (sent: Sent[], path: string) => sent.filter((r) => r.method === 'POST' && r.path === path);
type Turn = { conversation_id: string; role: 'customer' | 'agent'; text: string; at_ms: number };

/**
 * A server log that holds only what was actually posted (like carebus-api), so each turn's echo and reply are
 * new. `hold` delays a POST; `reject` answers it 400 instead.
 */
async function conversation(page: Page, opts: { hold?: number } = {}) {
	const sent = await mockApi(page);
	const base = overlay('conversation');
	const state = { reject: false, log: [] as Turn[] };
	await page.route(apiUrl('/api/conversations/conv-1'), (route) =>
		route.fulfill({ json: { ...base, conversation_id: 'conv-1', messages: state.log, case_ids: [] } }),
	);
	await page.route(apiUrl('/api/conversations/conv-1/messages'), async (route) => {
		if (state.reject)
			return route.fulfill({
				status: 400,
				json: { error: { code: 'invalid_body', message: 'text must not be empty' } },
			});
		const text = (JSON.parse(route.request().postData() ?? '{}') as { text: string }).text;
		if (opts.hold) await new Promise((r) => setTimeout(r, opts.hold));
		const at = Date.now();
		state.log.push({ conversation_id: 'conv-1', role: 'customer', text, at_ms: at });
		state.log.push({ conversation_id: 'conv-1', role: 'agent', text: REPLY, at_ms: at + 1 });
		await route.fulfill({ json: { turn: 'ask', text: REPLY, case_id: null } });
	});
	return { sent, state };
}

async function noHorizontalScroll(page: Page) {
	const overflow = await page.evaluate(
		() => document.documentElement.scrollWidth > document.documentElement.clientWidth,
	);
	expect(overflow).toBe(false);
}

for (const [look, label] of LOOKS) {
	test(`the ${label} look renders the customer message and the agent reply`, async ({ page }) => {
		await page.addInitScript(([k, v]) => localStorage.setItem(k, v), [LOOK_KEY, look] as const);
		const { sent, state } = await conversation(page);
		await page.goto('/chat');
		const shell = page.locator(`[data-look="${look}"]`);
		await expect(shell).toBeVisible();
		await expect(page.getByRole('button', { name: label })).toHaveAttribute('aria-pressed', 'true');
		const input = shell.getByLabel('Message');
		await input.fill('Where is my airfryer?');
		await input.press('Enter');
		await expect(input).toHaveValue('');
		await expect(shell.getByRole('log').getByText(REPLY)).toBeVisible();
		const agent = shell.locator('[data-testid="chat-message"][data-role="agent"]');
		await expect(agent).toHaveCount(1);
		const customer = shell.locator('[data-testid="chat-message"][data-role="customer"]', {
			hasText: 'Where is my airfryer?',
		});
		await expect(customer).toHaveCount(1);
		// The customer message carries its server timestamp in every look.
		await expect(customer.locator('time')).toHaveCount(1);
		if (look === 'whatsapp') {
			// Answered by the agent: the sent message shows read ticks.
			await expect(customer.getByTestId('chat-ticks')).toHaveAttribute('data-state', 'read');
			await expect(customer.getByRole('img', { name: 'Read' })).toBeVisible();
		}
		await expect(shell.getByTestId('chat-typing')).toHaveCount(0);
		expect(state.log.filter((m) => m.role === 'customer')).toHaveLength(1);
		// Job-scoped like the console: the conversation routes go through /api/jobs/{job}.
		expect(posts(sent, '/api/conversations')[0]?.job).toBe('customer_care_agent');
		await noHorizontalScroll(page);
	});
}

test('the look switcher persists the choice for this viewer across reloads', async ({ page }) => {
	await conversation(page);
	await page.goto('/chat');
	await expect(page.locator('[data-look="teams"]')).toBeVisible();
	await page.getByRole('button', { name: 'Slack-style' }).click();
	await expect(page.locator('[data-look="slack"]')).toBeVisible();
	await expect(page.locator('[data-look="teams"]')).toHaveCount(0);
	expect(await page.evaluate((k) => localStorage.getItem(k), LOOK_KEY)).toBe('slack');
	await page.reload();
	await expect(page.locator('[data-look="slack"]')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Slack-style' })).toHaveAttribute('aria-pressed', 'true');
	await page.getByRole('button', { name: 'WhatsApp-style' }).click();
	await page.reload();
	await expect(page.locator('[data-look="whatsapp"]')).toBeVisible();
});

test('the agent shows as typing while a turn is in flight', async ({ page }) => {
	await conversation(page, { hold: 1500 });
	await page.goto('/chat');
	const input = page.getByLabel('Message');
	await input.fill('Where is my airfryer?');
	await input.press('Enter');
	await expect(page.getByTestId('chat-typing')).toBeVisible();
	await expect(page.getByText('Care agent is typing…')).toBeAttached();
	await expect(page.getByRole('log').getByText(REPLY)).toBeVisible();
	await expect(page.getByTestId('chat-typing')).toHaveCount(0);
});

test("a rejected send shows the console's resend affordance, and Retry message sends it", async ({
	page,
}) => {
	const { state } = await conversation(page);
	state.reject = true;
	await page.addInitScript(([k, v]) => localStorage.setItem(k, v), [LOOK_KEY, 'whatsapp'] as const);
	await page.goto('/chat');
	const input = page.getByLabel('Message');
	await input.fill('Where is my airfryer?');
	await input.press('Enter');
	await expect(page.getByText('Not sent: text must not be empty')).toBeVisible();
	const failed = page.locator('[data-testid="chat-message"][data-status="failed"]');
	await expect(failed).toContainText('Where is my airfryer?');
	await expect(failed.getByRole('img', { name: 'Not sent' })).toBeVisible();
	state.reject = false;
	await page.getByRole('button', { name: 'Retry message' }).click();
	await expect(page.getByRole('log').getByText(REPLY)).toBeVisible();
	await expect(page.getByText(/Not sent:/)).toHaveCount(0);
	await expect(failed).toHaveCount(0);
});

test('every look fits a phone-width screen without horizontal scroll', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await conversation(page);
	await page.goto('/chat');
	for (const [look, label] of LOOKS) {
		await page.getByRole('button', { name: label }).click();
		const shell = page.locator(`[data-look="${look}"]`);
		await expect(shell).toBeVisible();
		await expect(shell.getByLabel('Message')).toBeVisible();
		await expect(shell.getByRole('button', { name: 'Send' })).toBeVisible();
		// The side list is hidden at this width; the conversation takes the room.
		await expect(shell.getByRole('region', { name: 'Conversation with Care agent' })).toBeVisible();
		const box = await shell.getByRole('button', { name: 'Send' }).boundingBox();
		expect(box && box.x + box.width).toBeLessThanOrEqual(390);
		await noHorizontalScroll(page);
	}
});

test('Customer chat is in the nav for a tenant user and opens the chat page', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await page.goto('/console');
	await expect(page.getByRole('button', { name: /navigation/ })).toBeVisible();
	const menu = page.getByRole('button', { name: 'Open navigation' });
	if (await menu.isVisible()) await menu.click();
	await page.getByRole('link', { name: 'Customer chat' }).click();
	await expect(page).toHaveURL(/\/chat(\?|$)/);
	await expect(page.getByRole('heading', { name: 'Customer chat' })).toBeVisible();
});
