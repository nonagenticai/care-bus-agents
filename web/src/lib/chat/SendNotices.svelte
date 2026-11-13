<script lang="ts">
	import type { ChatSession } from './session.svelte';

	/** The console's resend affordances: an unknown outcome, a rejected send, a conversation that did not load. */
	let { chat }: { chat: ChatSession } = $props();
</script>

{#if chat.unknown}
	<div class="cn err" role="alert">
		Send outcome unknown: the connection failed before the server answered, so it may or may not have received
		your message. New messages are paused until you settle this.
		<div class="cn-row">
			<button class="btn sm" type="button" onclick={() => chat.retry()}>Reload conversation</button>
			<button class="btn sm" type="button" onclick={() => chat.resendAnyway()}>Resend anyway</button>
			<button class="btn sm" type="button" onclick={() => chat.discard()}>Discard</button>
		</div>
	</div>
{:else if chat.failedSend}
	<div class="cn err">
		Not sent: {chat.outbox.error ?? 'the server rejected it before the page was reloaded.'}
		<div class="cn-row">
			<button class="btn sm" type="button" onclick={() => chat.retrySend()}>Retry message</button>
			<button class="btn sm" type="button" onclick={() => chat.discard()}>Discard</button>
		</div>
	</div>
{/if}
{#if chat.error}
	<div class="cn err" role="status">
		Conversation unavailable: {chat.error}
		<div class="cn-row">
			<button class="btn sm" type="button" onclick={() => chat.retry()}
				>{chat.conversationId || chat.unreachable ? 'Reload conversation' : 'Start conversation'}</button
			>
		</div>
	</div>
{/if}

<style>
	.cn {
		margin-bottom: 8px;
		padding: 8px 12px;
		border-radius: 8px;
		font: 13px/1.45 var(--sans);
		background: var(--panel);
		color: var(--fg);
		border: 1px solid var(--line);
	}
	.cn.err {
		border-color: color-mix(in srgb, var(--err) 45%, transparent);
		background: color-mix(in srgb, var(--err) 8%, var(--panel));
	}
	.cn-row {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-top: 6px;
	}
</style>
