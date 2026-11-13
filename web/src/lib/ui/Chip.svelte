<script lang="ts">
	// ui.js chip; variants cust|work|tool|dead|ok|warn|err|muted|accent.
	let {
		text,
		variant = 'muted',
		title,
	}: {
		text: string;
		variant?: 'cust' | 'work' | 'tool' | 'dead' | 'ok' | 'warn' | 'err' | 'muted' | 'accent';
		title?: string;
	} = $props();
</script>

<span class="chip {variant}" {title}>{text}</span>
