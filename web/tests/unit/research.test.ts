// Research runs (plan 18): the api module (types held to RESEARCH_SHAPES, the request layer, form and citation
// helpers), the run detail (polling, cancel, claims with citations, legal banner) and the research page.
import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, createClient, type Api } from '../../src/lib/api/client';
import { app } from '../../src/lib/app.svelte';
import { session } from '../../src/lib/auth/session.svelte';
import {
	LEGAL_BANNER,
	RESEARCH_SHAPES,
	citationHref,
	claimsWithCitations,
	eliHref,
	emptyResearchDraft,
	isTerminal,
	questionSnippet,
	statusWord,
	toResearchInput,
	validateResearchDraft,
	verifyDetailText,
	verifyVariant,
	type ResearchRun,
	type ResearchRunDetail,
	type ResearchRunSummary,
} from '../../src/lib/api/research';
import type { KnowledgeStoreItem } from '../../src/lib/api/types';
import { meAs } from './me';

// The matter UI of the research page exists only while MATTERS_ENABLED (hidden by default, src/lib/features.ts);
// these tests cover it as it is when shown again. matters-hidden.test.ts covers the default.
vi.mock('../../src/lib/features', async () => {
	const { LLM_USES } = await import('../../src/lib/api/providers');
	return { RESEARCH_ENABLED: true, MATTERS_ENABLED: true, SHOWN_LLM_USES: LLM_USES };
});
import RunDetail from '../../src/routes/admin/jobs/[job]/research/RunDetail.svelte';
import ResearchPage from '../../src/routes/admin/jobs/[job]/research/+page.svelte';

const RUN_ID = '0195f48b-d002-7000-8000-0000000000b1';
const STORE = '6f9619ff-8b86-d011-b42d-00cf4fc964ff';
const OTHER_STORE = '7f9619ff-8b86-d011-b42d-00cf4fc964ff';
const MATTER_ID = '0195f48b-d002-7000-8000-0000000000c1';

const run = (over: Partial<ResearchRun> = {}): ResearchRun => ({
	id: RUN_ID,
	profile: 'legal',
	matter_id: MATTER_ID,
	question: 'Czy najemca może wypowiedzieć umowę najmu bez zachowania terminu?',
	store_ids: [STORE],
	status: 'done',
	budget: { max_steps: 20 },
	steps_used: 3,
	tokens_used: 12345,
	attempts: 1,
	report: '# Memo\n\nThe tenant may terminate under art. 682 KC.',
	error: null,
	cancel_requested: false,
	created_by: 'u1',
	created_at: '2026-10-23T09:00:00Z',
	started_at: '2026-10-23T09:00:01Z',
	finished_at: '2026-10-23T09:03:00Z',
	updated_at: '2026-10-23T09:03:00Z',
	...over,
});

const detail = (over: Partial<ResearchRun> = {}): ResearchRunDetail => ({
	run: run(over),
	steps: [
		{
			ordinal: 0,
			kind: 'model',
			status: 'ok',
			duration_ms: 1200,
			tool: null,
			created_at: '2026-10-23T09:00:02Z',
		},
		{
			ordinal: 1,
			kind: 'tool',
			status: 'ok',
			duration_ms: 340,
			tool: 'eli_get_act',
			created_at: '2026-10-23T09:00:03Z',
		},
	],
	claims: [
		{
			id: 'c2',
			ordinal: 2,
			text: 'A judgment confirms it.',
			status: 'unsupported',
			citation_ids: ['x2', 'missing'],
		},
		{ id: 'c1', ordinal: 1, text: 'Art. 682 KC allows it.', status: 'supported', citation_ids: ['x1'] },
	],
	citations: [
		{
			id: 'x1',
			source_kind: 'eli',
			locator: 'DU/2024/1061',
			title: 'Kodeks cywilny',
			quote: 'najemca może wypowiedzieć najem bez zachowania terminów wypowiedzenia',
			step_ordinal: 1,
			verify_status: 'verified',
			verify_detail: null,
			verified_at: '2026-10-23T09:02:00Z',
			created_at: '2026-10-23T09:01:00Z',
		},
		{
			id: 'x2',
			source_kind: 'saos',
			locator: '123456',
			title: null,
			quote: 'a quote that is not there',
			step_ordinal: 1,
			verify_status: 'quote_mismatch',
			verify_detail: { reason: 'quote not found in step 1' },
			verified_at: '2026-10-23T09:02:00Z',
			created_at: '2026-10-23T09:01:30Z',
		},
	],
});

