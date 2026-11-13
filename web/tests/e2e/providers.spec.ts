import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { mockApi } from './mock-api';

test('providers page lists each capability with its owner, source and the probe caveat', async ({ page }) => {
	await mockApi(page);
	await page.goto('/admin/providers');
	await expect(page.getByRole('heading', { name: 'Providers', exact: true })).toBeVisible();
	const rerank = page.getByRole('row', { name: /Reranking/ });
	await expect(rerank).toContainText('strata_rag');
	await expect(rerank).toContainText('Deployment default');
	await expect(rerank).toContainText('cohere · BAAI/bge-reranker-v2-m3');
	const chat = page.getByRole('row', { name: /Chat/ });
	await expect(chat).toContainText('strata_rag');
	await expect(chat).toContainText('Deployment default');
	await expect(chat).toContainText('openai · local-llm');
	await expect(page.getByRole('row', { name: /OCR/ })).toContainText('strata_ingest');
	await expect(page.getByRole('row', { name: /Vision/ })).toContainText('Not configured');
	await expect(
		page.getByText('The provider must be reachable over public https; registration makes a live test call.'),
	).toBeVisible();
});

test('registering a provider sends the key once, and an unchanged base_url keeps the stored key', async ({
	page,
}) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	await page
		.getByRole('row', { name: /Vision/ })
		.getByRole('button')
		.click();
	const drawer = page.getByRole('dialog', { name: 'Vision' });
	await expect(drawer.getByText(/registration makes a live test call/)).toBeVisible();
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer.getByText('Enter the API key.')).toBeVisible();
	await drawer.getByLabel('Base URL').fill('https://vision.example.eu/v1');
	await drawer.getByLabel(/^Model(?! id)/).fill('acme-vl');
	await drawer.getByLabel('API key').fill('k-test-value');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer).toBeHidden();
	const put = sent.find((s) => s.method === 'PUT' && s.path === '/api/providers/slots/vision');
	expect(JSON.parse(put!.body!)).toMatchObject({
		base_url: 'https://vision.example.eu/v1',
		dialect: 'openai',
		model: 'acme-vl',
		api_key: 'k-test-value',
		timeout_ms: 60000,
		concurrency: 4,
	});
	expect(JSON.parse(put!.body!)).not.toHaveProperty('dims');

	await page
		.getByRole('row', { name: /Embeddings/ })
		.getByRole('button')
		.click();
	const embed = page.getByRole('dialog', { name: 'Embeddings' });
	await expect(embed.getByLabel('API key')).toHaveValue('');
	await embed.getByRole('button', { name: 'Save' }).click();
	await expect(embed).toBeHidden();
	const kept = sent.find((s) => s.method === 'PUT' && s.path === '/api/providers/slots/embed');
	expect(JSON.parse(kept!.body!)).not.toHaveProperty('api_key');
	expect(JSON.parse(kept!.body!).dims).toBe(1024);
});

test("an owner's refusal is shown next to its field and the drawer stays open", async ({ page }) => {
	await mockApi(page);
	await page.route('http://api.e2e.test/api/providers/slots/embed', (route) =>
		route.fulfill({
			status: 422,
			contentType: 'application/json',
			body: JSON.stringify({ error: { code: 'region_not_allowed', message: 'regions must contain eu' } }),
		}),
	);
	await page.goto('/admin/providers');
	await page
		.getByRole('row', { name: /Embeddings/ })
		.getByRole('button')
		.click();
	const embed = page.getByRole('dialog', { name: 'Embeddings' });
	await embed.getByLabel('Regions').fill('us');
	await embed.getByRole('button', { name: 'Save' }).click();
	await expect(embed.getByText('regions must contain eu')).toBeVisible();
});

