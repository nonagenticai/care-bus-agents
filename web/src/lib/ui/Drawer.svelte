<script lang="ts" module>
	let seq = 0;
	const FOCUSABLE =
		'a[href],button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),[tabindex]:not([tabindex="-1"]),[contenteditable="true"]';
	// Shared across all open drawers so stacked drawers restore exactly the prior state.
	const inerted = new Map<Element, { n: number; prior: boolean }>();
	let scrollLocks = 0;
	let priorOverflow = '';

	function isolate(keep: Element[]): () => void {
		const mine: Element[] = [];
		const keepSet = new Set(keep);
		let node: Element | null = keep[0] ?? null;
		while (node && node !== document.body) {
			const parent: Element | null = node.parentElement;
			if (!parent) break;
			for (const sib of Array.from(parent.children)) {
				if (keepSet.has(sib) || sib === node || sib.contains(node)) continue;
				if (['SCRIPT', 'STYLE', 'LINK'].includes(sib.tagName)) continue;
				const rec = inerted.get(sib);
				if (rec) rec.n++;
				else {
					inerted.set(sib, { n: 1, prior: sib.hasAttribute('inert') });
					sib.setAttribute('inert', '');
				}
				mine.push(sib);
			}
			node = parent;
		}
		if (scrollLocks++ === 0) {
			priorOverflow = document.body.style.overflow;
			document.body.style.overflow = 'hidden';
		}
		return () => {
			for (const el of mine) {
				const rec = inerted.get(el);
				if (!rec || --rec.n > 0) continue;
				inerted.delete(el);
				if (!rec.prior) el.removeAttribute('inert');
			}
			if (--scrollLocks === 0) document.body.style.overflow = priorOverflow;
		};
	}
</script>

<script lang="ts">
	import type { Snippet } from 'svelte';
	import { tick } from 'svelte';
	import Icon from './Icon.svelte';
	let {
		open = $bindable(false),
		title,
		sub,
		wide = false,
		dirty = false,
		children,
	}: {
		open?: boolean;
		title: string;
		sub?: Snippet;
		wide?: boolean;
		/** When true, Escape / scrim / close ask "Discard changes?" inside the drawer before closing. */
		dirty?: boolean;
		children?: Snippet;
	} = $props();

	const titleId = `drawer-title-${++seq}`;
	let panel = $state<HTMLElement>();
	let scrim = $state<HTMLElement>();
	let keepBtn = $state<HTMLButtonElement>();
	let confirming = $state(false);
	let opener: HTMLElement | null = null;

	// Focus moves in on open and returns to whatever opened the drawer on close.
	$effect(() => {
		if (!open) return;
		const active = document.activeElement;
		opener = active instanceof HTMLElement && active !== document.body ? active : null;
		tick().then(() => {
			if (panel && !panel.contains(document.activeElement)) panel.focus();
		});
		return () => {
			confirming = false;
			const back = opener;
			opener = null;
			if (back?.isConnected) tick().then(() => back.focus());
		};
	});

	// Background inert + scroll lock while open (siblings of the drawer up to <body>).
	$effect(() => {
		if (!open || !panel || !scrim) return;
		return isolate([panel, scrim]);
	});

	async function requestClose() {
		if (!dirty) {
			open = false;
			return;
		}
		confirming = true;
		await tick();
		keepBtn?.focus();
	}
	function discard() {
		confirming = false;
		open = false;
	}
	function keepEditing() {
		confirming = false;
		panel?.focus();
	}

	function onKey(e: KeyboardEvent) {
		if (!open || e.defaultPrevented) return;
		if (e.key === 'Escape') {
			e.preventDefault();
			if (confirming) keepEditing();
			else requestClose();
			return;
		}
		if (e.key !== 'Tab' || !panel) return;
		// Focus trap: Tab cycles inside the dialog.
		const items = [...panel.querySelectorAll<HTMLElement>(FOCUSABLE)].filter(
			(el) => !el.closest('[hidden],[inert]'),
		);
		if (!items.length) {
			e.preventDefault();
			panel.focus();
			return;
		}
		const first = items[0]!;
		const last = items[items.length - 1]!;
		const at = document.activeElement;
		if (!panel.contains(at)) {
			e.preventDefault();
			(e.shiftKey ? last : first).focus();
		} else if (e.shiftKey && (at === first || at === panel)) {
			e.preventDefault();
			last.focus();
		} else if (!e.shiftKey && at === last) {
			e.preventDefault();
			first.focus();
		}
	}
</script>

<svelte:window onkeydown={onKey} />
{#if open}
	<div class="scrim" role="presentation" bind:this={scrim} onclick={requestClose}></div>
	<div
		class="drawer"
		class:wide
		role="dialog"
		aria-modal="true"
		aria-labelledby={titleId}
		tabindex="-1"
		bind:this={panel}
	>
		<div class="drawer-head">
			<div>
				<h2 class="drawer-title" id={titleId}>{title}</h2>
				<div class="drawer-sub">{@render sub?.()}</div>
			</div>
			<button class="icon-btn" type="button" aria-label="Close details" onclick={requestClose}
				><Icon name="x" /></button
			>
		</div>
		{#if confirming}
			<div class="drawer-confirm" role="alertdialog" aria-label="Discard changes?">
				<strong>Discard changes?</strong>
				<span class="row">
					<button class="btn sm" type="button" bind:this={keepBtn} onclick={keepEditing}>Keep editing</button>
					<button class="btn sm danger" type="button" onclick={discard}>Discard</button>
				</span>
			</div>
		{/if}
		<div class="drawer-body">{@render children?.()}</div>
	</div>
{/if}
