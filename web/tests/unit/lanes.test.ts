import { describe, expect, it } from 'vitest';
import {
	caseProcedure,
	deadLetter,
	groupHealth,
	streamHealth,
	traceRows,
} from '../../src/routes/admin/jobs/[job]/cases/lanes';
import type { Entry, GroupInfo, StreamInfo } from '../../src/lib/api/types';

// Inline inputs (F7): every value asserted below is written in this file, not read from src/lib/api/fixtures.
const group = (pending: number, lag: number | null): GroupInfo => ({
	name: 'g',
	consumers: 2,
	pending,
	lag,
	last_delivered_id: '0-0',
});

const envelope = (id: string, payload: unknown) => ({
	id,
	topic: 't',
	from: 'f',
	correlation_id: 'C',
	published_at_ms: 0,
	tenant_id: '0195f48b-d000-7000-8000-000000000099',
	job: 'customer_care_agent',
	payload,
});

const deadLetterEntry: Entry = {
	stream: 'bus:dead',
	entry_id: '1792066211044-0',
	envelope: envelope('e3', {
		stream: 'bus:tool:delivery',
		entry_id: '1792065600500-0',
		deliveries: 5,
		reason: 'timeout after 3000 ms',
		original: envelope('e2', { ok: true }),
	}),
};

const stream = (bus: StreamInfo['bus'], length: number, groups: GroupInfo[]) =>
	({ key: `bus:${bus}`, bus, length, groups }) as unknown as StreamInfo;

describe('streamHealth', () => {
	it('a dead stream with entries is an error, an empty one is healthy', () => {
		expect(streamHealth(stream('dead', 3, []))).toEqual({ word: 'err', label: 'Has entries' });
		expect(streamHealth(stream('dead', 0, []))).toEqual({ word: 'ok', label: 'Healthy' });
	});
	it('other buses take the worst consumer group; warn and err keep distinct labels', () => {
		expect(streamHealth(stream('tool', 9, [group(4, 0)]))).toEqual({ word: 'warn', label: 'Falling behind' });
		expect(streamHealth(stream('tool', 9, [group(0, 0), group(14, 37)]))).toEqual({
			word: 'err',
			label: 'Behind',
		});
		expect(streamHealth(stream('worker', 9, [group(0, 0)]))).toEqual({ word: 'ok', label: 'Healthy' });
	});
});

describe('runtime helpers', () => {
	it('groupHealth uses the design thresholds and treats unknown lag as ok', () => {
		expect(groupHealth(group(0, 0))).toBe('ok');
		expect(groupHealth(group(4, 0))).toBe('warn');
		expect(groupHealth(group(14, 37))).toBe('err');
		expect(groupHealth(group(0, null))).toBe('ok');
	});

	it('deadLetter reads the payload bus.rs writes and rejects other entries', () => {
		expect(deadLetter(deadLetterEntry)).toMatchObject({
			stream: 'bus:tool:delivery',
			deliveries: 5,
			reason: 'timeout after 3000 ms',
		});
		expect(deadLetter({ stream: 'bus:dead', entry_id: '1-0', envelope: envelope('x', {}) })).toBeNull();
	});

	it('traceRows orders by entry id and makes time relative', () => {
		const mk = (stream: string, entry_id: string) => ({
			stream,
			entry_id,
			envelope: envelope(entry_id, {}),
			consumer_group: null,
			acked: null,
		});
		const rows = traceRows([
			mk('bus:tool:order', '1792065600300-0'),
			mk('bus:customer:faq', '1792065600000-0'),
			mk('bus:worker:faq:faq', '1792065600100-1'),
		]);
		expect(rows.map((r) => [r.lane, r.t_ms])).toEqual([
			['customer', 0],
			['worker', 100],
			['tool', 300],
		]);
	});
});

describe('caseProcedure', () => {
	const derive = (key: string) => (key === 'retail:order-support:returns' ? 'return' : null);
	const space = 'retail:order-support:returns';
	it('prefers the recorded name@version', () => {
		expect(caseProcedure({ procedure: 'return@1.4.0-beta', context_space: space }, derive)).toEqual({
			text: 'return@1.4.0-beta',
			recorded: true,
		});
	});
	it('falls back to the context space procedure name, marked as not recorded', () => {
		expect(caseProcedure({ procedure: null, context_space: space }, derive)).toEqual({
			text: 'return',
			recorded: false,
		});
	});
	it('is null when neither is known', () => {
		expect(caseProcedure({ procedure: null, context_space: 'x' }, derive)).toBeNull();
	});
});