test('a tenant_user gets a read-only form with a visible role hint', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await page.goto('/admin/providers');
	await page
		.getByRole('row', { name: /Embeddings/ })
		.getByRole('button')
		.click();
	const save = page.getByRole('dialog').getByRole('button', { name: 'Save', exact: true });
	await expect(save).toBeDisabled();
	await expect(save).toHaveAttribute('title', 'Needs the tenant_admin role');
	await expect(
		page.getByRole('dialog').getByText('Changing providers needs the tenant_admin role.'),
	).toBeVisible();
	await expect(page.getByLabel('Base URL')).toBeDisabled();
	await expect(page.getByLabel('API key')).toBeDisabled();
	await expect(page.getByLabel('Model', { exact: true })).toBeDisabled();
});

test('a chat provider goes to strata_rag with thinking and temperature only once they are set', async ({
	page,
}) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	await page.getByRole('row', { name: /Chat/ }).getByRole('button').click();
	const drawer = page.getByRole('dialog', { name: 'Chat' });
	await expect(drawer.getByLabel(/Timeout/)).toHaveValue('60000');
	await expect(drawer.getByLabel('Dimensions')).toHaveCount(0);
	await expect(drawer.getByLabel(/Thinking/)).not.toBeChecked();
	await expect(drawer.getByText('Not sent (provider default)')).toBeVisible();
	await drawer.getByLabel('Base URL').fill('https://chat.example.eu/v1');
	await drawer.getByLabel(/^Model(?! id)/).fill('acme-chat');
	await drawer.getByLabel('API key').fill('k-chat-value');
	await drawer.getByLabel(/Temperature/).fill('2.5');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer.getByText('0 to 2.')).toBeVisible();
	await drawer.getByLabel(/Temperature/).fill('');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer).toBeHidden();
	const untouched = JSON.parse(
		sent.find((s) => s.method === 'PUT' && s.path === '/api/providers/slots/chat')!.body!,
	);
	expect(untouched).toMatchObject({
		base_url: 'https://chat.example.eu/v1',
		dialect: 'openai',
		model: 'acme-chat',
		api_key: 'k-chat-value',
		timeout_ms: 60000,
		thinking: null,
		temperature: null,
	});
	expect(untouched).not.toHaveProperty('dims');

	await page.getByRole('row', { name: /Chat/ }).getByRole('button').click();
	await drawer.getByLabel('Base URL').fill('https://chat.example.eu/v1');
	await drawer.getByLabel(/^Model(?! id)/).fill('acme-chat');
	await drawer.getByLabel('API key').fill('k-chat-value');
	await drawer.getByLabel(/Thinking/).check();
	await expect(drawer.getByText('On: the model reasons before answering')).toBeVisible();
	await drawer.getByLabel(/Temperature/).fill('0.2');
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer).toBeHidden();
	const puts = sent.filter((s) => s.method === 'PUT' && s.path === '/api/providers/slots/chat');
	expect(puts).toHaveLength(2);
	expect(JSON.parse(puts[1]!.body!)).toMatchObject({ thinking: true, temperature: 0.2 });
});

/** Drops the PUT for one capability (no answer reaches the page); `readsDown` makes the re-read fail too. */
async function loseWrite(page: import('@playwright/test').Page, capability: string) {
	const state = { puts: 0, readsDown: false };
	await page.route('http://api.e2e.test/api/providers**', (route) => {
		const req = route.request();
		const path = new URL(req.url()).pathname;
		if (req.method() === 'PUT' && path === `/api/providers/slots/${capability}`) {
			state.puts++;
			return route.abort('connectionreset');
		}
		if (req.method() === 'GET' && state.readsDown) return route.abort('connectionrefused');
		return route.fallback();
	});
	return state;
}

test('a lost answer is reconciled: the stored provider differs, so the drawer and draft stay', async ({
	page,
}) => {
	await mockApi(page);
	const lost = await loseWrite(page, 'vision');
	await page.goto('/admin/providers');
	await page
		.getByRole('row', { name: /Vision/ })
		.getByRole('button')
		.click();
	const drawer = page.getByRole('dialog', { name: 'Vision' });
	await drawer.getByLabel('Base URL').fill('https://vision.example.eu/v1');
	await drawer.getByLabel(/^Model(?! id)/).fill('acme-vl');
	await drawer.getByLabel('API key').fill('k-test-value');
	lost.readsDown = true;
	await drawer.getByRole('button', { name: 'Save' }).click();
	await expect(drawer.getByRole('status')).toHaveText(
		/Still can't reach the server; your changes are kept\./,
	);
	lost.readsDown = false;
	await drawer.getByRole('button', { name: 'Check again' }).click();
	await expect(drawer.getByRole('status')).toHaveText(
		'Not saved — your changes are still here; you can save again.',
	);
	await expect(drawer.getByLabel('Base URL')).toHaveValue('https://vision.example.eu/v1');
	expect(lost.puts, 'never resubmitted').toBe(1);
});

