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

<div class="sk-host" data-look="slack">
	<div class="sk">
		<div class="sk-rail" aria-hidden="true">
			<div class="sk-ws">CB</div>
			<div class="sk-rail-ic"><svg viewBox="0 0 24 24"><path d={ICON.home} /></svg></div>
			<div class="sk-rail-ic"><svg viewBox="0 0 24 24"><path d={ICON.chat} /></svg></div>
		</div>

		<aside class="sk-side" aria-label="Channels">
			<header class="sk-ws-head"><span class="sk-ws-name">Care Bus</span></header>
			<div class="sk-side-scroll">
				<div class="sk-sect">Channels</div>
				<div class="sk-ch on" aria-current="true"><span aria-hidden="true">#</span> support</div>
				<div class="sk-sect">Direct messages</div>
				<div class="sk-dm"><span class="sk-pres on" aria-hidden="true"></span>{agentName}</div>
				<div class="sk-dm">
					<span class="sk-pres on" aria-hidden="true"></span>{customerName} <span class="sk-you">you</span>
				</div>
			</div>
		</aside>

		<section class="sk-main" aria-label="Conversation with {agentName}">
			<header class="sk-head">
				<div class="sk-title"><span class="sk-hash" aria-hidden="true">#</span> support</div>
				<div class="sk-topic">Ask the care team anything about your orders</div>
			</header>
			<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
			<div
				class="sk-log"
				role="log"
				aria-live="polite"
				aria-relevant="additions"
				tabindex="0"
				use:stick={lines.length + (typing ? 0.5 : 0)}
			>
				<div class="sk-intro">
					<div class="sk-intro-hash" aria-hidden="true">#</div>
					<h3>This is the start of #support</h3>
					<p>Messages here go to {agentName}, the care team's interaction agent.</p>
				</div>
				{#each lines as m (m.key)}
					{#if m.role === 'system'}
						<div class="sk-sys" data-testid="chat-message" data-role="system">{m.text}</div>
					{:else}
						{@const you = m.role === 'customer'}
						<div
							class="sk-line"
							class:failed={m.status === 'failed' || m.status === 'unknown'}
							data-testid="chat-message"
							data-role={m.role}
							data-status={m.status}
						>
							<div class="sk-ava" class:you aria-hidden="true">
								{initials(you ? customerName : agentName)}
							</div>
							<div class="sk-line-body">
								<div class="sk-line-head">
									<span class="sk-sender">{you ? customerName : agentName}</span>
									{#if m.at !== null}<time class="sk-ts">{formatTime(m.at)}</time>{/if}
									{#if unsent(m.status)}<span class="sk-state"
											>{PENDING_LABEL[m.status as keyof typeof PENDING_LABEL]}</span
										>{/if}
								</div>
								<div class="sk-text">{m.text}</div>
							</div>
						</div>
					{/if}
				{/each}
				{#if typing}
					<div class="sk-line" data-testid="chat-typing">
						<div class="sk-ava" aria-hidden="true">{initials(agentName)}</div>
						<div class="sk-line-body">
							<div class="sk-line-head"><span class="sk-sender">{agentName}</span></div>
							<div class="sk-typing"><i></i><i></i><i></i> is typing…</div>
						</div>
					</div>
				{/if}
			</div>
			<div class="sk-sr" role="status">{typing ? `${agentName} is typing…` : ''}</div>
			{#if notice}<div class="sk-notice">{@render notice()}</div>{/if}
			<form
				class="sk-compose"
				onsubmit={(e) => {
					e.preventDefault();
					onsend();
				}}
			>
				<textarea
					bind:value={text}
					rows="1"
					placeholder="Message #support"
					aria-label="Message"
					onkeydown={(e) => enterSends(e, onsend)}
				></textarea>
				<div class="sk-foot">
					<span class="sk-hint">Enter to send · Shift+Enter for a new line</span>
					<button class="sk-send" type="submit" disabled={locked || !text.trim()} aria-label="Send">
						<svg viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.send} /></svg>
					</button>
				</div>
			</form>
		</section>
	</div>
</div>

<style>
	.sk-host {
		/* Slack-style palette: light, dark (follows the app theme through color-scheme). */
		--sk-bg: light-dark(#ffffff, #1a1d21);
		--sk-fg: light-dark(#1d1c1d, #d1d2d3);
		--sk-rail: light-dark(#3a1140, #121016);
		--sk-side: light-dark(#3f0e40, #19171d);
		--sk-side-fg: light-dark(#cfc3cf, #cfc3cf);
		--sk-side-muted: light-dark(#b39db4, #a99aaa);
		--sk-side-on: light-dark(#1164a3, #1164a3);
		--sk-presence: #2bac76;
		--sk-brand: light-dark(#4a154b, #6a2a6b);
		--sk-you: light-dark(#2eb67d, #1f8a5c);
		--sk-line: light-dark(#e2e2e2, #35373b);
		--sk-muted: light-dark(#616061, #ababad);
		--sk-hover: light-dark(#f8f8f8, #222529);
		--sk-intro: light-dark(#efefef, #222529);
		--sk-compose-line: light-dark(#8d8d8d, #565856);
		--sk-compose-focus: light-dark(#1d1c1d, #d1d2d3);
		--sk-input: light-dark(#ffffff, #222529);
		--sk-send: light-dark(#007a5a, #007a5a);
		--sk-send-hover: light-dark(#148567, #148567);
		--sk-send-off-bg: light-dark(#e8e8e8, #2c2e33);
		--sk-send-off-fg: light-dark(#a0a0a0, #6a6c70);
		--sk-typing: light-dark(#9a9a9a, #7a7a7a);
		--sk-err: light-dark(#b8312f, #f0827f);

		container: sk / inline-size;
		height: 100%;
	}
	.sk {
		display: grid;
		grid-template-columns: 70px 260px minmax(0, 1fr);
		grid-template-rows: minmax(0, 1fr);
		height: 100%;
		background: var(--sk-bg);
		color: var(--sk-fg);
		font-family: Lato, 'Helvetica Neue', system-ui, sans-serif;
		font-size: 15px;
	}
	.sk-rail {
		background: var(--sk-rail);
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		padding: 14px 0;
	}
	.sk-ws {
		width: 40px;
		height: 40px;
		border-radius: 11px;
		background: #fff;
		color: #3a1140;
		display: flex;
		align-items: center;
		justify-content: center;
		font-weight: 800;
		font-size: 16px;
	}
	.sk-rail-ic {
		width: 40px;
		height: 40px;
		border-radius: 11px;
		background: rgba(255, 255, 255, 0.12);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.sk-rail-ic svg {
		width: 22px;
		height: 22px;
		fill: #fff;
	}
	.sk-side {
		background: var(--sk-side);
		color: var(--sk-side-fg);
		display: flex;
		flex-direction: column;
		border-right: 1px solid rgba(0, 0, 0, 0.2);
		min-width: 0;
	}
	.sk-ws-head {
		padding: 16px 16px 14px;
		border-bottom: 1px solid rgba(255, 255, 255, 0.12);
	}
	.sk-ws-name {
		color: #fff;
		font-weight: 800;
		font-size: 17px;
	}
	.sk-side-scroll {
		padding: 12px 8px;
		overflow-y: auto;
	}
	.sk-sect {
		font-size: 13px;
		color: var(--sk-side-muted);
		padding: 12px 12px 4px;
	}
	.sk-ch,
	.sk-dm {
		padding: 5px 14px;
		border-radius: 6px;
		display: flex;
		align-items: center;
		gap: 7px;
		line-height: 1.5;
		min-width: 0;
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.sk-ch.on {
		background: var(--sk-side-on);
		color: #fff;
		font-weight: 700;
	}
	.sk-pres {
		width: 9px;
		height: 9px;
		border-radius: 50%;
		flex-shrink: 0;
		border: 1.5px solid var(--sk-side-muted);
	}
	.sk-pres.on {
		background: var(--sk-presence);
		border-color: var(--sk-presence);
	}
	.sk-you {
		color: var(--sk-side-muted);
		font-size: 13px;
	}
	.sk-main {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
	}
	.sk-head {
		padding: 11px 20px;
		border-bottom: 1px solid var(--sk-line);
	}
	.sk-title {
		font-weight: 900;
		font-size: 18px;
	}
	.sk-hash {
		color: var(--sk-muted);
	}
	.sk-topic {
		font-size: 13px;
		color: var(--sk-muted);
		margin-top: 2px;
	}
	.sk-log {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 16px 0 8px;
		display: flex;
		flex-direction: column;
	}
	.sk-intro {
		padding: 18px 20px 22px;
	}
	.sk-intro-hash {
		width: 44px;
		height: 44px;
		border-radius: 10px;
		background: var(--sk-intro);
		display: flex;
		align-items: center;
		justify-content: center;
		font-size: 26px;
		font-weight: 800;
		margin-bottom: 10px;
	}
	.sk-intro h3 {
		margin: 0 0 4px;
		font-size: 22px;
		font-weight: 800;
	}
	.sk-intro p {
		margin: 0;
		color: var(--sk-muted);
	}
	.sk-sys {
		padding: 4px 20px;
		font-size: 13px;
		color: var(--sk-muted);
		font-style: italic;
	}
	.sk-line {
		display: flex;
		gap: 9px;
		padding: 6px 20px;
	}
	.sk-line:hover {
		background: var(--sk-hover);
	}
	.sk-ava {
		width: 36px;
		height: 36px;
		border-radius: 8px;
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		color: #fff;
		font-weight: 700;
		font-size: 13px;
		background: var(--sk-brand);
	}
	.sk-ava.you {
		background: var(--sk-you);
	}
	.sk-line-body {
		min-width: 0;
	}
	.sk-line-head {
		display: flex;
		align-items: baseline;
		gap: 7px;
		flex-wrap: wrap;
	}
	.sk-sender {
		font-weight: 900;
	}
	.sk-ts,
	.sk-state {
		font-size: 12px;
		color: var(--sk-muted);
	}
	.sk-line.failed .sk-state {
		color: var(--sk-err);
		font-weight: 700;
	}
	.sk-text {
		line-height: 1.46;
		overflow-wrap: anywhere;
		white-space: pre-wrap;
	}
	.sk-line.failed .sk-text {
		color: var(--sk-muted);
	}
	.sk-typing {
		display: flex;
		align-items: center;
		gap: 4px;
		color: var(--sk-muted);
		font-size: 14px;
		font-style: italic;
	}
	.sk-typing i {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--sk-typing);
		animation: sk-pulse 1.3s infinite ease-in-out;
	}
	.sk-typing i:nth-child(2) {
		animation-delay: 0.18s;
	}
	.sk-typing i:nth-child(3) {
		animation-delay: 0.36s;
	}
	@keyframes sk-pulse {
		0%,
		60%,
		100% {
			opacity: 0.4;
		}
		30% {
			opacity: 1;
		}
	}
	.sk-notice {
		padding: 0 20px;
	}
	.sk-compose {
		margin: 4px 20px 20px;
		border: 1px solid var(--sk-compose-line);
		border-radius: 8px;
		overflow: hidden;
		background: var(--sk-input);
	}
	.sk-compose:focus-within {
		border-color: var(--sk-compose-focus);
		box-shadow: 0 1px 3px rgba(0, 0, 0, 0.12);
	}
	.sk-compose textarea {
		width: 100%;
		border: 0;
		outline: none;
		padding: 11px 14px;
		display: block;
		resize: none;
		line-height: 1.4;
		max-height: 140px;
		background: transparent;
		color: var(--sk-fg);
		field-sizing: content;
	}
	.sk-foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
		padding: 6px 10px;
	}
	.sk-hint {
		font-size: 12px;
		color: var(--sk-muted);
		min-width: 0;
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.sk-send {
		border: 0;
		cursor: pointer;
		width: 32px;
		height: 28px;
		border-radius: 6px;
		background: var(--sk-send);
		color: #fff;
		display: flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
	}
	.sk-send svg {
		width: 16px;
		height: 16px;
		fill: currentColor;
	}
	.sk-send:hover:not(:disabled) {
		background: var(--sk-send-hover);
	}
	.sk-send:disabled {
		background: var(--sk-send-off-bg);
		color: var(--sk-send-off-fg);
		cursor: default;
	}
	.sk-sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
	@container sk (max-width: 720px) {
		.sk {
			grid-template-columns: 56px minmax(0, 1fr);
		}
		.sk-side {
			display: none;
		}
		.sk-head {
			padding: 10px 12px;
		}
		.sk-line,
		.sk-intro,
		.sk-sys,
		.sk-notice {
			padding-inline: 12px;
		}
		.sk-compose {
			margin: 4px 12px 12px;
		}
	}
	@container sk (max-width: 380px) {
		.sk {
			grid-template-columns: minmax(0, 1fr);
		}
		.sk-rail {
			display: none;
		}
	}
</style>
