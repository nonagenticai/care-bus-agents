import { expect, test, type Page } from '@playwright/test';
import { apiUrl, mockApi, overlay, type Sent } from './mock-api';

const posts = (sent: Sent[], path: string) => sent.filter((r) => r.method === 'POST' && r.path === path);
/** Phone-width console shows one panel at a time; on desktop every panel is visible and this is a no-op. */
async function view(page: Page, name: 'Chat' | 'Activity' | 'Details') {
	// isVisible() does not wait: let the console render first (it mounts after GET /api/me and /api/jobs).
	await expect(page.getByLabel('Customer message')).toBeAttached();
	const tab = page.getByRole('tab', { name, exact: true });
	if (await tab.isVisible()) await tab.click();
}

test('a delivery question shows the reply and the bus messages in the right lanes', async ({ page }) => {
	await mockApi(page, {
		override: { '/api/cases/CASE-1': overlay('case'), '/api/conversations/conv-1': overlay('conversation') },
	});
	await page.goto('/console');
	await page.getByRole('button', { name: 'Where is my airfryer?' }).click();
	await expect(
		page.getByText('Your Philips Airfryer XXL (ORD-58790) is on its way with PostNL.'),
	).toBeVisible();
	await view(page, 'Activity');
	await expect(page.getByLabel('Case activity')).toContainText('Observed from bus events');
	await expect(page.getByLabel('Case activity')).toContainText('CASE-1');
	const customer = page.getByLabel('customer-bus');
	const tool = page.getByLabel('tool-bus');
	await expect(customer.getByText('logistics.delivery-status.request.qualified')).toBeVisible();
	await expect(tool.getByText('logistics.delivery-status.tool.response')).toBeVisible();
	// Ack pills come from the recipient's XACK (event: ack), not from a reply being on the bus.
	const qualified = customer.getByRole('button', { name: /logistics\.delivery-status\./ });
	await expect(qualified.locator('.ack')).toHaveText('acked');
	await expect(qualified).toContainText('interaction → orchestrator');
	const response = tool.getByRole('button', { name: /logistics\.delivery-status\./ });
	await expect(response.locator('.ack')).toHaveText('pending');
	await tool.getByText('logistics.delivery-status.tool.response').click();
	const inspector = page.getByRole('complementary', { name: 'Inspector' });
	await expect(inspector).toContainText('1792065600400-0');
	// The envelope and stream-entry JSON live on the Raw tab; Summary is the plain-language view.
	await inspector.getByRole('tab', { name: 'Raw' }).click();
	await expect(inspector).toContainText('"consumer_group": "worker-delivery"');
	await expect(inspector).toContainText('"ack": "pending"');
	await view(page, 'Activity');
	await customer.getByText('logistics.delivery-status.request.qualified').click();
	await expect(inspector).toContainText('"consumer_group": "orchestrator"');
	await expect(inspector).toContainText('"ack": "acked"');
	// The chat header shows the customer the server returned from POST /api/conversations.
	const header = page.locator('.cust:not(.note)');
	await expect(header).toContainText('Anna de Vries');
	await expect(header).toContainText('Gold');
	const overflow = await page.evaluate(
		() => document.documentElement.scrollWidth > document.documentElement.clientWidth,
	);
	expect(overflow).toBe(false);
});

test('the customer can keep typing while the agent replies; queued messages go out together after the reply', async ({
	page,
}) => {
	await mockApi(page);
	// A server log that holds only what was actually posted, so the echo of each turn is new.
	const base = overlay('conversation');
	const log: { conversation_id: string; role: 'customer' | 'agent'; text: string; at_ms: number }[] = [];
	const posts: string[] = [];
	// Registered after mockApi, so these run first.
	await page.route(apiUrl('/api/conversations/conv-1'), (route) =>
		route.fulfill({ json: { ...base, conversation_id: 'conv-1', messages: log, case_ids: [] } }),
	);
	await page.route(apiUrl('/api/conversations/conv-1/messages'), async (route) => {
		const text = (JSON.parse(route.request().postData() ?? '{}') as { text: string }).text;
		posts.push(text);
		// Hold the first turn so the agent is still "replying" while the customer keeps typing.
		if (posts.length === 1) await new Promise((r) => setTimeout(r, 2000));
		const at = 1_000 * posts.length;
		log.push({ conversation_id: 'conv-1', role: 'customer', text, at_ms: at });
		log.push({ conversation_id: 'conv-1', role: 'agent', text: `Reply ${posts.length}`, at_ms: at + 1 });
		await route.fulfill({ json: { turn: 'ask', text: `Reply ${posts.length}` } });
	});
	await page.goto('/console');
	const input = page.getByLabel('Customer message');
	await input.fill('Where is my airfryer?');
	await input.press('Enter');
	await input.fill('It was order ORD-58790');
	await input.press('Enter');
	await input.fill('and it is urgent');
	await input.press('Enter');
	// Never blocked: the input clears and both follow-ups show at once, before the reply.
	await expect(input).toHaveValue('');
	await expect(input).toBeEnabled();
	const chat = page.getByLabel('Customer chat');
	await expect(chat.getByText('It was order ORD-58790')).toBeVisible();
	await expect(chat.getByText('and it is urgent')).toBeVisible();
	expect(posts).toEqual(['Where is my airfryer?']);
	await expect(chat.getByText('Reply 1')).toBeVisible();
	// After the reply, both queued messages go out as ONE turn, and nothing is left pending.
	await expect.poll(() => posts.length).toBe(2);
	expect(posts[1]).toBe('It was order ORD-58790\nand it is urgent');
	await expect(chat.getByText('Reply 2')).toBeVisible();
	await expect(chat.locator('.pending')).toHaveCount(0);
});

