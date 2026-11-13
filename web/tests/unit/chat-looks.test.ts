// web/tests/unit/chat-looks.test.ts
import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import LookSwitcher from '../../src/lib/chat/LookSwitcher.svelte';
import {
	DEFAULT_LOOK,
	LOOK_KEY,
	LOOKS,
	chatLines,
	formatTime,
	initials,
	loadLook,
	saveLook,
} from '../../src/lib/chat/looks';
import type { ChatEvent } from '../../src/lib/api/types';

const throwing = {
	getItem: () => {
		throw new Error('SecurityError');
	},
	setItem: () => {
		throw new Error('QuotaExceededError');
	},
};

beforeEach(() => localStorage.clear());
afterEach(cleanup);

describe('look storage', () => {
	it('defaults when nothing or junk is stored, and round-trips a saved look', () => {
		expect(loadLook()).toBe(DEFAULT_LOOK);
		localStorage.setItem(LOOK_KEY, 'msn');
		expect(loadLook()).toBe(DEFAULT_LOOK);
		saveLook('whatsapp');
		expect(localStorage.getItem(LOOK_KEY)).toBe('whatsapp');
		expect(loadLook()).toBe('whatsapp');
	});
	it('survives storage that throws on read and write', () => {
		expect(loadLook(throwing)).toBe(DEFAULT_LOOK);
		expect(() => saveLook('slack', throwing)).not.toThrow();
		expect(loadLook(null)).toBe(DEFAULT_LOOK);
	});
	it('names the looks generically', () => {
		expect(LOOKS.map(([, l]) => l)).toEqual(['Teams-style', 'Slack-style', 'WhatsApp-style']);
	});
});

describe('LookSwitcher', () => {
	it('marks the current look and persists a new pick', async () => {
		render(LookSwitcher, { look: 'teams' });
		const slack = screen.getByRole('button', { name: 'Slack-style' });
		expect(screen.getByRole('button', { name: 'Teams-style' }).getAttribute('aria-pressed')).toBe('true');
		expect(slack.getAttribute('aria-pressed')).toBe('false');
		await fireEvent.click(slack);
		expect(slack.getAttribute('aria-pressed')).toBe('true');
		expect(localStorage.getItem(LOOK_KEY)).toBe('slack');
		expect(loadLook()).toBe('slack');
	});
});

describe('chat helpers', () => {
	it('initials', () => {
		expect(initials('Anna de Vries')).toBe('AV');
		expect(initials('agent')).toBe('AG');
		expect(initials('  ')).toBe('?');
	});
	it('formatTime is empty for an unsent message', () => {
		expect(formatTime(null)).toBe('');
		expect(formatTime(Date.UTC(2026, 9, 18, 14, 37), 'en-US', 'UTC')).toBe('2:37 PM');
	});
	it('chatLines: stored customer messages are read once the agent answers; pending ones follow with their status', () => {
		const m = (role: ChatEvent['role'], text: string, at_ms: number): ChatEvent => ({
			conversation_id: 'c',
			role,
			text,
			at_ms,
		});
		const lines = chatLines(
			[m('customer', 'hi', 1), m('agent', 'hello', 2), m('customer', 'order?', 3)],
			[{ id: 1, text: 'urgent', status: 'queued', batch: null, after: 2 }],
		);
		expect(lines.map((l) => [l.role, l.status, l.at])).toEqual([
			['customer', 'read', 1],
			['agent', 'sent', 2],
			['customer', 'sent', 3],
			['customer', 'queued', null],
		]);
	});
});
