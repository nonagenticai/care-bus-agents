<script lang="ts">
	import './console.css';
	import { onDestroy, tick } from 'svelte';
	import { page } from '$app/state';
	import { setQuery } from '$lib/query';
	import Tabs from '$lib/ui/Tabs.svelte';
	import { api, guard } from '$lib/app.svelte';
	import { getToken } from '$lib/auth/keycloak';
	import { session } from '$lib/auth/session.svelte';
	import type { AgentView } from '$lib/api/types';
	import { Epoch, MAX_PER_LANE, busKey, direction, type Lane } from '$lib/console/console.svelte';
	import {
		activity,
		agentCards,
		clock,
		contextOf,
		elapsed,
		eventOf,
		isBad,
		plain,
		summary,
	} from '$lib/console/bus';
	import { CONVERSATION_KEY } from '$lib/console/outbox';
	import { ChatSession } from '$lib/chat/session.svelte';
	import type { FeedStatus } from '$lib/api/sse';
	import AgentStrip from './AgentStrip.svelte';
	import Inspector from './Inspector.svelte';

	let agentViews = $state<AgentView[] | null>(null);
	let selectedAgent = $state<string | null>('orchestrator');
	let hoverCid = $state<string | null>(null);
	let text = $state('');
	/** Narrow layouts (container < 700px) show one panel at a time; ignored by the CSS on wider layouts. */
	type View = 'chat' | 'activity' | 'details';
	let view = $state<View>('chat');
	const VIEWS: [View, string][] = [
		['chat', 'Chat'],
		['activity', 'Activity'],
		['details', 'Details'],
	];
	/** Ticks the elapsed time in the activity summary. */
	let now = $state(Date.now());
	const nowTimer = setInterval(() => (now = Date.now()), 1000);
	/** Trace filter: one case's events only ('' = all). */
	let caseFilter = $state('');
	/** Per lane: follow the newest event; turned off when the reader scrolls up, on again at the bottom. */
	let follow = $state<Record<Lane, boolean>>({ customer: true, worker: true, tool: true, dead: true });
	/** Per lane: `store.added` when the reader last saw the bottom. */
	let seenAt = $state<Record<Lane, number>>({ customer: 0, worker: 0, tool: 0, dead: 0 });
	let listEls: Partial<Record<Lane, HTMLElement>> = {};
	let agentsTimer: ReturnType<typeof setTimeout> | null = null;
	/** Dropped agent loads after the page is gone. */
	const epoch = new Epoch();

	/** The conversation named in the URL (`?conversation=`), which wins over the one stored for this tab. */
	function urlConversation(): string | null {
		try {
			return page.url.searchParams.get('conversation') || null;
		} catch {
			return null;
		}
	}
	/** Keeps `?conversation=` in step with the open conversation (replaceState: no history entry per start). */
	function writeUrl(id: string | null) {
		if (urlConversation() === id) return;
		try {
			void setQuery({ conversation: id ?? '' }, { resetPage: false }).catch(() => {});
		} catch {
			/* router not ready: sessionStorage still carries the id */
		}
	}

	const chat = new ChatSession({
		api,
		token: getToken,
		storageKey: CONVERSATION_KEY,
		readUrl: urlConversation,
		writeUrl,
		onActivity: agentsSoon,
		onStart: () => void loadAgents(),
	});
	const store = chat.store;
	const CHIPS = [
		'Where is my airfryer?',
		'I want to return my headphones',
		'I think I was charged twice',
		'My drill stopped working',
		'How long does a refund take?',
	];
	const LANES = [
		{
			id: 'customer',
			title: 'customer-bus',
			cls: '--cust',
			flow: 'bus:customer:{area} · only fully qualified requests',
			legend: ['request.qualified → orchestrator', 'request.resolved → interaction'],
			empty:
				'Nothing yet. A request appears here only once the interaction agent has every required field, and its resolution comes back here.',
		},
		{
			id: 'worker',
			title: 'worker-bus',
			cls: '--work',
			flow: 'bus:worker:{area}:{tool} · bus:worker:{area}:results',
			legend: ['work.requested → worker-{tool}', 'work.succeeded / failed → orchestrator'],
			empty: 'Work requests from the orchestrator go down; success or failure comes back up.',
		},
		{
			id: 'tool',
			title: 'tool-bus',
			cls: '--tool',
			flow: 'bus:tool:{tool} · MCP bridge → nonagenticai_mcp_server /mcp',
			legend: ['tool.call → tool-executor', 'tool.response → worker-{tool}'],
			empty: 'MCP tool calls go down to the bridge; tool responses come back up to the worker.',
		},
		{
			id: 'dead',
			title: 'dead-letters',
			cls: '--err',
			flow: 'bus:dead · entries no consumer could process',
			legend: ['entry moved to bus:dead', 'inspect it in Admin · Buses'],
			empty: 'No dead letters.',
		},
	] as const;

	const events = $derived(store.all);
	const holding = $derived(chat.holding);
	const cards = $derived(agentCards(events, agentViews, holding));
	const agentCard = $derived(store.selected ? null : (cards.find((c) => c.id === selectedAgent) ?? null));

	const unknown = $derived(chat.unknown);
	const FEED: Record<FeedStatus, string> = {
		connecting: 'connecting…',
		open: 'connected',
		retrying: 'reconnecting…',
		stopped: 'not connected',
	};
	/** Observed from bus events only: the case in view, its latest step, outcome and elapsed time. */
	const act = $derived(activity(events, caseFilter));
	/** The dead-letter lane is shown only once a dead letter was observed. */
	const lanes = $derived(LANES.filter((l) => l.id !== 'dead' || store.dead.length > 0));
	const shown = (lane: Lane) =>
		caseFilter ? store[lane].filter((e) => e.envelope.correlation_id === caseFilter) : store[lane];
	const fresh = (lane: Lane) => Math.max(0, store.added[lane] - seenAt[lane]);

	function toEnd(lane: Lane) {
		const el = listEls[lane];
		if (el) el.scrollTop = el.scrollHeight;
	}
	function onScroll(lane: Lane) {
		const el = listEls[lane];
		if (!el) return;
		follow[lane] = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
		if (follow[lane]) seenAt[lane] = store.added[lane];
	}
	function jump(lane: Lane) {
		follow[lane] = true;
		seenAt[lane] = store.added[lane];
		toEnd(lane);
	}
	$effect(() => {
		for (const lane of ['customer', 'worker', 'tool', 'dead'] as const) {
			const n = store.added[lane];
			void caseFilter;
			if (follow[lane]) {
				seenAt[lane] = n;
				void tick().then(() => toEnd(lane));
			}
		}
	});

	function agentsSoon() {
		if (agentsTimer) return;
		agentsTimer = setTimeout(() => {
			agentsTimer = null;
			void loadAgents();
		}, 1500);
	}
	async function loadAgents() {
		const g = epoch.now();
		try {
			const r = await api().agents();
			if (!epoch.stale(g)) agentViews = r.items;
		} catch {
			/* the strip falls back to bus-derived status with runtime "unknown" */
		}
	}
	function send(msg: string) {
		if (chat.send(msg)) text = '';
	}

	function reset() {
		caseFilter = '';
		follow = { customer: true, worker: true, tool: true, dead: true };
		seenAt = { customer: 0, worker: 0, tool: 0, dead: 0 };
		chat.reset();
	}

	function selectAgent(id: string) {
		store.selected = null;
		selectedAgent = id;
		view = 'details';
	}

	onDestroy(() => {
		epoch.bump();
		chat.destroy();
		if (agentsTimer) clearTimeout(agentsTimer);
		clearInterval(nowTimer);
	});
	void chat.start();
