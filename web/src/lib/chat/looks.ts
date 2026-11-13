import type { ChatEvent } from '$lib/api/types';
import type { Pending, PendingStatus } from '$lib/console/outbox';

/** How the customer chat page is drawn. Presentation only: every look talks to the same conversation. */
export type Look = 'teams' | 'slack' | 'whatsapp';
/** Generic names: the looks echo familiar chat apps without using their names or marks as product names. */
export const LOOKS: [Look, string][] = [
	['teams', 'Teams-style'],
	['slack', 'Slack-style'],
	['whatsapp', 'WhatsApp-style'],
];
export const LOOK_KEY = 'carebus.chat.look';
export const DEFAULT_LOOK: Look = 'teams';

type Store = Pick<Storage, 'getItem' | 'setItem'>;
const local = (): Store | null => {
	try {
		return typeof localStorage === 'undefined' ? null : localStorage;
	} catch {
		return null;
	}
};
const isLook = (v: unknown): v is Look => LOOKS.some(([id]) => id === v);

/** The viewer's look, kept per browser; the default when storage is blocked, empty or holds junk. */
export function loadLook(s: Store | null = local()): Look {
	try {
		const v = s?.getItem(LOOK_KEY);
		return isLook(v) ? v : DEFAULT_LOOK;
	} catch {
		return DEFAULT_LOOK;
	}
}
export function saveLook(look: Look, s: Store | null = local()) {
	try {
		s?.setItem(LOOK_KEY, look);
	} catch {
		/* storage blocked or full: the choice lasts for this page only */
	}
}

/** "Anna de Vries" -> "AV", "agent" -> "AG", "" -> "?" */
export function initials(name: string): string {
	const parts = name
		.trim()
		.split(/[\s._-]+/)
		.filter(Boolean);
	if (parts.length === 0) return '?';
	if (parts.length === 1) return parts[0]!.slice(0, 2).toUpperCase();
	return (parts[0]![0]! + parts.at(-1)![0]!).toUpperCase();
}

/** A message's clock time ("2:37 PM" in en-US, "14:37" in a 24-hour locale); '' for a message not yet stored. */
export function formatTime(ms: number | null, locale?: string, timeZone?: string): string {
	if (ms === null || !Number.isFinite(ms)) return '';
	return new Intl.DateTimeFormat(locale, { hour: 'numeric', minute: '2-digit', timeZone }).format(ms);
}

/**
 * A stored customer message is `sent`, and `read` once an agent message follows it; an unsent one carries its
 * outbox status. Agent and system lines are `sent`.
 */
export type LineStatus = 'sent' | 'read' | PendingStatus;
export interface ChatLine {
	key: string;
	role: ChatEvent['role'];
	text: string;
	/** Server time; null while the message is still in the outbox. */
	at: number | null;
	status: LineStatus;
}

/** The server log followed by the outbox's pending bubbles, in the order the customer sees them. */
export function chatLines(chat: ChatEvent[], pending: Pending[]): ChatLine[] {
	let lastAgent = -1;
	chat.forEach((m, i) => {
		if (m.role === 'agent') lastAgent = i;
	});
	const stored: ChatLine[] = chat.map((m, i) => ({
		key: `${m.conversation_id}|${m.role}|${m.at_ms}`,
		role: m.role,
		text: m.text,
		at: m.at_ms,
		status: m.role === 'customer' && i < lastAgent ? 'read' : 'sent',
	}));
	return [
		...stored,
		...pending.map((p) => ({
			key: `pending|${p.id}`,
			role: 'customer' as const,
			text: p.text,
			at: null,
			status: p.status,
		})),
	];
}

/** Short words for an unsent message's state, shown beside it in every look. */
export const PENDING_LABEL: Record<PendingStatus, string> = {
	queued: 'queued',
	sending: 'sending',
	unknown: 'outcome unknown',
	failed: 'not sent',
};