test('a message sent before the conversation exists is kept and sent once it does', async ({ page }) => {
	const sent = await mockApi(page);
	// Registered after mockApi, so it runs first: the conversation start is slow.
	await page.route(apiUrl('/api/conversations'), async (route) => {
		if (route.request().method() === 'POST') await new Promise((r) => setTimeout(r, 1500));
		return route.fallback();
	});
	await page.goto('/console');
	const input = page.getByLabel('Customer message');
	await input.fill('Where is my airfryer?');
	await input.press('Enter');
	const chat = page.getByLabel('Customer chat');
	await expect(chat.locator('.bub.pending', { hasText: 'Where is my airfryer?' })).toBeVisible();
	await expect(
		chat.getByText('Your Philips Airfryer XXL (ORD-58790) is on its way with PostNL.'),
	).toBeVisible();
	await expect.poll(() => posts(sent, '/api/conversations/conv-1/messages').length).toBe(1);
	expect(posts(sent, '/api/conversations')).toHaveLength(1);
	await expect(page).toHaveURL(/[?&]conversation=conv-1/);
});

test('the conversation id lives in the URL; a reload resumes it without resending', async ({ page }) => {
	const sent = await mockApi(page, {
		override: { '/api/cases/CASE-1': overlay('case'), '/api/conversations/conv-1': overlay('conversation') },
	});
	await page.goto('/console');
	await page.getByRole('button', { name: 'Where is my airfryer?' }).click();
	const reply = page.getByText('Your Philips Airfryer XXL (ORD-58790) is on its way with PostNL.');
	await expect(reply).toBeVisible();
	await expect(page).toHaveURL(/[?&]conversation=conv-1/);
	await page.reload();
	await expect(reply).toBeVisible();
	await expect(page).toHaveURL(/[?&]conversation=conv-1/);
	expect(posts(sent, '/api/conversations/conv-1/messages')).toHaveLength(1);
	expect(posts(sent, '/api/conversations')).toHaveLength(1);
});

test('a conversation in the URL wins over the one stored for the tab', async ({ page }) => {
	await page.addInitScript(() => sessionStorage.setItem('carebus.console.conversation', 'conv-old'));
	const sent = await mockApi(page, {
		override: { '/api/cases/CASE-1': overlay('case'), '/api/conversations/conv-1': overlay('conversation') },
	});
	await page.goto('/console?conversation=conv-1');
	await expect(
		page.getByText('Your Philips Airfryer XXL (ORD-58790) is on its way with PostNL.'),
	).toBeVisible();
	expect(sent.some((r) => r.path === '/api/conversations/conv-old')).toBe(false);
	expect(posts(sent, '/api/conversations')).toHaveLength(0);
	expect(await page.evaluate(() => sessionStorage.getItem('carebus.console.conversation'))).toBe('conv-1');
});

test('a case filter that matches nothing in a lane says so; selecting an event opens its details', async ({
	page,
}) => {
	const one = overlay('case');
	const other = {
		...one,
		trace: [
			{
				...one.trace[1],
				stream: 'bus:worker:logistics:delivery',
				entry_id: '1792065600900-0',
				consumer_group: 'worker-delivery',
				envelope: {
					...one.trace[1].envelope,
					id: '1792065600900-0',
					topic: 'logistics.delivery-status.work.requested',
					correlation_id: 'CASE-2',
					published_at_ms: 1792065600900,
					payload: { worker: 'delivery', action: 'track_shipment', attempt: 1 },
				},
			},
		],
	};
	await mockApi(page, {
		override: {
			'/api/cases/CASE-1': one,
			'/api/cases/CASE-2': other,
			'/api/conversations/conv-1': { ...overlay('conversation'), case_ids: ['CASE-1', 'CASE-2'] },
		},
	});
	await page.goto('/console?conversation=conv-1');
	await view(page, 'Activity');
	const worker = page.getByLabel('worker-bus');
	await expect(worker.locator('.msg')).toHaveCount(1);
	await page.getByLabel('Filter trace by case').selectOption('CASE-1');
	await expect(worker).toContainText('No events for case CASE-1 in this lane.');
	await expect(page.getByLabel('Case activity')).toContainText('CASE-1');
	await page.getByLabel('Filter trace by case').selectOption('CASE-2');
	await expect(page.getByLabel('Case activity')).toContainText(
		'Orchestrator asked the delivery worker to track shipment.',
	);
	await worker.locator('.msg').click();
	// On a phone this switches to the Details view; on desktop the inspector is beside the lanes.
	await expect(page.getByRole('complementary', { name: 'Inspector' })).toBeVisible();
	await expect(page.getByRole('complementary', { name: 'Inspector' })).toContainText('1792065600900-0');
});