const keys = (o: object) => Object.keys(o).sort();
const mapKeys = (m: Record<string, unknown>, all = false) =>
	Object.entries(m)
		.filter(([, v]) => all || v === true)
		.map(([k]) => k)
		.sort();

afterEach(() => {
	cleanup();
	app.api = null;
});

async function flush() {
	for (let i = 0; i < 5; i++) await Promise.resolve();
	await new Promise((r) => setTimeout(r, 0));
}

describe('research shapes', () => {
	it('the samples carry exactly the keys routes::research serves', () => {
		const d = detail();
		expect(keys(d)).toEqual(mapKeys(RESEARCH_SHAPES.DETAIL));
		expect(keys(d.run)).toEqual(mapKeys(RESEARCH_SHAPES.RUN));
		for (const s of d.steps) expect(keys(s)).toEqual(mapKeys(RESEARCH_SHAPES.STEP));
		for (const c of d.claims) expect(keys(c)).toEqual(mapKeys(RESEARCH_SHAPES.CLAIM));
		for (const c of d.citations) expect(keys(c)).toEqual(mapKeys(RESEARCH_SHAPES.CITATION));
		expect(mapKeys(RESEARCH_SHAPES.STEP, true)).toContain('request');
	});
});

describe('research helpers', () => {
	it('terminal statuses stop the poll; status words colour the badge', () => {
		expect(['done', 'failed', 'cancelled'].every((s) => isTerminal(s as never))).toBe(true);
		expect(['queued', 'running', 'verifying'].some((s) => isTerminal(s as never))).toBe(false);
		expect(statusWord('done')).toBe('ok');
		expect(statusWord('verifying')).toBe('running');
		expect(statusWord('failed')).toBe('failed');
	});

	it('verified is green, unchecked or unverifiable amber, a failed check red', () => {
		expect(verifyVariant('verified')).toBe('ok');
		expect(verifyVariant('pending')).toBe('warn');
		expect(verifyVariant('unverifiable')).toBe('warn');
		expect(verifyVariant('not_in_force')).toBe('warn');
		expect(verifyVariant('quote_mismatch')).toBe('err');
		expect(verifyVariant('source_unresolved')).toBe('err');
		expect(verifyDetailText(null)).toBe('');
		expect(verifyDetailText('gone')).toBe('gone');
		expect(verifyDetailText({ reason: 'no match', score: 0.4 })).toBe('reason: no match; score: 0.4');
	});

	it('links ELI ids from 2012 on, SAOS judgments, EUR-Lex CELEX and web URLs; nothing else', () => {
		expect(eliHref('DU/2024/1061')).toBe(
			'https://isap.sejm.gov.pl/isap.nsf/DocDetails.xsp?id=WDU20240001061',
		);
		expect(eliHref('https://eli.gov.pl/eli/MP/2019/5')).toBe(
			'https://isap.sejm.gov.pl/isap.nsf/DocDetails.xsp?id=WMP20190000005',
		);
		// before 2012 the ISAP id needs the issue number the ELI does not carry
		expect(eliHref('DU/1964/93')).toBeNull();
		expect(citationHref({ source_kind: 'eli', locator: 'DU/1964/93' })).toBeNull();
		expect(citationHref({ source_kind: 'saos', locator: '123456' })).toBe(
			'https://www.saos.org.pl/judgments/123456',
		);
		expect(citationHref({ source_kind: 'saos', locator: 'II CSK 1/20' })).toBeNull();
		expect(citationHref({ source_kind: 'eurlex', locator: '32016R0679' })).toBe(
			'https://eur-lex.europa.eu/legal-content/PL/TXT/?uri=CELEX:32016R0679',
		);
		expect(citationHref({ source_kind: 'web', locator: 'https://example.eu/a?b=1' })).toBe(
			'https://example.eu/a?b=1',
		);
		expect(citationHref({ source_kind: 'web', locator: 'javascript:alert(1)' })).toBeNull();
		expect(citationHref({ source_kind: 'strata', locator: STORE })).toBeNull();
		expect(citationHref({ source_kind: 'krs', locator: '0000123456' })).toBeNull();
	});

	it('orders claims and resolves their citations, skipping an id without a row', () => {
		const c = claimsWithCitations(detail());
		expect(c.map((x) => x.claim.id)).toEqual(['c1', 'c2']);
		expect(c[0]!.citations.map((x) => x.id)).toEqual(['x1']);
		expect(c[1]!.citations.map((x) => x.id)).toEqual(['x2']);
	});

	it('validates the form as the server would and sends only what was given', () => {
		const d = emptyResearchDraft();
		expect(validateResearchDraft(d)).toEqual({ question: 'Ask a question.' });
		d.question = '  What changed in the VAT rules?  ';
		expect(validateResearchDraft(d)).toEqual({});
		expect(toResearchInput(d)).toEqual({ profile: 'research', question: 'What changed in the VAT rules?' });
		d.question = 'x'.repeat(8001);
		expect(validateResearchDraft(d).question).toMatch(/^At most 8000 characters/);
		d.question = 'q';
		d.budget.max_wall_s = '59';
		d.budget.max_steps = '1.5';
		d.budget.max_tokens = ' 2000 ';
		expect(validateResearchDraft(d)).toEqual({
			max_steps: 'A whole number from 1 to 200.',
			max_wall_s: 'A whole number from 60 to 21600.',
		});
		d.budget.max_wall_s = '600';
		d.budget.max_steps = '';
		d.store_ids = [STORE];
		d.profile = 'legal';
		expect(toResearchInput(d)).toEqual({
			profile: 'legal',
			question: 'q',
			store_ids: [STORE],
			budget: { max_wall_s: 600, max_tokens: 2000 },
		});
		d.store_ids = Array.from({ length: 21 }, (_, i) => `s${i}`);
		expect(validateResearchDraft(d).store_ids).toBe('At most 20 stores.');
	});

	it('a list row shows the first line of the question, cut', () => {
		expect(questionSnippet('first line\nsecond')).toBe('first line');
		expect(questionSnippet('abcdefghij', 5)).toBe('abcd…');
	});
});

