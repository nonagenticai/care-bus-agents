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
	const last = $derived(lines.filter((l) => l.role !== 'system').at(-1));
	/** Read ticks: one grey tick while sending, two grey once stored, two blue once the agent has answered. */
	const TICK: Record<LineStatus, { icon: string; label: string }> = {
		read: { icon: ICON.checks, label: 'Read' },
		sent: { icon: ICON.checks, label: 'Delivered' },
		sending: { icon: ICON.check, label: 'Sending' },
		queued: { icon: ICON.clock, label: 'Queued' },
		failed: { icon: ICON.alert, label: 'Not sent' },
		unknown: { icon: ICON.alert, label: 'Outcome unknown' },
	};
</script>

<div class="wa-host" data-look="whatsapp">
	<div class="wa">
		<aside class="wa-list" aria-label="Conversations">
			<header class="wa-list-head">
				<div class="wa-me" aria-hidden="true">{initials(customerName)}</div>
				<svg class="wa-ic" viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.more} /></svg>
			</header>
			<div class="wa-search" aria-hidden="true">
				<svg viewBox="0 0 24 24"><path d={ICON.search} /></svg><span>Search or start a new chat</span>
			</div>
			<div class="wa-row on" aria-current="true">
				<div class="wa-ava" aria-hidden="true">{initials(agentName)}</div>
				<div class="wa-row-body">
					<div class="wa-row-top">
						<span class="wa-row-name">{agentName}</span><span class="wa-row-time"
							>{formatTime(last?.at ?? null)}</span
						>
					</div>
					<div class="wa-row-prev">{typing ? 'typing…' : (last?.text ?? 'Start the conversation')}</div>
				</div>
			</div>
		</aside>

		<section class="wa-conv" aria-label="Conversation with {agentName}">
			<header class="wa-head">
				<div class="wa-ava sm" aria-hidden="true">{initials(agentName)}</div>
				<div class="wa-head-meta">
					<span class="wa-head-name">{agentName}</span>
					<span class="wa-head-sub">{typing ? 'typing…' : 'online'}</span>
				</div>
				<svg class="wa-ic" viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.search} /></svg>
				<svg class="wa-ic" viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.more} /></svg>
			</header>
			<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
			<div
				class="wa-log"
				role="log"
				aria-live="polite"
				aria-relevant="additions"
				tabindex="0"
				use:stick={lines.length + (typing ? 0.5 : 0)}
			>
				<div class="wa-banner">
					<svg viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.lock} /></svg>
					Replies come from the care team's agents. Your messages are kept with your case.
				</div>
				{#each lines as m (m.key)}
					{#if m.role === 'system'}
						<div class="wa-sys" data-testid="chat-message" data-role="system">{m.text}</div>
					{:else}
						{@const you = m.role === 'customer'}
						<div
							class="wa-line"
							class:you
							class:failed={m.status === 'failed' || m.status === 'unknown'}
							data-testid="chat-message"
							data-role={m.role}
							data-status={m.status}
						>
							<div class="wa-bubble">
								<span class="wa-sr">{you ? customerName : agentName}:</span>
								<span class="wa-txt">{m.text}</span>
								<span class="wa-stamp">
									{#if m.at !== null}<time>{formatTime(m.at)}</time>{:else if m.status !== 'sending'}<span
											>{PENDING_LABEL[m.status as keyof typeof PENDING_LABEL]}</span
										>{/if}
									{#if you}
										<span
											class="wa-ticks"
											data-state={m.status}
											data-testid="chat-ticks"
											role="img"
											aria-label={TICK[m.status].label}
											><svg viewBox="0 0 24 24" aria-hidden="true"><path d={TICK[m.status].icon} /></svg
											></span
										>
									{/if}
								</span>
							</div>
						</div>
					{/if}
				{/each}
				{#if typing}
					<div class="wa-line" data-testid="chat-typing">
						<div class="wa-bubble wa-typing" aria-hidden="true"><i></i><i></i><i></i></div>
					</div>
				{/if}
			</div>
			<div class="wa-sr" role="status">{typing ? `${agentName} is typing…` : ''}</div>
			{#if notice}<div class="wa-notice">{@render notice()}</div>{/if}
			<form
				class="wa-compose"
				onsubmit={(e) => {
					e.preventDefault();
					onsend();
				}}
			>
				<textarea
					bind:value={text}
					rows="1"
					placeholder="Type a message"
					aria-label="Message"
					onkeydown={(e) => enterSends(e, onsend)}
				></textarea>
				<button class="wa-send" type="submit" disabled={locked || !text.trim()} aria-label="Send">
					<svg viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.send} /></svg>
				</button>
			</form>
		</section>
	</div>
</div>

<style>
	.wa-host {
		/* WhatsApp-style palette: light, dark (follows the app theme through color-scheme). */
		--wa-bg: light-dark(#f0f2f5, #111b21);
		--wa-fg: light-dark(#111b21, #e9edef);
		--wa-panel: light-dark(#ffffff, #111b21);
		--wa-bar: light-dark(#f0f2f5, #202c33);
		--wa-line: light-dark(#e9edef, #222d34);
		--wa-icon: light-dark(#54656f, #aebac1);
		--wa-muted: light-dark(#667781, #8696a0);
		--wa-search: light-dark(#f0f2f5, #202c33);
		--wa-row-on: light-dark(#f0f2f5, #2a3942);
		--wa-brand: light-dark(#00a884, #00a884);
		--wa-ava: linear-gradient(135deg, #00a884, #008069);
		--wa-wall: light-dark(#efeae2, #0b141a);
		--wa-wall-dot: light-dark(rgba(0, 0, 0, 0.02), rgba(255, 255, 255, 0.02));
		--wa-banner: light-dark(#ffeecd, #182229);
		--wa-banner-fg: light-dark(#54656f, #ffd279);
		--wa-in: light-dark(#ffffff, #202c33);
		--wa-out: light-dark(#d9fdd3, #005c4b);
		--wa-input: light-dark(#ffffff, #2a3942);
		--wa-tick: light-dark(#667781, #8696a0);
		--wa-tick-read: light-dark(#53bdeb, #53bdeb);
		--wa-typing: light-dark(#9aa6ac, #8696a0);
		--wa-err: light-dark(#b3261e, #f2b8b5);

		container: wa / inline-size;
		height: 100%;
	}
	.wa {
		display: grid;
		grid-template-columns: 340px minmax(0, 1fr);
		grid-template-rows: minmax(0, 1fr);
		height: 100%;
		background: var(--wa-bg);
		color: var(--wa-fg);
		font-family: 'Segoe UI', 'Helvetica Neue', system-ui, sans-serif;
		font-size: 14.5px;
	}
	.wa-ic {
		width: 20px;
		height: 20px;
		fill: var(--wa-icon);
		flex-shrink: 0;
	}
	.wa-list {
		background: var(--wa-panel);
		border-right: 1px solid var(--wa-line);
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.wa-list-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		background: var(--wa-bar);
		padding: 10px 16px;
	}
	.wa-me {
		width: 40px;
		height: 40px;
		border-radius: 50%;
		background: var(--wa-brand);
		color: #fff;
		display: flex;
		align-items: center;
		justify-content: center;
		font-weight: 700;
	}
	.wa-search {
		display: flex;
		align-items: center;
		gap: 10px;
		margin: 8px 12px;
		padding: 7px 12px;
		background: var(--wa-search);
		border-radius: 8px;
		color: var(--wa-muted);
		font-size: 14px;
	}
	.wa-search svg {
		width: 15px;
		height: 15px;
		fill: currentColor;
	}
	.wa-row {
		display: flex;
		gap: 13px;
		align-items: center;
		padding: 11px 16px;
		border-bottom: 1px solid var(--wa-line);
	}
	.wa-row.on {
		background: var(--wa-row-on);
	}
	.wa-ava {
		width: 48px;
		height: 48px;
		border-radius: 50%;
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		color: #fff;
		font-weight: 700;
		font-size: 15px;
		background: var(--wa-ava);
	}
	.wa-ava.sm {
		width: 40px;
		height: 40px;
		font-size: 13px;
	}
	.wa-row-body {
		flex: 1;
		min-width: 0;
	}
	.wa-row-top {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 8px;
	}
	.wa-row-name {
		font-size: 16px;
		font-weight: 500;
	}
	.wa-row-time,
	.wa-row-prev {
		font-size: 12px;
		color: var(--wa-muted);
	}
	.wa-row-prev {
		font-size: 14px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.wa-conv {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
	}
	.wa-head {
		display: flex;
		align-items: center;
		gap: 14px;
		background: var(--wa-bar);
		padding: 9px 18px;
	}
	.wa-head-meta {
		flex: 1;
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.wa-head-name {
		font-size: 16px;
		font-weight: 500;
	}
	.wa-head-sub {
		font-size: 12.5px;
		color: var(--wa-muted);
	}
	.wa-log {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 18px 7% 12px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		background-color: var(--wa-wall);
		background-image: radial-gradient(circle at 20% 30%, var(--wa-wall-dot) 0 8px, transparent 9px),
			radial-gradient(circle at 70% 60%, var(--wa-wall-dot) 0 6px, transparent 7px),
			radial-gradient(circle at 45% 85%, var(--wa-wall-dot) 0 10px, transparent 11px),
			radial-gradient(circle at 90% 20%, var(--wa-wall-dot) 0 7px, transparent 8px);
		background-size: 280px 280px;
	}
	.wa-banner {
		align-self: center;
		display: flex;
		gap: 6px;
		align-items: flex-start;
		background: var(--wa-banner);
		color: var(--wa-banner-fg);
		font-size: 12.5px;
		text-align: center;
		padding: 6px 12px;
		border-radius: 8px;
		margin-bottom: 8px;
		max-width: 90%;
		box-shadow: 0 1px 0.5px rgba(0, 0, 0, 0.13);
	}
	.wa-banner svg {
		width: 12px;
		height: 12px;
		fill: currentColor;
		flex-shrink: 0;
		margin-top: 3px;
	}
	.wa-sys {
		align-self: center;
		background: var(--wa-in);
		color: var(--wa-muted);
		font-size: 12.5px;
		padding: 5px 12px;
		border-radius: 8px;
		margin: 4px 0;
	}
	.wa-line {
		display: flex;
	}
	.wa-line.you {
		justify-content: flex-end;
	}
	.wa-bubble {
		position: relative;
		max-width: 65%;
		padding: 6px 9px 8px 11px;
		border-radius: 8px;
		line-height: 1.32;
		box-shadow: 0 1px 0.5px rgba(0, 0, 0, 0.13);
		overflow-wrap: anywhere;
		background: var(--wa-in);
		border-top-left-radius: 0;
	}
	.wa-line.you .wa-bubble {
		background: var(--wa-out);
		border-top-left-radius: 8px;
		border-top-right-radius: 0;
	}
	.wa-bubble::before {
		content: '';
		position: absolute;
		top: 0;
		width: 8px;
		height: 13px;
		left: -8px;
		background: radial-gradient(circle at top left, transparent 8px, var(--wa-in) 8px) top left / 8px 13px
			no-repeat;
	}
	.wa-line.you .wa-bubble::before {
		left: auto;
		right: -8px;
		background: radial-gradient(circle at top right, transparent 8px, var(--wa-out) 8px) top right / 8px 13px
			no-repeat;
	}
	.wa-txt {
		white-space: pre-wrap;
	}
	.wa-stamp {
		float: right;
		margin: 6px 0 -3px 10px;
		font-size: 11px;
		color: var(--wa-muted);
		display: inline-flex;
		align-items: center;
		gap: 3px;
	}
	.wa-line.failed .wa-stamp {
		color: var(--wa-err);
	}
	.wa-ticks {
		display: inline-flex;
		color: var(--wa-tick);
	}
	.wa-ticks svg {
		width: 16px;
		height: 16px;
		fill: currentColor;
	}
	.wa-ticks[data-state='read'] {
		color: var(--wa-tick-read);
	}
	.wa-ticks[data-state='failed'],
	.wa-ticks[data-state='unknown'] {
		color: var(--wa-err);
	}
	.wa-typing {
		display: flex;
		gap: 4px;
		padding: 11px 14px;
	}
	.wa-typing i {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--wa-typing);
		animation: wa-blink 1.3s infinite ease-in-out;
	}
	.wa-typing i:nth-child(2) {
		animation-delay: 0.18s;
	}
	.wa-typing i:nth-child(3) {
		animation-delay: 0.36s;
	}
	@keyframes wa-blink {
		0%,
		60%,
		100% {
			opacity: 0.4;
		}
		30% {
			opacity: 1;
		}
	}
	.wa-notice {
		padding: 8px 16px 0;
		background: var(--wa-bar);
	}
	.wa-compose {
		display: flex;
		align-items: flex-end;
		gap: 10px;
		background: var(--wa-bar);
		padding: 8px 16px;
	}
	.wa-compose textarea {
		flex: 1;
		min-width: 0;
		border: 0;
		outline: none;
		background: var(--wa-input);
		border-radius: 8px;
		padding: 11px 14px;
		font-size: 15px;
		color: var(--wa-fg);
		resize: none;
		line-height: 1.4;
		max-height: 140px;
		field-sizing: content;
	}
	.wa-compose textarea:focus-visible {
		outline: 2px solid var(--wa-brand);
		outline-offset: 0;
	}
	.wa-send {
		width: 42px;
		height: 42px;
		border-radius: 50%;
		border: 0;
		cursor: pointer;
		background: var(--wa-brand);
		color: #fff;
		display: flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
	}
	.wa-send svg {
		width: 20px;
		height: 20px;
		fill: currentColor;
	}
	.wa-send:disabled {
		background: none;
		color: var(--wa-icon);
		opacity: 0.6;
		cursor: default;
	}
	.wa-sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
	@container wa (max-width: 720px) {
		.wa {
			grid-template-columns: minmax(0, 1fr);
		}
		.wa-list {
			display: none;
		}
		.wa-head {
			padding: 9px 12px;
			gap: 10px;
		}
		.wa-log {
			padding-inline: 12px;
		}
		.wa-bubble {
			max-width: 85%;
		}
		.wa-compose,
		.wa-notice {
			padding-inline: 10px;
		}
	}
</style>