test('a lost answer is reconciled: the stored provider matches, so it counts as saved', async ({ page }) => {
	await mockApi(page);
	const lost = await loseWrite(page, 'embed');
	await page.goto('/admin/providers');
	await page
		.getByRole('row', { name: /Embeddings/ })
		.getByRole('button')
		.click();
	const embed = page.getByRole('dialog', { name: 'Embeddings' });
	await embed.getByRole('button', { name: 'Save' }).click();
	await expect(embed).toBeHidden();
	await expect(page.getByText('Saved — confirmed after reloading.')).toBeVisible();
	expect(lost.puts).toBe(1);
});

// ---- LLM providers (spec 2026-10-18 §2) ----------------------------------------------------------------------------

const LLM_ID = '0195f48b-d001-7000-8000-0000000000a1';
const LAYA_ID = '0195f48b-d001-7000-8000-0000000000a2';

test('LLM providers sit above the slots: list, token state, uses and the gates-off warning', async ({
	page,
}) => {
	await mockApi(page);
	await page.goto('/admin/providers');
	const llm = page.getByRole('table', { name: 'LLM providers' });
	const vllm = llm.getByRole('row', { name: /Local vLLM/ });
	await expect(vllm).toContainText('OpenAI-compatible');
	await expect(vllm).toContainText('https://llm.example.eu/v1');
	await expect(vllm).toContainText('Set');
	await expect(llm.getByRole('row', { name: /Laya EU/ })).toContainText('Not set');
	// Above the strata slots.
	const llmTop = (await llm.boundingBox())!.y;
	const slotsTop = (await page.getByRole('heading', { name: 'Knowledge and ingestion' }).boundingBox())!.y;
	expect(llmTop).toBeLessThan(slotsTop);
	await expect(page.getByRole('region', { name: 'Use Qualify' })).toContainText('Local vLLM · local-llm');
	const gates = page.getByRole('region', { name: 'Use Laya gates' });
	await expect(gates.getByText('Laya gates off').first()).toBeVisible();
	await expect(gates.getByLabel('Laya gates provider')).toHaveValue('');
});

test('unbound qualify shows the platform default', async ({ page }) => {
	await mockApi(page, { override: { '/api/providers': { ...llmBody(), uses: [] } } });
	await page.goto('/admin/providers');
	await expect(page.getByRole('region', { name: 'Use Qualify' })).toContainText(
		'Platform default: local-llm @ http://vllm.example:8001/v1',
	);
});

test('unbound qualify without a platform default runs on keyword rules', async ({ page }) => {
	const none = { base_url: null, model: null, configured: false };
	await mockApi(page, { override: { '/api/providers': { ...llmBody(), uses: [], platform_default: none } } });
	await page.goto('/admin/providers');
	const qualify = page.getByRole('region', { name: 'Use Qualify' });
	await expect(qualify.getByText('No LLM — keyword rules').first()).toBeVisible();
	await expect(qualify).not.toContainText('Platform default');
});