describe('research client', () => {
	const json = (status: number, body: unknown) =>
		new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

	it('sends each call to the job route with the right method and body', async () => {
		const fetchFn = vi.fn(async () => json(200, {}));
		const api = createClient({
			baseUrl: 'http://api',
			token: async () => 't',
			job: () => 'legal_desk',
			fetch: fetchFn,
		});
		await api.createResearchRun({ profile: 'legal', question: 'q', store_ids: [STORE] });
		await api.researchRuns({ status: 'running', before: 'a|b' });
		await api.researchRuns({ matter_id: MATTER_ID });
		await api.researchRun(RUN_ID);
		await api.researchRun(RUN_ID, { stepsFull: true });
		await api.cancelResearchRun(RUN_ID);
		const calls = fetchFn.mock.calls.map((c) => {
			const [url, init] = c as unknown as [string, RequestInit];
			return `${init.method} ${url}${init.body ? ` ${String(init.body)}` : ''}`;
		});
		expect(calls).toEqual([
			`POST http://api/api/jobs/legal_desk/research-runs {"profile":"legal","question":"q","store_ids":["${STORE}"]}`,
			'GET http://api/api/jobs/legal_desk/research-runs?status=running&before=a%7Cb',
			`GET http://api/api/jobs/legal_desk/research-runs?matter_id=${MATTER_ID}`,
			`GET http://api/api/jobs/legal_desk/research-runs/${RUN_ID}`,
			`GET http://api/api/jobs/legal_desk/research-runs/${RUN_ID}?include=steps_full`,
			`POST http://api/api/jobs/legal_desk/research-runs/${RUN_ID}/cancel`,
		]);
	});

	it('an unknown store comes back as 422 unknown_store', async () => {
		const api = createClient({
			baseUrl: 'http://api',
			token: async () => 't',
			fetch: async () =>
				json(422, {
					error: { code: 'unknown_store', message: 'store x is not a knowledge store of this job.' },
				}),
		});
		await expect(
			api.createResearchRun({ profile: 'research', question: 'q', store_ids: [STORE] }),
		).rejects.toMatchObject({ status: 422, code: 'unknown_store' });
	});
});