test('fault injection sits in a collapsed demo-only group, read-only unless super_admin', async ({
	page,
}) => {
	await mockApi(page, { role: 'tenant_user' });
	await page.goto('/console');
	const demo = page.locator('details.demo');
	await expect(demo).toContainText('demo only');
	const fault = page.getByRole('button', { name: 'Next carrier call times out' });
	await expect(fault).toBeHidden();
	await demo.locator('summary').click();
	await expect(fault).toBeVisible();
	await expect(fault).toBeDisabled();
	await expect(fault).toHaveAttribute('title', 'Needs the super_admin role');
});

test('a conversation that fails to load (not 404) is kept with its outbox; Reload conversation resumes it', async ({
	page,
}) => {
	await page.addInitScript(() =>
		sessionStorage.setItem(
			'carebus.console.outbox.conv-1',
			JSON.stringify({
				items: [{ id: 1, text: 'Keep me', status: 'failed', batch: null, after: 0 }],
				nextId: 2,
			}),
		),
	);
	const sent = await mockApi(page, {
		override: { '/api/cases/CASE-1': overlay('case') },
	});
	let down = true;
	await page.route(apiUrl('/api/conversations/conv-1'), (route) =>
		down && route.request().method() === 'GET'
			? route.fulfill({ status: 503, json: { error: { code: 'unavailable', message: 'store down' } } })
			: route.fallback(),
	);
	await page.goto('/console?conversation=conv-1');
	await expect(page.getByText(/Conversation unavailable/)).toBeVisible();
	await expect(page).toHaveURL(/[?&]conversation=conv-1/);
	expect(posts(sent, '/api/conversations')).toHaveLength(0);
	down = false;
	await page.getByRole('button', { name: 'Reload conversation' }).click();
	await expect(page.locator('.bub.pending', { hasText: 'Keep me' })).toContainText('not sent');
	await expect(page.getByText(/Conversation unavailable/)).toHaveCount(0);
	expect(posts(sent, '/api/conversations')).toHaveLength(0);
	expect(posts(sent, '/api/conversations/conv-1/messages')).toHaveLength(0);
});

test('a conversation the server no longer has (404) is replaced by a new one', async ({ page }) => {
	const sent = await mockApi(page);
	await page.route(apiUrl('/api/conversations/conv-gone'), (route) =>
		route.fulfill({ status: 404, json: { error: { code: 'not_found', message: 'no such conversation' } } }),
	);
	await page.goto('/console?conversation=conv-gone');
	await expect(page).toHaveURL(/[?&]conversation=conv-1/);
	expect(posts(sent, '/api/conversations')).toHaveLength(1);
	await expect(page.getByText(/Conversation unavailable/)).toHaveCount(0);
});

test('in a short window the activity summary and agent strip keep their height and never overlay the lanes', async ({
	page,
}, info) => {
	test.skip(info.project.name === 'phone', 'phone shows one panel at a time');
	// Wide enough for the side-by-side layout (below ~960px of console the panels stack and nothing is squeezed).
	await page.setViewportSize({ width: 1440, height: 600 });
	await mockApi(page, {
		override: { '/api/cases/CASE-1': overlay('case'), '/api/conversations/conv-1': overlay('conversation') },
	});
	await page.goto('/console');
	await page.getByRole('button', { name: 'Where is my airfryer?' }).click();
	await expect(page.getByLabel('Case activity')).toContainText('CASE-1');
	const box = async (label: string) => (await page.getByLabel(label, { exact: true }).boundingBox())!;
	const activity = await box('Case activity');
	const agents = await box('Agents');
	const customer = await box('customer-bus');
	// Each summary row is at least as tall as its content, and each section starts below the previous one.
	for (const label of ['Case activity', 'Agents']) {
		const el = page.getByLabel(label, { exact: true });
		const [client, scroll] = await el.evaluate((n) => [n.clientHeight, n.scrollHeight]);
		expect(scroll - client, `${label} is clipped`).toBeLessThanOrEqual(1);
	}
	expect(agents.y).toBeGreaterThanOrEqual(activity.y + activity.height - 1);
	expect(customer.y).toBeGreaterThanOrEqual(agents.y + agents.height - 1);
});
