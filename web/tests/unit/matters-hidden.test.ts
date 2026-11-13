// Legal matters are hidden by default (src/lib/features.ts, owner 2026-10-25): the matters page redirects to the job
// dashboard, the research page shows no matter anywhere, and research stays visible.
import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Api } from '../../src/lib/api/client';
import { createProblem } from '../../src/lib/api/research';
import { LLM_USES } from '../../src/lib/api/providers';
import { app } from '../../src/lib/app.svelte';
import { session } from '../../src/lib/auth/session.svelte';
import { MATTERS_ENABLED, RESEARCH_ENABLED, SHOWN_LLM_USES } from '../../src/lib/features';
import { meAs } from './me';
import ResearchPage from '../../src/routes/admin/jobs/[job]/research/+page.svelte';
import { load as researchLoad } from '../../src/routes/admin/jobs/[job]/research/+page';
import { load as mattersLoad } from '../../src/routes/admin/jobs/[job]/matters/+page';

afterEach(() => {
	cleanup();
	app.api = null;
});

describe('legal matters hidden, research visible', () => {
	it('flags: research on, matters off, every LLM use offered', () => {
		expect(RESEARCH_ENABLED).toBe(true);
		expect(MATTERS_ENABLED).toBe(false);
		expect(SHOWN_LLM_USES).toEqual(LLM_USES);
	});

	it('the research page loads; the matters page redirects to the job dashboard', () => {
		expect(() => researchLoad({ params: { job: 'customer_care_agent' } })).not.toThrow();
		let thrown: unknown;
		try {
			mattersLoad({ params: { job: 'customer_care_agent' } });
		} catch (e) {
			thrown = e;
		}
		expect(thrown).toMatchObject({ status: 307, location: '/admin/jobs/customer_care_agent' });
	});

	it('the research page shows no matter, asks for none, and does not read matters', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user'));
		const created = vi.fn(async () => ({
			id: 'r2',
			profile: 'legal',
			matter_id: null,
			question: 'Czy?',
			status: 'queued',
			steps_used: 0,
			tokens_used: 0,
			created_at: '2026-10-23T09:00:00Z',
			finished_at: null,
			report: null,
		}));
		const api = {
			knowledgeStores: vi.fn(async () => ({ items: [], next: null })),
			researchRuns: vi.fn(async () => ({
				items: [
					{
						id: 'r1',
						profile: 'legal',
						matter_id: null,
						question: 'Czy?',
						status: 'done',
						steps_used: 1,
						tokens_used: 2,
						created_at: '2026-10-23T09:00:00Z',
						finished_at: '2026-10-23T09:03:00Z',
					},
				],
				next: null,
			})),
			legalMatters: vi.fn(),
			createResearchRun: created,
			researchRun: vi.fn(async () => new Promise(() => {})),
		};
		app.api = api as unknown as Api;
		const { container } = render(ResearchPage);
		await screen.findByRole('table', { name: 'Research runs' });
		await fireEvent.change(screen.getByLabelText('Profile'), { target: { value: 'legal' } });
		expect(container.textContent ?? '').not.toMatch(/matter/i);
		expect(container.querySelector('[aria-label="Matter filter"]')).toBeNull();
		expect(api.legalMatters).not.toHaveBeenCalled();
		// a legal run starts without a matter
		await fireEvent.input(screen.getByLabelText('Question'), { target: { value: 'Czy?' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Start run' }));
		expect(created).toHaveBeenCalledWith({ profile: 'legal', question: 'Czy?' });
	});

	it('a walled-store refusal is worded without matters', () => {
		expect(createProblem('store_walled')).not.toMatch(/matter/i);
	});
});