describe('RunDetail', () => {
	function fakeApi(over: Partial<Api>) {
		app.api = { researchRun: vi.fn(async () => detail()), ...over } as unknown as Api;
		return app.api as unknown as Record<string, ReturnType<typeof vi.fn>>;
	}

	it('shows the legal banner, the report, claims with linked citations and verify badges, and the steps', async () => {
		fakeApi({});
		render(RunDetail, { id: RUN_ID, canWrite: true });
		expect(await screen.findByText(LEGAL_BANNER)).toBeTruthy();
		expect(screen.getByTestId('report').textContent).toContain('# Memo');
		const claims = screen.getByRole('list', { name: 'Claims' });
		const eli = within(claims).getByRole('link', { name: 'DU/2024/1061' });
		expect(eli.getAttribute('href')).toBe(
			'https://isap.sejm.gov.pl/isap.nsf/DocDetails.xsp?id=WDU20240001061',
		);
		expect(within(claims).getByRole('link', { name: '123456' }).getAttribute('href')).toBe(
			'https://www.saos.org.pl/judgments/123456',
		);
		expect(within(claims).getByText('Verified').className).toContain('ok');
		const bad = within(claims).getByText('Quote not in source');
		expect(bad.className).toContain('err');
		expect(bad.getAttribute('title')).toBe('reason: quote not found in step 1');
		// a finished run has no Cancel and is not followed
		expect(screen.queryByRole('button', { name: 'Cancel run' })).toBeNull();
		expect(screen.queryByTestId('following')).toBeNull();
		const steps = screen.getByRole('table', { name: 'Steps' });
		expect(within(steps).getByText('eli_get_act')).toBeTruthy();
		expect(within(steps).getByText('1.2 s')).toBeTruthy();
	});

	it('a research run has no legal banner', async () => {
		fakeApi({ researchRun: vi.fn(async () => detail({ profile: 'research' })) } as Partial<Api>);
		render(RunDetail, { id: RUN_ID });
		await screen.findByTestId('report');
		expect(screen.queryByText(LEGAL_BANNER)).toBeNull();
	});

	it('polls every 3 s while the run is going and stops once it is finished', async () => {
		vi.useFakeTimers({ shouldAdvanceTime: true });
		try {
			const researchRun = vi
				.fn()
				.mockResolvedValueOnce(detail({ status: 'queued', report: null, finished_at: null }))
				.mockResolvedValueOnce(detail({ status: 'running', report: null, finished_at: null }))
				.mockResolvedValueOnce(detail({ status: 'verifying', report: null, finished_at: null }))
				.mockResolvedValue(detail());
			fakeApi({ researchRun } as Partial<Api>);
			const seen: string[] = [];
			render(RunDetail, { id: RUN_ID, canWrite: true, onchange: (r: ResearchRun) => seen.push(r.status) });
			expect(await screen.findByTestId('following')).toBeTruthy();
			expect(screen.getByText('Queued')).toBeTruthy();
			await vi.advanceTimersByTimeAsync(3000);
			await flush();
			expect(screen.getByText('Running')).toBeTruthy();
			await vi.advanceTimersByTimeAsync(3000);
			await flush();
			expect(screen.getByText('Verifying')).toBeTruthy();
			await vi.advanceTimersByTimeAsync(3000);
			await flush();
			expect(screen.getByText('Done')).toBeTruthy();
			expect(screen.queryByTestId('following')).toBeNull();
			expect(researchRun).toHaveBeenCalledTimes(4);
			await vi.advanceTimersByTimeAsync(10_000);
			expect(researchRun).toHaveBeenCalledTimes(4);
			expect(seen).toEqual(['queued', 'running', 'verifying', 'done']);
		} finally {
			vi.useRealTimers();
		}
	});

	it('a 404 while following stops the poll and says the run is gone', async () => {
		vi.useFakeTimers({ shouldAdvanceTime: true });
		try {
			const researchRun = vi
				.fn()
				.mockResolvedValueOnce(detail({ status: 'running', report: null }))
				.mockRejectedValue(new ApiError(404, 'not_found', 'research run'));
			fakeApi({ researchRun } as Partial<Api>);
			render(RunDetail, { id: RUN_ID });
			await screen.findByTestId('following');
			await vi.advanceTimersByTimeAsync(3000);
			await flush();
			expect(screen.getByTestId('poll-error').textContent).toContain('This run no longer exists.');
			await vi.advanceTimersByTimeAsync(30_000);
			expect(researchRun).toHaveBeenCalledTimes(2);
		} finally {
			vi.useRealTimers();
		}
	});

	it('Cancel asks the server and shows what it answered; a finished run says so', async () => {
		const going = detail({ status: 'running', report: null, finished_at: null });
		const cancelResearchRun = vi
			.fn()
			.mockResolvedValueOnce({ ...going.run, cancel_requested: true })
			.mockRejectedValueOnce(new ApiError(409, 'run_finished', 'This run is already done.'));
		const api = fakeApi({ researchRun: vi.fn(async () => going), cancelResearchRun } as Partial<Api>);
		const { unmount } = render(RunDetail, { id: RUN_ID, canWrite: true });
		await fireEvent.click(await screen.findByRole('button', { name: 'Cancel run' }));
		await flush();
		expect(api.cancelResearchRun).toHaveBeenCalledWith(RUN_ID);
		expect(screen.getByText('cancel requested')).toBeTruthy();
		expect((screen.getByRole('button', { name: 'Cancel run' }) as HTMLButtonElement).disabled).toBe(true);
		unmount();
		render(RunDetail, { id: RUN_ID, canWrite: true });
		await fireEvent.click(await screen.findByRole('button', { name: 'Cancel run' }));
		await flush();
		expect(screen.getByText('This run has already finished.')).toBeTruthy();
	});

	it('Cancel is disabled without a write role', async () => {
		fakeApi({ researchRun: vi.fn(async () => detail({ status: 'queued', report: null })) } as Partial<Api>);
		render(RunDetail, { id: RUN_ID, canWrite: false });
		expect(((await screen.findByRole('button', { name: 'Cancel run' })) as HTMLButtonElement).disabled).toBe(
			true,
		);
	});
});

