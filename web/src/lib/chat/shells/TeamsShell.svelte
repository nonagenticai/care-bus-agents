<script lang="ts">
	import { ICON } from '../icons';
	import { PENDING_LABEL, formatTime, initials, type LineStatus } from '../looks';
	import { enterSends, stick, type ShellProps } from './types';

	let {
		lines,
		typing,
		agentName,
		customerName,
		text = $bindable(),
		locked,
		onsend,
		notice,
	}: ShellProps = $props();
	const unsent = (s: LineStatus) => s !== 'sent' && s !== 'read';
</script>

<div class="tm-host" data-look="teams">
	<div class="tm">
		<section class="tm-conv" aria-label="Conversation with {agentName}">
			<header class="tm-head">
				<div class="tm-ava bot lg">{initials(agentName)}</div>
				<div class="tm-head-meta">
					<span class="tm-head-name">{agentName}</span>
					<span class="tm-head-sub">{typing ? 'Typing…' : 'Customer care · available'}</span>
				</div>
			</header>
			<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
			<div
				class="tm-log"
				role="log"
				aria-live="polite"
				aria-relevant="additions"
				tabindex="0"
				use:stick={lines.length + (typing ? 0.5 : 0)}
			>
				{#each lines as m (m.key)}
					{#if m.role === 'system'}
						<div class="tm-sys" data-testid="chat-message" data-role="system">{m.text}</div>
					{:else}
						{@const you = m.role === 'customer'}
						<div
							class="tm-msg"
							class:you
							class:failed={m.status === 'failed' || m.status === 'unknown'}
							data-testid="chat-message"
							data-role={m.role}
							data-status={m.status}
						>
							{#if !you}<div class="tm-ava bot sm" aria-hidden="true">{initials(agentName)}</div>{/if}
							<div class="tm-body">
								<div class="tm-meta">
									<span class="tm-name">{you ? customerName : agentName}</span>
									{#if m.at !== null}<time class="tm-time">{formatTime(m.at)}</time>{/if}
									{#if unsent(m.status)}<span class="tm-state">
											{PENDING_LABEL[m.status as keyof typeof PENDING_LABEL]}
										</span>{/if}
								</div>
								<div class="tm-bubble"><span class="tm-txt">{m.text}</span></div>
							</div>
							{#if you}<div class="tm-ava you sm" aria-hidden="true">{initials(customerName)}</div>{/if}
						</div>
					{/if}
				{/each}
				{#if typing}
					<div class="tm-msg" data-testid="chat-typing">
						<div class="tm-ava bot sm" aria-hidden="true">{initials(agentName)}</div>
						<div class="tm-body">
							<div class="tm-bubble tm-typing" aria-hidden="true"><i></i><i></i><i></i></div>
						</div>
					</div>
				{/if}
			</div>
			<div class="tm-sr" role="status">{typing ? `${agentName} is typing…` : ''}</div>
			{#if notice}<div class="tm-notice">{@render notice()}</div>{/if}
			<form
				class="tm-compose"
				onsubmit={(e) => {
					e.preventDefault();
					onsend();
				}}
			>
				<div class="tm-compose-row">
					<textarea
						bind:value={text}
						rows="1"
						placeholder="Type a message"
						aria-label="Message"
						onkeydown={(e) => enterSends(e, onsend)}></textarea>
					<button class="tm-send" type="submit" disabled={locked || !text.trim()} aria-label="Send">
						<svg viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.send} /></svg>
					</button>
				</div>
			</form>
		</section>
	</div>
</div>

<style>
	.tm-host {
		/* Teams-style palette: light, dark (follows the app theme through color-scheme). */
		--tm-bg: light-dark(#f5f5f5, #1f1f1f);
		--tm-fg: light-dark(#242424, #e0e0e0);
		--tm-brand: light-dark(#5b5fc7, #7f85f5);
		--tm-line: light-dark(#e6e6e6, #2b2b2b);
		--tm-item-on: light-dark(#f0f0fb, #2d2c2c);
		--tm-conv: light-dark(#ffffff, #1a1a1a);
		--tm-sub: light-dark(#616161, #c8c8c8);
		--tm-name: light-dark(#424242, #d0d0d0);
		--tm-time: light-dark(#6e6e6e, #a6a6a6);
		--tm-bot-bubble: light-dark(#f0f0f0, #2d2c2c);
		--tm-you-bubble: light-dark(#e8ebfa, #383a5c);
		--tm-ava-bot: linear-gradient(135deg, #6264a7, #464775);
		--tm-ava-you: linear-gradient(135deg, #ca5010, #a74109);
		--tm-compose-line: light-dark(#d6d6d6, #4a4a4a);
		--tm-typing: light-dark(#9a9a9a, #8a8a8a);
		--tm-send-off: light-dark(#c0c0c0, #5a5a5a);
		--tm-err: light-dark(#a4262c, #f1707b);

		container: tm / inline-size;
		height: 100%;
	}
	.tm {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		grid-template-rows: minmax(0, 1fr);
		height: 100%;
		background: var(--tm-bg);
		color: var(--tm-fg);
		font-family: 'Segoe UI', system-ui, 'Helvetica Neue', sans-serif;
		font-size: 14px;
	}
	.tm-ava {
		width: 32px;
		height: 32px;
		border-radius: 50%;
		display: flex;
		align-items: center;
		justify-content: center;
		font-size: 12px;
		font-weight: 600;
		color: #fff;
		flex-shrink: 0;
	}
	.tm-ava.bot {
		background: var(--tm-ava-bot);
	}
	.tm-ava.you {
		background: var(--tm-ava-you);
	}
	.tm-ava.lg {
		width: 36px;
		height: 36px;
		font-size: 13px;
	}
	.tm-ava.sm {
		width: 28px;
		height: 28px;
		font-size: 11px;
	}
	.tm-conv {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
		background: var(--tm-conv);
	}
	.tm-head {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 12px 20px;
		border-bottom: 1px solid var(--tm-line);
	}
	.tm-head-meta {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.tm-head-name {
		font-weight: 600;
		font-size: 15px;
	}
	.tm-head-sub {
		font-size: 12px;
		color: var(--tm-sub);
	}
	.tm-log {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 18px 20px;
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.tm-sys {
		align-self: center;
		font-size: 12px;
		color: var(--tm-sub);
		text-align: center;
		max-width: 90%;
	}
	.tm-msg {
		display: flex;
		gap: 8px;
		align-items: flex-end;
		max-width: 78%;
		align-self: flex-start;
	}
	.tm-msg.you {
		align-self: flex-end;
	}
	.tm-body {
		min-width: 0;
	}
	.tm-meta {
		display: flex;
		gap: 8px;
		align-items: baseline;
		margin: 0 4px 3px;
		flex-wrap: wrap;
	}
	.tm-msg.you .tm-meta {
		justify-content: flex-end;
	}
	.tm-name {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--tm-name);
	}
	.tm-time,
	.tm-state {
		font-size: 11px;
		color: var(--tm-time);
	}
	.tm-msg.failed .tm-state {
		color: var(--tm-err);
		font-weight: 600;
	}
	.tm-bubble {
		padding: 9px 13px;
		border-radius: 8px;
		line-height: 1.45;
		box-shadow: 0 1px 1px rgba(0, 0, 0, 0.06);
		overflow-wrap: anywhere;
		background: var(--tm-bot-bubble);
		border-top-left-radius: 2px;
	}
	.tm-msg.you .tm-bubble {
		background: var(--tm-you-bubble);
		border-top-left-radius: 8px;
		border-top-right-radius: 2px;
	}
	.tm-msg.failed .tm-bubble {
		box-shadow: inset 0 0 0 1px var(--tm-err);
	}
	.tm-txt {
		white-space: pre-wrap;
	}
	.tm-typing {
		display: flex;
		gap: 4px;
		padding: 12px 14px;
	}
	.tm-typing i {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--tm-typing);
		animation: tm-bounce 1.3s infinite ease-in-out;
	}
	.tm-typing i:nth-child(2) {
		animation-delay: 0.18s;
	}
	.tm-typing i:nth-child(3) {
		animation-delay: 0.36s;
	}
	@keyframes tm-bounce {
		0%,
		60%,
		100% {
			transform: translateY(0);
			opacity: 0.5;
		}
		30% {
			transform: translateY(-5px);
			opacity: 1;
		}
	}
	.tm-notice {
		padding: 0 20px;
	}
	.tm-compose {
		padding: 12px 20px 16px;
	}
	.tm-compose-row {
		display: flex;
		align-items: flex-end;
		gap: 8px;
		border: 1px solid var(--tm-compose-line);
		border-radius: 8px;
		padding: 4px 6px 4px 14px;
		background: var(--tm-conv);
	}
	.tm-compose-row:focus-within {
		border-color: var(--tm-brand);
		box-shadow: 0 0 0 1px var(--tm-brand);
	}
	.tm-compose textarea {
		flex: 1;
		min-width: 0;
		border: 0;
		outline: none;
		background: none;
		padding: 9px 0;
		color: var(--tm-fg);
		resize: none;
		line-height: 1.4;
		max-height: 140px;
		field-sizing: content;
	}
	.tm-send {
		border: 0;
		background: none;
		cursor: pointer;
		width: 34px;
		height: 34px;
		border-radius: 6px;
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--tm-brand);
		flex-shrink: 0;
	}
	.tm-send svg {
		width: 20px;
		height: 20px;
		fill: currentColor;
	}
	.tm-send:hover:not(:disabled) {
		background: var(--tm-item-on);
	}
	.tm-send:disabled {
		color: var(--tm-send-off);
		cursor: default;
	}
	.tm-sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
	@container tm (max-width: 720px) {
		.tm-head,
		.tm-log,
		.tm-compose {
			padding-inline: 12px;
		}
		.tm-notice {
			padding-inline: 12px;
		}
		.tm-msg {
			max-width: 92%;
		}
	}
</style>
