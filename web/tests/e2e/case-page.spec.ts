import { expect, test } from '@playwright/test';
import { E2E_JOB, mockApi } from './mock-api';

// The case is a page of its own (cases/[id]), not a drawer: the answer the agent gave is read first, the bus
// trace takes the height that is left, and the raw case hash stays closed until it is asked for.
//
// The FAQ case is built here rather than read from a fixture because what it asserts is the SHAPE of a real one:
// `hash.resolution` is a JSON string inside the hash (carebus-api returns every field as a string), and the answer
// lives in `facts.faq` with the ids it stands on.
const FAQ = {
	answer:
		'Care Bus is George’s customer-care project: agents that run in your own channels [1][2]. ' +
		'It is open source with a public repository [2].\n\nI don’t have a language version in the sources.',
	citations: ['a', 'b'],
	sources: [
		{ chunk_id: 'a', document_id: 'a', store_path: ['faq'] },
		{ chunk_id: 'b', document_id: 'b', store_path: ['faq'] },
	],
	timings_ms: { total: 1 },
};

const event = (stream: string, topic: string, from: string, id: string, payload: unknown) => ({
	stream,
	entry_id: id,
	envelope: {
		id,
		topic,
		from,
		correlation_id: 'CASE-1',
		published_at_ms: 1792065600000,
		tenant_id: 'aaaaaaaa-0000-4000-8000-000000000099',
		job: E2E_JOB,
		payload,
	},
	consumer_group: 'faq',
	acked: true,
});

const detail = {
	case: {
		case_id: 'CASE-1',
		conversation_id: 'conv-1',
		customer_id: 'C-10482',
		request_type: 'faq',
		area: 'faq',
		context_space: 'website:faq',
		outcome: 'completed',
		reason: null,
		work_items: 2,
		retries: 0,
		started_at: '2026-11-27T20:20:34.000000Z',
		resolved_at: '2026-11-27T20:20:53.000000Z',
		duration_ms: 19000,
		procedure: 'website_faq@1.0.1',
	},
	hash: {
		resolution: JSON.stringify({
			case_id: 'CASE-1',
			request_type: 'faq',
			outcome: 'completed',
			reason: null,
			facts: { faq: FAQ },
		}),
		phase: '6',
	},
	trace: [
		event('bus:customer:faq', 'faq.general.request.qualified', 'interaction', '1792065600000-0', {
			request_type: 'faq',
		}),
		event('bus:worker:faq', 'faq.general.work.requested', 'orchestrator', '1792065600100-0', {
			worker: 'faq',
		}),
		event('bus:tool:faq', 'faq.general.tool.ask', 'worker:faq', '1792065600200-0', {
			question: 'what is care bus?',
		}),
		event('bus:worker:faq', 'faq.general.request.resolved', 'orchestrator', '1792065600300-0', {
			outcome: 'completed',
		}),
		event('bus:customer:faq', 'faq.general.answer.ready', 'interaction', '1792065600400-0', { cited: 1 }),
	],
};

test('the answer reads first, the trace fills the page, and the hash stays shut', async ({
	page,
}, testInfo) => {
	await mockApi(page, { override: { '/api/cases/CASE-1': detail } });
	await page.goto(`/admin/jobs/${E2E_JOB}/cases/CASE-1`);

	// The answer was reachable only by scrolling a 54k-character blob; it is the first thing on the page now.
	await expect(page.locator('p.answer')).toContainText('Care Bus is George’s customer-care project');
	await expect(page.locator('p.answer')).toContainText('I don’t have a language version in the sources.');
	await expect(page.getByText(/cited \d+ source/)).toBeVisible();
	// The markers the answer was grounded on are not repeated in the prose: the count says it once.
	await expect(page.locator('p.answer')).not.toContainText('[1]');
	await expect(page.locator('p.answer')).not.toContainText('[2]');

	// Every event has its own row, and the empty-lane cells do not push the event out of its column.
	await expect(page.locator('.seq-row')).toHaveCount(5);
	await expect(page.locator('.seq-row').first()).toContainText('request.qualified');

	// The raw hash is collapsed: present for the audit, not in the way. Its text is in the DOM (a closed
	// <details> keeps it) but never painted.
	const hashBand = page.locator('details.band.raw');
	await expect(hashBand).not.toHaveAttribute('open', '');
	await expect(hashBand.locator('pre.code')).toBeHidden();

	// Selecting a row opens its payload beside the trace.
	await page.locator('.seq-row').nth(2).click();
	const detailPane = page.getByRole('complementary', { name: 'Event detail' });
	await expect(detailPane.getByRole('tab', { name: 'Payload' })).toBeVisible();
	await detailPane.getByRole('tab', { name: 'Payload' }).click();
	await expect(detailPane.getByText('what is care bus?', { exact: false })).toBeVisible();

	// Focus gives the trace the page: the bands go away, one click brings them back.
	await page.getByRole('button', { name: 'Focus trace' }).click();
	await expect(hashBand).toBeHidden();
	await expect(page.getByRole('button', { name: 'Show summary' })).toBeVisible();

	await page.getByRole('button', { name: 'Show summary' }).click();
	await page.screenshot({ path: testInfo.outputPath('case-page.png'), fullPage: true });
	// A phone must not scroll sideways on it.
	expect(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)).toBe(false);
});