describe('/admin/jobs/[job]/research', () => {
	const summary = (over: Partial<ResearchRunSummary> = {}): ResearchRunSummary => ({
		id: RUN_ID,
		profile: 'legal',
		matter_id: MATTER_ID,
		question: 'Czy najemca może wypowiedzieć umowę?\nDruga linia',
		status: 'done',
		steps_used: 3,
		tokens_used: 12345,
		created_at: '2026-10-23T09:00:00Z',
		finished_at: '2026-10-23T09:03:00Z',
		...over,
	});
	const storeItem = (over: Partial<KnowledgeStoreItem>): KnowledgeStoreItem =>
		({
			key: 'returns',
			label: 'Returns policy',
			parent: null,
			kind: 'store',
			areas: [],
			description: null,
			strata_store_id: STORE,
			...over,
		}) as KnowledgeStoreItem;

	function fakeApi(over: Partial<Api> = {}) {
		const api = {
			knowledgeStores: vi.fn(async () => ({
				items: [
					storeItem({}),
					storeItem({ key: 'other', label: 'Other client', strata_store_id: OTHER_STORE }),
					storeItem({ key: 'unbound', label: 'Not indexed', strata_store_id: null }),
				],
				next: null,
			})),
			researchRuns: vi.fn(async () => ({ items: [summary()], next: null })),
			researchRun: vi.fn(async () => detail()),
			legalMatters: vi.fn(async () => ({
				items: [
					{
						id: MATTER_ID,
						name: 'Najem',
						reference: 'I C 1/26',
						created_by: 'u1',
						created_at: '2026-10-23T08:00:00Z',
						closed_at: null,
						my_role: 'lead',
						member_count: 1,
						store_count: 1,
					},
				],
				next: null,
			})),
			legalMatter: vi.fn(async () => ({
				matter: {},
				members: [],
				stores: [{ store_id: STORE, added_by: 'u1', added_at: '2026-10-23T08:00:00Z' }],
			})),
			createResearchRun: vi.fn(async () => run({ id: 'new-run', status: 'queued', report: null })),
			...over,
		};
		app.api = api as unknown as Api;
		return api;
	}

	it('lists runs and opens one in the drawer', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user'));
		const api = fakeApi();
		render(ResearchPage);
		const table = await screen.findByRole('table', { name: 'Research runs' });
		expect(within(table).getByText('Legal (PL)')).toBeTruthy();
		expect(within(table).getByText('12,345')).toBeTruthy();
		await fireEvent.click(
			within(table).getByRole('button', { name: 'Czy najemca może wypowiedzieć umowę?' }),
		);
		expect(await screen.findByText(LEGAL_BANNER)).toBeTruthy();
		expect(api.researchRun).toHaveBeenCalledWith(RUN_ID);
	});

	it('the status filter is sent to the server', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user'));
		const api = fakeApi();
		render(ResearchPage);
		await screen.findByRole('table', { name: 'Research runs' });
		await fireEvent.change(screen.getByLabelText('Status filter'), { target: { value: 'failed' } });
		await flush();
		expect(api.researchRuns).toHaveBeenLastCalledWith({ status: 'failed' });
	});

	it('starts a run with the chosen profile, stores and limits; only indexed stores are offered', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user'));
		const api = fakeApi();
		render(ResearchPage);
		await screen.findByRole('table', { name: 'Research runs' });
		const form = screen.getByRole('form', { name: 'New research run' });
		expect(within(form).queryByText('Not indexed')).toBeNull();
		await fireEvent.change(within(form).getByLabelText('Profile'), { target: { value: 'legal' } });
		await fireEvent.input(within(form).getByLabelText('Question'), { target: { value: ' Is it allowed? ' } });
		// before a matter is chosen every indexed store is offered; with one, only its attached stores
		expect(within(form).getByRole('checkbox', { name: /Other client/ })).toBeTruthy();
		await fireEvent.change(within(form).getByLabelText(/^Matter/), { target: { value: MATTER_ID } });
		await flush();
		expect(api.legalMatter).toHaveBeenCalledWith(MATTER_ID);
		expect(within(form).queryByRole('checkbox', { name: /Other client/ })).toBeNull();
		await fireEvent.click(within(form).getByRole('checkbox', { name: /Returns policy/ }));
		await fireEvent.input(within(form).getByLabelText('Max steps'), { target: { value: '30' } });
		await fireEvent.click(within(form).getByRole('button', { name: 'Start run' }));
		await flush();
		expect(api.createResearchRun).toHaveBeenCalledWith({
			profile: 'legal',
			question: 'Is it allowed?',
			store_ids: [STORE],
			matter_id: MATTER_ID,
			budget: { max_steps: 30 },
		});
		// the new run opens in the drawer
		expect(api.researchRun).toHaveBeenCalledWith('new-run');
	});

	it('an empty question is refused in the form; unknown_store is explained', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user'));
		const api = fakeApi({
			createResearchRun: vi.fn(async () => {
				throw new ApiError(422, 'unknown_store', 'store x is not a knowledge store of this job.');
			}),
		} as Partial<Api>);
		render(ResearchPage);
		await screen.findByRole('table', { name: 'Research runs' });
		const form = screen.getByRole('form', { name: 'New research run' });
		await fireEvent.click(within(form).getByRole('button', { name: 'Start run' }));
		expect(within(form).getByText('Ask a question.')).toBeTruthy();
		expect(api.createResearchRun).not.toHaveBeenCalled();
		await fireEvent.input(within(form).getByLabelText(/^Question/), { target: { value: 'q' } });
		await fireEvent.click(within(form).getByRole('button', { name: 'Start run' }));
		await flush();
		expect(within(form).getByText(/no longer a knowledge store of this job.*\(unknown_store\)/)).toBeTruthy();
	});

	it('without a role in the tenant the form is read only', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs(null));
		fakeApi();
		render(ResearchPage);
		await screen.findByRole('table', { name: 'Research runs' });
		expect((screen.getByRole('button', { name: 'Start run' }) as HTMLButtonElement).disabled).toBe(true);
	});
});
