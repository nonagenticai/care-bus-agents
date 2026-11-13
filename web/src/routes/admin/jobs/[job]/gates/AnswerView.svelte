<script lang="ts">
	import Chip from '$lib/ui/Chip.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import { fmt } from '$lib/format';
	import {
		isSkipped,
		probabilityRows,
		ruleSummary,
		scoreLevels,
		wouldFire,
		type GateAnswer,
		type GateQuestion,
	} from '$lib/api/gates';

	// `fired` is carebus-api's verdict (the key is in fired_keys); the rule is re-applied only to explain it.
	// `text`: what the question was asked about (the playground's `texts`), when known.
	let {
		q,
		a,
		fired,
		text = null,
	}: { q: GateQuestion; a: GateAnswer | null; fired: boolean; text?: string | null } = $props();
	const skipped = $derived(isSkipped(a));
	const local = $derived(skipped ? null : wouldFire(q, a));
	const rows = $derived(a ? probabilityRows(q.type, a, q.criteria) : []);
	const level = $derived(
		a && typeof a.score === 'number' ? (scoreLevels(q.criteria)[a.score] ?? null) : null,
	);
</script>

<div class="ans" class:fired data-testid="answer-{q.key}">
	<div class="head">
		<span class="mono key">{q.key}</span>
		<Chip text={q.type} variant="accent" />
		{#if fired}<Chip text="fired · {q.action}" variant="err" />{:else if skipped}<Chip
				text="skipped · precheck"
				variant="muted"
			/>{:else if a}<Chip text="no fire" variant="ok" />{:else}<Chip text="not asked" variant="muted" />{/if}
		<span class="muted rule">{ruleSummary(q)}</span>
	</div>
	{#if text != null}
		<div class="line">
			<span class="lbl">{skipped ? 'not sent' : 'sent'}</span><span class="mono sent">{text}</span>
		</div>
	{/if}
	{#if skipped}
		<p class="muted">
			Its precheck did not match this text, so laya was not asked. A skipped question never fires.
		</p>
	{:else if a}
		{#if q.type === 'noul'}
			<div class="line">
				<span class="lbl">noul</span>
				<Meter value={a.noul ?? 0} cls={fired ? 's-err' : 's-accent'} />
				<span class="mono">{a.noul == null ? '—' : a.noul.toFixed(2)}</span>
			</div>
		{:else if q.type === 'choice'}
			<div class="line">
				<span class="lbl">choice</span><span class="mono">{a.choice ?? '—'}</span>
				<span class="muted">confidence {a.confidence == null ? '—' : a.confidence.toFixed(2)}</span>
			</div>
		{:else}
			<div class="line">
				<span class="lbl">score</span><span class="mono">{a.score ?? '—'}</span>
				{#if level}<span>{level}</span>{/if}
				{#if a.confidence != null}<span class="muted">confidence {a.confidence.toFixed(2)}</span>{/if}
			</div>
		{/if}
		{#each rows as [label, p]}
			<div class="line prob">
				<span class="lbl mono">{label}</span>
				<Meter
					value={p}
					cls={(q.type === 'choice' && label === a.choice) ||
					(q.type === 'score' && label.startsWith(`${a.score} `))
						? 's-accent'
						: 's-muted'}
				/>
				<span class="mono">{fmt.pct(p, 0)}</span>
			</div>
		{/each}
		{#if local !== null && local !== fired}
			<p class="muted">The server's verdict differs from this rule applied to the answer shown.</p>
		{/if}
	{/if}
</div>

<style>
	.ans {
		border: 1px solid var(--line-2);
		border-left: 3px solid var(--line);
		border-radius: var(--r);
		padding: 8px 10px;
		display: grid;
		gap: 4px;
	}
	.ans.fired {
		border-left-color: var(--err);
	}
	.head {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.key {
		font-weight: 600;
	}
	.rule {
		font-size: 12px;
	}
	.line {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
		font-size: 13px;
	}
	.lbl {
		min-width: 6em;
		color: var(--muted);
	}
	.prob .lbl {
		min-width: 10em;
	}
	.sent {
		white-space: pre-wrap;
		word-break: break-word;
		flex: 1;
		min-width: 0;
	}
</style>