</script>

<div class="console-host">
	<div class="console app" data-view={view}>
		<div class="ctabs" data-pane="nav">
			<Tabs items={VIEWS} bind:active={view} label="Console view" />
		</div>
		<section class="panel chat" aria-label="Customer chat" data-pane="chat">
			<header>
				<span class="label">Customer care · live</span>
				<h1>Care Bus Console</h1>
				<p class="sub">
					Chat as the demo customer. Every message the agents exchange appears on the buses to the right. Live
					feed: <span class="conn" data-s={chat.feedStatus} role="status">{FEED[chat.feedStatus]}</span>
				</p>
				{#if chat.customer}
					<div class="cust">
						{#if chat.customer.name}<b>{chat.customer.name}</b>{/if}<span class="tag">{chat.customer.id}</span
						>{#if chat.customer.tier}<span class="tag">{chat.customer.tier}</span>{/if}<span
							>demo customer</span
						>
					</div>
				{/if}
			</header>
			<div class="log" aria-live="polite">
				{#each store.chat as m (`${m.conversation_id}|${m.role}|${m.at_ms}`)}
					{#if m.role === 'system'}
						<div class="note">{m.text}</div>
					{:else}
						<div class="bub {m.role === 'customer' ? 'user' : 'agent'}">
							{#if m.role === 'agent'}<span class="bwho">Interaction agent</span>{/if}<span class="txt"
								>{m.text}</span
							>
						</div>
					{/if}
					{#if m.case_id}<div class="note cust">{m.case_id} published to customer-bus</div>{/if}
				{/each}
				{#each chat.outbox.items as p (p.id)}
					<div class="bub user pending" class:failed={p.status === 'failed'}>
						<span class="txt">{p.text}</span>
						<span class="qtag"
							>{p.status === 'queued'
								? 'queued'
								: p.status === 'sending'
									? 'sending'
									: p.status === 'unknown'
										? 'outcome unknown'
										: 'not sent'}</span
						>
					</div>
				{/each}
				{#if holding}
					<div class="note hint">
						The interaction agent is holding this request until it has every required field; nothing is on the
						bus yet.
					</div>
				{/if}
				{#if chat.inFlight}
					<div class="typing" role="status">
						<span class="dots" aria-hidden="true"><i></i><i></i><i></i></span>Interaction agent is replying…
					</div>
				{/if}
				{#if unknown}
					<div class="note err" role="alert">
						Send outcome unknown: the connection failed before the server answered, so it may or may not have
						received your message. New messages are paused until you settle this.
						<div class="row">
							<button class="btn sm" type="button" onclick={() => chat.retry()}>Reload conversation</button>
							<button class="btn sm" type="button" onclick={() => chat.resendAnyway()}>Resend anyway</button>
							<button class="btn sm" type="button" onclick={() => chat.discard()}>Discard</button>
						</div>
					</div>
				{:else if chat.failedSend}
					<div class="note err">
						Not sent: {chat.outbox.error ?? 'the server rejected it before the page was reloaded.'}
						<div class="row">
							<button class="btn sm" type="button" onclick={() => chat.retrySend()}>Retry message</button>
							<button class="btn sm" type="button" onclick={() => chat.discard()}>Discard</button>
						</div>
					</div>
				{/if}
			</div>
			{#if chat.error}
				<div class="note err" role="status">
					Conversation unavailable: {chat.error}
					<div class="row">
						<button class="btn sm" type="button" onclick={() => chat.retry()}
							>{chat.conversationId || chat.unreachable
								? 'Reload conversation'
								: 'Start conversation'}</button
						>
					</div>
				</div>
			{/if}
			<div class="qchips">
				{#each CHIPS as c (c)}
					<button type="button" class="qchip" disabled={unknown} onclick={() => send(c)}>{c}</button>
				{/each}
			</div>
			<form
				class="send"
				onsubmit={(e) => {
					e.preventDefault();
					send(text);
				}}
			>
				<input
					id="msgInput"
					bind:value={text}
					autocomplete="off"
					placeholder="Type as the customer…"
					aria-label="Customer message"
				/>
				<button class="btn primary" type="submit" disabled={unknown}>Send</button>
			</form>
			<div class="controls">
				<button class="btn" type="button" onclick={reset}>New conversation</button>
			</div>
			<details class="demo">
				<summary>Demo controls <span class="tag">demo only</span></summary>
				<p>
					Fault injection for demonstrations: it changes how the demo tools behave for everyone using this
					environment.
				</p>
				<button
					class="btn"
					type="button"
					disabled={!session.canWriteDemo}
					title={session.canWriteDemo ? '' : 'Needs the super_admin role'}
					onclick={() =>
						guard(
							() => api().setFault({ tool: 'delivery_track_shipment', mode: 'timeout', count: 1 }),
							'The next carrier call will time out.',
						)}>Next carrier call times out</button
				>
			</details>
		</section>

		<div class="main">
			<div class="tracebar" data-pane="activity">
				<label
					>Case
					<select bind:value={caseFilter} aria-label="Filter trace by case">
						<option value="">All cases ({store.caseIds.length})</option>
						{#each store.caseIds as c (c)}<option value={c}>{c}</option>{/each}
					</select></label
				>
				<span
					>Agent status is derived from the events seen here and consumer-group metrics, not reported by the
					agents.</span
				>
				{#if store.trimmed > 0}
					<span role="status"
						>Showing the last {MAX_PER_LANE} events per bus; {store.trimmed} older not shown (open the case for
						its full trace).</span
					>
				{/if}
			</div>
			<section class="panel activity" aria-label="Case activity" data-pane="activity">
				<span class="label">Observed from bus events</span>
				{#if act}
					<dl>
						<div>
							<dt>Case</dt>
							<dd class="mono">{act.caseId}{caseFilter ? '' : ' (latest seen)'}</dd>
						</div>
						<div>
							<dt>Last observed step</dt>
							<dd>
								{act.step || 'No events observed for this case yet.'}{#if act.stepAt !== null}
									<span class="time">{clock(act.stepAt)}</span>{/if}
							</dd>
						</div>
						<div>
							<dt>Outcome</dt>
							<dd class:bad={act.bad}>{act.outcome ?? 'No resolution observed yet.'}</dd>
						</div>
						<div>
							<dt>{act.endedAt !== null ? 'Took' : 'Elapsed'}</dt>
							<dd class="mono">
								{act.startedAt === null
									? '—'
									: act.endedAt !== null
										? `${elapsed(act.endedAt - act.startedAt)} (first to resolution event)`
										: `${elapsed(now - act.startedAt)} since the first event`}
							</dd>
						</div>
					</dl>
				{:else}
					<p>No case on the bus yet. Activity appears once a request is fully qualified.</p>
				{/if}
			</section>
			<div class="strip" data-pane="activity">
				<AgentStrip {cards} selected={store.selected ? null : selectedAgent} onselect={selectAgent} />
			</div>
			<div class="buses">
				{#each lanes as lane (lane.id)}
					<section
						class="panel bus"
						class:dead={lane.id === 'dead'}
						style="--c:var({lane.cls})"
						aria-label={lane.title}
						data-pane="activity"
					>
						<header>
							<h2>
								{lane.title}
								<span class="count"
									>{caseFilter ? `${shown(lane.id).length}/` : ''}{store[lane.id].length}</span
								>
							</h2>
							<span class="flow">{lane.flow}</span>
							<span class="legend"
								><span><b>↓</b> {lane.legend[0]}</span><span><b>↑</b> {lane.legend[1]}</span><span
									><span class="ack">pending</span> → <span class="ack done">acked</span></span
								></span
							>
						</header>
						<div class="list" bind:this={listEls[lane.id]} onscroll={() => onScroll(lane.id)}>
							{#if store[lane.id].length === 0}<div class="empty">{lane.empty}</div>
							{:else if caseFilter && shown(lane.id).length === 0}<div class="empty">
									No events for case {caseFilter} in this lane.
								</div>{/if}
							{#each shown(lane.id) as e (busKey(e))}
								{@const dir = direction(e.envelope.topic)}
								{@const ack = store.ackOf(e)}
								{@const ev = eventOf(e.envelope.topic)}
								<button
									type="button"
									class="msg fresh {dir}"
									class:bad={isBad(e)}
									class:rel={hoverCid !== null && hoverCid === e.envelope.correlation_id}
									class:sel={store.selected !== null && busKey(store.selected) === busKey(e)}
									onclick={() => {
										store.selected = e;
										view = 'details';
									}}
									onmouseenter={() => (hoverCid = e.envelope.correlation_id)}
									onmouseleave={() => (hoverCid = null)}
								>
									<span class="r1"
										><span class="topic"
											><b class="dir" title={dir === 'down' ? 'forward' : 'return path'}
												>{dir === 'down' ? '↓' : '↑'}</b
											>{#if ev}<span class="ctx">{contextOf(e.envelope.topic)}</span>{ev}{:else}{e.envelope
													.topic}{/if}</span
										><span class="time">{clock(e.envelope.published_at_ms)}</span></span
									>
									{#if plain(e)}<span class="plain">{plain(e)}</span>{/if}
									<span class="route"
										>{e.envelope.from} → {e.consumer_group ?? '—'} · {e.envelope.correlation_id}</span
									>
									{#if summary(e)}<span class="sum">{summary(e)}</span>{/if}
									<span class="meta"
										><span class="skey">{e.stream}</span><span>{e.entry_id}</span>{#if ack}<span
												class="ack"
												class:done={ack === 'acked'}
												title={ack === 'acked'
													? `XACKed by ${e.consumer_group}`
													: `In ${e.consumer_group}: not acknowledged yet`}>{ack}</span
											>{/if}</span
									>
								</button>
							{/each}
							{#if fresh(lane.id) > 0 && !follow[lane.id]}
								<button type="button" class="newev" onclick={() => jump(lane.id)}
									>{fresh(lane.id)} new event{fresh(lane.id) === 1 ? '' : 's'} ↓</button
								>
							{/if}
						</div>
					</section>
				{/each}
				<div class="ipane" data-pane="details">
					<Inspector
						event={store.selected}
						agent={agentCard}
						ack={store.selected ? store.ackOf(store.selected) : null}
					/>
				</div>
			</div>
		</div>
	</div>
</div>
