import type { Snippet } from 'svelte';
import type { ChatLine } from '../looks';

/** What every look draws; the page owns the conversation, a shell only presents it. */
export interface ShellProps {
	lines: ChatLine[];
	/** A turn is in flight: show the agent typing. */
	typing: boolean;
	agentName: string;
	customerName: string;
	/** The composer's text (bindable). */
	text: string;
	/** New messages are paused (a send's outcome is unknown); the composer stays usable for typing. */
	locked: boolean;
	onsend: () => void;
	/** Send-failure and conversation notices, drawn above the composer. */
	notice?: Snippet;
}

/** Enter sends, Shift+Enter starts a new line; IME composition never sends. */
export function enterSends(e: KeyboardEvent, send: () => void) {
	if (e.key !== 'Enter' || e.shiftKey || e.isComposing) return;
	e.preventDefault();
	send();
}

/**
 * Keeps a transcript pinned to its newest line while the reader is at the bottom; a reader who scrolled up is
 * left where they are. `dep` changes whenever a line is added or the typing indicator toggles.
 */
export function stick(node: HTMLElement, _dep: unknown) {
	let pinned = true;
	const onScroll = () => {
		pinned = node.scrollHeight - node.scrollTop - node.clientHeight < 48;
	};
	node.addEventListener('scroll', onScroll);
	node.scrollTop = node.scrollHeight;
	return {
		update() {
			if (pinned) queueMicrotask(() => (node.scrollTop = node.scrollHeight));
		},
		destroy() {
			node.removeEventListener('scroll', onScroll);
		},
	};
}