test('adding a provider sends the token once; editing with a blank token keeps it; Clear token sends ""', async ({
	page,
}) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	await page.getByRole('button', { name: 'Add provider' }).click();
	const add = page.getByRole('dialog', { name: 'Add LLM provider' });
	await add.getByLabel('Base URL').fill('https://u:p@laya.example.eu');
	await add.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(add.getByText('Enter a name.')).toBeVisible();
	await expect(add.getByText(/No user or password in the URL/)).toBeVisible();
	await add.getByLabel('Name').fill('Laya staging');
	await add.getByLabel('Kind').selectOption('laya');
	await add.getByLabel('Base URL').fill('https://laya-staging.example.eu');
	await add.getByLabel('Token').fill('lic-123');
	await add.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(add).toBeHidden();
	const post = sent.find((s) => s.method === 'POST' && s.path === '/api/providers');
	expect(JSON.parse(post!.body!)).toEqual({
		name: 'Laya staging',
		kind: 'laya',
		base_url: 'https://laya-staging.example.eu',
		token: 'lic-123',
	});
	const row = page.getByRole('table', { name: 'LLM providers' }).getByRole('row', { name: /Laya staging/ });
	await expect(row).toContainText('Set');

	await page.getByRole('button', { name: 'Local vLLM', exact: true }).click();
	const edit = page.getByRole('dialog', { name: 'Local vLLM' });
	await expect(edit.getByLabel('Token')).toHaveValue('');
	await expect(edit.getByText('Used by: Qualify')).toBeVisible();
	await edit.getByLabel('Name').fill('Local vLLM (eu)');
	await edit.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(edit).toBeHidden();
	const kept = sent.find((s) => s.method === 'PUT' && s.path === `/api/providers/${LLM_ID}`);
	expect(JSON.parse(kept!.body!)).not.toHaveProperty('token');
	expect(JSON.parse(kept!.body!).name).toBe('Local vLLM (eu)');

	await page.getByRole('button', { name: 'Local vLLM (eu)', exact: true }).click();
	const again = page.getByRole('dialog', { name: 'Local vLLM (eu)' });
	await again.getByRole('button', { name: 'Clear token' }).click();
	await expect(again.getByLabel('Token')).toBeDisabled();
	await again.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(again).toBeHidden();
	const cleared = sent.filter((s) => s.method === 'PUT' && s.path === `/api/providers/${LLM_ID}`);
	expect(JSON.parse(cleared[1]!.body!).token).toBe('');
	await expect(
		page.getByRole('table', { name: 'LLM providers' }).getByRole('row', { name: /Local vLLM \(eu\)/ }),
	).toContainText('Not set');
});

test('deleting a provider bound to a use explains provider_in_use and keeps the drawer open', async ({
	page,
}) => {
	await mockApi(page);
	await page.goto('/admin/providers');
	await page.getByRole('button', { name: 'Local vLLM', exact: true }).click();
	const drawer = page.getByRole('dialog', { name: 'Local vLLM' });
	await drawer.getByRole('button', { name: 'Delete provider' }).click();
	await drawer
		.getByRole('alertdialog', { name: 'Delete provider' })
		.getByRole('button', { name: 'Delete' })
		.click();
	await expect(drawer.getByText(/still bound to a use/)).toBeVisible();
	await expect(drawer).toBeVisible();
});

test('Test shows ok, status, latency and models; binding gates picks a model from that test', async ({
	page,
}) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	const gates = page.getByRole('region', { name: 'Use Laya gates' });
	// The qualify picker offers only openai providers, the gates picker only laya ones.
	await expect(
		page.getByRole('region', { name: 'Use Qualify' }).getByLabel('Qualify provider').locator('option'),
	).toHaveText(['None (platform default)', 'Local vLLM']);
	await expect(gates.getByLabel('Laya gates provider').locator('option')).toHaveText([
		'None (gates off)',
		'Laya EU',
	]);
	await gates.getByLabel('Laya gates provider').selectOption(LAYA_ID);
	// No test yet: the model is free text.
	await expect(gates.getByRole('textbox', { name: 'Laya gates model' })).toBeVisible();

	await page.getByRole('button', { name: 'Test Laya EU' }).click();
	const result = page.getByRole('status', { name: 'Test result for Laya EU' });
	await expect(result).toContainText('ok · HTTP 200 · 84 ms');
	await expect(result).toContainText('jev-system1, jev-system1-mini');

	const model = gates.getByRole('combobox', { name: 'Laya gates model' });
	await expect(model.locator('option')).toHaveText(['Choose a model', 'jev-system1', 'jev-system1-mini']);
	await model.selectOption('jev-system1');
	await gates.getByRole('button', { name: 'Save Laya gates' }).click();
	await expect(gates.getByText('Laya EU · jev-system1')).toBeVisible();
	await expect(gates.getByText('Laya gates off')).toHaveCount(0);
	const put = sent.find((s) => s.method === 'PUT' && s.path === '/api/providers/uses/gates');
	expect(JSON.parse(put!.body!)).toEqual({ provider_id: LAYA_ID, model: 'jev-system1' });
});

