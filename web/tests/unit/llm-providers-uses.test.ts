// The LLM providers card with every use the server binds (qualify, gates, plan 18's research and research_write):
// one section per use from USE_RULES, no per-use ternaries, and the provider form's "Use for" offers every use of
// the provider's kind.
import { readFileSync } from 'node:fs';
import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Api } from '../../src/lib/api/client';
import type { LlmProviders } from '../../src/lib/api/providers';
import { app } from '../../src/lib/app.svelte';
import { session } from '../../src/lib/auth/session.svelte';
import { meAs } from './me';
import LlmProvidersCard from '../../src/routes/admin/providers/LlmProviders.svelte';

// Research is hidden by default (src/lib/features.ts); these tests cover the research uses as they are when it is
// shown again. research-hidden.test.ts covers the default.
vi.mock('../../src/lib/features', async () => {
	const { LLM_USES } = await import('../../src/lib/api/providers');
	return { RESEARCH_ENABLED: true, SHOWN_LLM_USES: LLM_USES };
});

const body = (): LlmProviders =>
	JSON.parse(readFileSync('src/lib/api/fixtures/providers/llm_providers.json', 'utf8'));

afterEach(() => {
	cleanup();
	app.api = null;
});

async function flush() {
	for (let i = 0; i < 5; i++) await Promise.resolve();
	await new Promise((r) => setTimeout(r, 0));
}

function fakeApi() {
	const d = body();
	const vllm = d.providers.find((p) => p.kind === 'openai')!;
	const api = {
		llmProviders: vi.fn(async () => body()),
		updateLlmProvider: vi.fn(async () => vllm),
		bindLlmUse: vi.fn(async () => ({})),
	};
	app.api = api as unknown as Api;
	return { api, vllm };
}

describe('LLM providers: research uses', () => {
	it('lists a section per use, each with its own "none" option and unbound notice', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		fakeApi();
		render(LlmProvidersCard);
		const research = await screen.findByRole('region', { name: 'Use Research (plan/read/act)' });
		const writer = screen.getByRole('region', { name: 'Use Research memo writer' });
		expect(screen.getByRole('region', { name: 'Use Qualify' })).toBeTruthy();
		expect(screen.getByRole('region', { name: 'Use Laya gates' })).toBeTruthy();
		expect(within(research).getByRole('option', { name: 'None (platform default)' })).toBeTruthy();
		expect(within(writer).getByRole('option', { name: 'None (same as Research)' })).toBeTruthy();
		expect(
			within(screen.getByRole('region', { name: 'Use Laya gates' })).getByRole('option', {
				name: 'None (gates off)',
			}),
		).toBeTruthy();
		// research takes openai providers only
		const opts = within(research)
			.getAllByRole('option')
			.map((o) => o.textContent);
		expect(opts).toEqual(['None (platform default)', 'Local vLLM']);
		expect(
			within(research).getByText(/^research: not bound — research runs use the platform default/),
		).toBeTruthy();
		expect(
			within(writer).getByText(/^research_write: not bound — the research binding writes the memo/),
		).toBeTruthy();
	});

	it('the form of an openai provider offers qualify, research and research_write; Save binds what is checked', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const { api, vllm } = fakeApi();
		render(LlmProvidersCard);
		await fireEvent.click(await screen.findByRole('button', { name: 'Local vLLM' }));
		const drawer = await screen.findByRole('dialog');
		const boxes = within(drawer)
			.getAllByRole('checkbox')
			.map((b) => b.parentElement!.textContent!.trim());
		expect(boxes).toEqual([
			'Use for chat qualification (qualify)',
			'Use for research runs (research)',
			'Use for research memos (research_write)',
		]);
		await fireEvent.click(within(drawer).getByRole('checkbox', { name: 'Use for research runs (research)' }));
		await fireEvent.input(within(drawer).getByLabelText('Research (plan/read/act) model'), {
			target: { value: 'qwen3-27b' },
		});
		await fireEvent.click(within(drawer).getByRole('button', { name: 'Save' }));
		await flush();
		expect(api.updateLlmProvider).toHaveBeenCalledTimes(1);
		// qualify was already bound with its model: only research is sent
		expect(api.bindLlmUse.mock.calls).toEqual([['research', { provider_id: vllm.id, model: 'qwen3-27b' }]]);
	});
});

describe('LLM providers: external LLMs for research (0023)', () => {
	const toggle = () => screen.findByRole('checkbox', { name: 'Allow external LLMs for research runs' });

	it('shows the stored setting, off by default, and saves a change through PUT /api/providers/settings', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const { api } = fakeApi();
		const setLlmSettings = vi.fn(async () => ({ external_llm_allowed: true }));
		Object.assign(api, { setLlmSettings });
		render(LlmProvidersCard);
		const box = (await toggle()) as HTMLInputElement;
		expect(box.checked).toBe(false);
		expect(box.disabled).toBe(false);
		expect(screen.getByText(/Research runs only/)).toBeTruthy();
		await fireEvent.click(box);
		await flush();
		expect(setLlmSettings).toHaveBeenCalledWith({ external_llm_allowed: true });
		// the list is read again after the write
		expect(api.llmProviders.mock.calls.length).toBeGreaterThan(1);
	});

	it('is read-only without tenant_admin', async () => {
		session.set({ sub: 'u2', name: 'B' }, meAs('tenant_user'));
		fakeApi();
		render(LlmProvidersCard);
		expect(((await toggle()) as HTMLInputElement).disabled).toBe(true);
	});
});
