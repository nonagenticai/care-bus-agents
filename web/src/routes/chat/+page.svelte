<script lang="ts">
	import { onDestroy } from 'svelte';
	import { page } from '$app/state';
	import { setQuery } from '$lib/query';
	import { api } from '$lib/app.svelte';
	import { getToken } from '$lib/auth/keycloak';
	import type { FeedStatus } from '$lib/api/sse';
	import { ChatSession } from '$lib/chat/session.svelte';
	import { chatLines, loadLook, LOOKS, type Look } from '$lib/chat/looks';
	import LookSwitcher from '$lib/chat/LookSwitcher.svelte';
	import SendNotices from '$lib/chat/SendNotices.svelte';
	import TeamsShell from '$lib/chat/shells/TeamsShell.svelte';
	import SlackShell from '$lib/chat/shells/SlackShell.svelte';
	import WhatsAppShell from '$lib/chat/shells/WhatsAppShell.svelte';

	const AGENT = 'Care agent';
	const SHELLS = { teams: TeamsShell, slack: SlackShell, whatsapp: WhatsAppShell };
	const FEED: Record<FeedStatus, string> = {
		connecting: 'connecting…',
		open: 'connected',
		retrying: 'reconnecting…',
		stopped: 'not connected',
	};

	let look = $state<Look>(loadLook());
	let text = $state('');

	function urlConversation(): string | null {
		try {
			return page.url.searchParams.get('conversation') || null;
		} catch {
			return null;
		}
	}
	/** Keeps `?conversation=` in step with the open conversation, so the link resumes it. */
	function writeUrl(id: string | null) {
		if (urlConversation() === id) return;
		try {
			void setQuery({ conversation: id ?? '' }, { resetPage: false }).catch(() => {});
		} catch {
			/* router not ready: sessionStorage still carries the id */
		}
	}

	// The same conversation API as the console (job-scoped through client.ts), without the bus traces.
	const chat = new ChatSession({
		api,
		token: getToken,
		storageKey: 'carebus.chat.conversation',
		readUrl: urlConversation,
		writeUrl,
		trace: false,
	});
	const lines = $derived(chatLines(chat.store.chat, chat.outbox.items));
	const Shell = $derived(SHELLS[look]);
	const lookName = $derived(LOOKS.find(([id]) => id === look)?.[1] ?? '');

	function send() {
		if (chat.send(text)) text = '';
	}

	onDestroy(() => chat.destroy());
	void chat.start();
</script>

<div class="chat-page">
	<div class="chat-bar">
		<div class="chat-title">
			<h1>Customer chat</h1>
			<p class="muted">
				Chat as the demo customer{chat.customer?.name ? ` (${chat.customer.name})` : ''} in a {lookName} look. Live
				feed: <span class="conn" data-s={chat.feedStatus} role="status">{FEED[chat.feedStatus]}</span>
			</p>
		</div>
		<div class="chat-actions">
			<LookSwitcher bind:look />
			<button class="btn sm" type="button" onclick={() => chat.reset()}>New conversation</button>
		</div>
	</div>
	<div class="chat-stage">
		<Shell
			{lines}
			typing={chat.inFlight}
			agentName={AGENT}
			customerName={chat.customer?.name ?? 'You'}
			bind:text
			locked={chat.unknown}
			onsend={send}
		>
			{#snippet notice()}
				<SendNotices {chat} />
			{/snippet}
		</Shell>
	</div>
</div>

<style>
	.chat-page {
		display: grid;
		gap: 14px;
		min-width: 0;
	}
	.chat-bar {
		display: flex;
		flex-wrap: wrap;
		align-items: flex-end;
		justify-content: space-between;
		gap: 10px 16px;
	}
	.chat-title h1 {
		margin: 0;
		font-size: 20px;
	}
	.chat-title p {
		margin: 2px 0 0;
		font-size: 13px;
	}
	.chat-actions {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
	}
	.conn[data-s='open'] {
		color: var(--ok);
	}
	.conn[data-s='retrying'] {
		color: var(--warn);
	}
	.chat-stage {
		height: max(460px, calc(100dvh - 230px));
		border: 1px solid var(--line);
		border-radius: 10px;
		overflow: hidden;
		min-width: 0;
	}
</style>