test('the form Test checks the typed URL and token without saving', async ({ page }) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	await page.getByRole('button', { name: 'Add provider' }).click();
	const add = page.getByRole('dialog', { name: 'Add LLM provider' });
	await add.getByLabel('Base URL').fill('https://llm.acme.example/v1');
	await add.getByLabel('Token').fill('sk-test');
	await add.getByRole('button', { name: 'Test', exact: true }).click();
	const result = add.getByRole('status', { name: 'Form test result' });
	await expect(result).toContainText('ok · HTTP 200 · 84 ms');
	await expect(result).toContainText('local-llm, qwen3-27b');
	const test = sent.find((s) => s.method === 'POST' && s.path === '/api/providers/test');
	expect(JSON.parse(test!.body!)).toEqual({
		kind: 'openai',
		base_url: 'https://llm.acme.example/v1',
		token: 'sk-test',
	});
	expect(sent.some((s) => s.method === 'POST' && s.path === '/api/providers')).toBe(false);
	await expect(add).toBeVisible();
});

test('editing: Test uses the stored token for the stored URL, and asks for it again for a new URL', async ({
	page,
}) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	await page.getByRole('button', { name: 'Local vLLM', exact: true }).click();
	const edit = page.getByRole('dialog', { name: 'Local vLLM' });
	await edit.getByRole('button', { name: 'Test', exact: true }).click();
	const result = edit.getByRole('status', { name: 'Form test result' });
	await expect(result).toContainText('ok · HTTP 200 · 84 ms');
	const tests = () => sent.filter((s) => s.method === 'POST' && s.path === '/api/providers/test');
	const first = JSON.parse(tests()[0]!.body!);
	expect(first).toMatchObject({ kind: 'openai', provider_id: LLM_ID });
	expect(first).not.toHaveProperty('token');

	await edit.getByLabel('Base URL').fill('https://llm.other.example/v1');
	await edit.getByRole('button', { name: 'Test', exact: true }).click();
	await expect(result).toContainText('Enter the token again for a new URL');
	expect(JSON.parse(tests()[1]!.body!)).toEqual({
		kind: 'openai',
		base_url: 'https://llm.other.example/v1',
		provider_id: LLM_ID,
	});
	await edit.getByLabel('Token').fill('sk-new');
	await edit.getByRole('button', { name: 'Test', exact: true }).click();
	await expect(result).toContainText('ok · HTTP 200');
	expect(JSON.parse(tests()[2]!.body!)).toEqual({
		kind: 'openai',
		base_url: 'https://llm.other.example/v1',
		token: 'sk-new',
	});
});

test('Use for: a new Laya provider is bound to gates on Save, with a model from the form Test', async ({
	page,
}) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	await page.getByRole('button', { name: 'Add provider' }).click();
	const add = page.getByRole('dialog', { name: 'Add LLM provider' });
	await add.getByLabel('Name').fill('Laya prod');
	await add.getByLabel('Kind').selectOption('laya');
	await add.getByLabel('Base URL').fill('https://laya-prod.example.eu');
	await add.getByLabel('Token').fill('lic-9');
	await expect(add.getByText('Use for chat qualification (qualify)')).toHaveCount(0);
	await add.getByRole('checkbox', { name: 'Use for Laya gates (gates)' }).check();
	// Before a Test: free text, with the known Laya models suggested.
	const free = add.getByRole('combobox', { name: 'Laya gates model' });
	await expect(free).toHaveAttribute('list', 'llm-model-suggestions-gates');
	await expect(add.locator('#llm-model-suggestions-gates option')).toHaveCount(3);
	await add.getByRole('button', { name: 'Test', exact: true }).click();
	const model = add.getByRole('combobox', { name: 'Laya gates model' });
	await expect(model.locator('option')).toHaveText(['Choose a model', 'jev-system1', 'jev-system1-mini']);
	await model.selectOption('jev-system1');
	await add.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(add).toBeHidden();
	const post = sent.find((s) => s.method === 'POST' && s.path === '/api/providers');
	expect(JSON.parse(post!.body!)).toMatchObject({ name: 'Laya prod', kind: 'laya', token: 'lic-9' });
	const bind = sent.find((s) => s.method === 'PUT' && s.path === '/api/providers/uses/gates');
	expect(JSON.parse(bind!.body!)).toEqual({
		provider_id: expect.stringMatching(/^0195f48b-d001-7000-8000-00000000f\d{3}$/),
		model: 'jev-system1',
	});
	const gates = page.getByRole('region', { name: 'Use Laya gates' });
	await expect(gates.getByText('Laya prod · jev-system1')).toBeVisible();
	await expect(gates.getByRole('status', { name: 'gates not bound' })).toHaveCount(0);
});

