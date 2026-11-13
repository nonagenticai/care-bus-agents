<script lang="ts" module>
	// ui.js statusOf plus open (warn) and unavailable (err).
	const MAP: Record<string, 'ok' | 'warn' | 'err' | 'muted'> = {
		ok: 'ok',
		healthy: 'ok',
		stable: 'ok',
		completed: 'ok',
		success: 'ok',
		PUBLISHED: 'ok',
		warn: 'warn',
		beta: 'warn',
		not_eligible: 'warn',
		degraded: 'warn',
		CANARY: 'warn',
		open: 'warn',
		running: 'warn',
		unsupported: 'err',
		queued: 'muted',
		rejected: 'err',
		cancelled: 'muted',
		cancelling: 'warn',
		deleting: 'warn',
		err: 'err',
		failed: 'err',
		failure: 'err',
		down: 'err',
		DISCARDED: 'err',
		unavailable: 'err',
		draft: 'muted',
		deprecated: 'muted',
		idle: 'muted',
		DRAFT: 'muted',
		ARCHIVED: 'muted',
	};
	// Case-insensitive: 'PUBLISHED', 'Published' and 'published' are the same status.
	const LOWER = Object.fromEntries(Object.entries(MAP).map(([k, v]) => [k.toLowerCase(), v]));
	export const statusOf = (w: string) => MAP[w] ?? LOWER[String(w ?? '').toLowerCase()] ?? 'muted';
</script>

<script lang="ts">
	let { word, label }: { word: string; label?: string } = $props();
</script>

<span class="st st-{statusOf(word)}"><i></i>{label ?? word.replaceAll('_', ' ')}</span>