test('Use for: the bound provider opens checked; unchecking unbinds, and Bind brings it back', async ({
	page,
}) => {
	const sent = await mockApi(page);
	await page.goto('/admin/providers');
	// Unbound gates say so loudly.
	await expect(
		page.getByRole('region', { name: 'Use Laya gates' }).getByRole('status', { name: 'gates not bound' }),
	).toContainText('gates: not bound — Laya gates are off');
	await page.getByRole('button', { name: 'Local vLLM', exact: true }).click();
	const edit = page.getByRole('dialog', { name: 'Local vLLM' });
	const box = edit.getByRole('checkbox', { name: 'Use for chat qualification (qualify)' });
	await expect(box).toBeChecked();
	await expect(edit.getByLabel('Qualify model')).toHaveValue('local-llm');
	await box.uncheck();
	await expect(edit.getByText('Saving unbinds qualify from this provider.')).toBeVisible();
	await edit.getByRole('button', { name: 'Save', exact: true }).click();
	await expect(edit).toBeHidden();
	const unbind = sent.find((s) => s.method === 'PUT' && s.path === '/api/providers/uses/qualify');
	expect(JSON.parse(unbind!.body!)).toEqual({ provider_id: null, model: null });
	const qualify = page.getByRole('region', { name: 'Use Qualify' });
	const notice = qualify.getByRole('status', { name: 'qualify not bound' });
	await expect(notice).toContainText(
		'qualify: not bound — chat uses the platform default (local-llm @ http://vllm.example:8001/v1)',
	);
	await notice.getByRole('button', { name: 'Bind qualify' }).click();
	await expect(qualify.getByLabel('Qualify provider')).toHaveValue(LLM_ID);
	await expect(qualify.getByLabel('Qualify provider')).toBeFocused();
});

test('a tenant_user sees the LLM providers read-only with the role hint', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await page.goto('/admin/providers');
	const add = page.getByRole('button', { name: 'Add provider' });
	await expect(add).toBeDisabled();
	await expect(add).toHaveAttribute('title', 'Needs the tenant_admin role');
	await expect(page.getByRole('button', { name: 'Test Local vLLM' })).toBeDisabled();
	await expect(page.getByLabel('Qualify provider')).toBeDisabled();
	await expect(page.getByRole('button', { name: 'Save Qualify' })).toBeDisabled();
	await page.getByRole('button', { name: 'Local vLLM', exact: true }).click();
	const drawer = page.getByRole('dialog', { name: 'Local vLLM' });
	await expect(drawer.getByText('Changing providers needs the tenant_admin role.')).toBeVisible();
	await expect(drawer.getByLabel('Token')).toBeDisabled();
	await expect(drawer.getByRole('button', { name: 'Save', exact: true })).toBeDisabled();
	await expect(drawer.getByRole('button', { name: 'Delete provider' })).toBeDisabled();
});

/** The generated LLM providers body, for overrides that change one part. */
function llmBody() {
	return JSON.parse(readFileSync('src/lib/api/fixtures/providers/llm_providers.json', 'utf8'));
}
