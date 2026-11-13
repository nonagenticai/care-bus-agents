<script lang="ts">
	// Onboarding: the early-access signup queue (care-bus 17; UX from agent's platform/signups page), super_admin
	// only, on carebus-api `/api/signup-approvals*`. A request arrives from the website or the signup chat;
	// approving it founds the requester's workspace with them as its tenant admin (login, workspace, seat, licences,
	// one set-password email in their language), journaled step by step and shown here. Rejecting sends nothing and
	// undoes what an interrupted approve had founded. The Telegram notice links `?signup=<id>` here (with
	// `&action=approve|reject` the decision opens prefilled; nothing is posted until confirmed).
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { replaceState } from '$app/navigation';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { ApiError } from '$lib/api/client';
	import {
		CONFLICT_TEXT,
		FILTERS,
		LOCALE_NAMES,
		REMOVAL_STEP_VARIANT,
		ROW_STATE,
		SIGNUP_LOCALES,
		STEP_VARIANT,
		UNCERTAIN_ACTION_TEXT,
		UNCERTAIN_EMAIL_TEXT,
		Latest,
		applies,
		canRemove,
		canRetry,
		canRetryRemoval,
		emailNote,
		emptyInvite,
		inviteFault,
		isFilter,
		localeLabel,
		maskEmail,
		removalConfirmed,
		removalStepLabel,
		repairLine,
		replaceRow,
		rowState,
		serverStatus,
		stepLabel,
		stepSummary,
		toInviteInput,
		uncertainEmail,
		uncertainFailure,
		validateInvite,
		visible,
		type Fault,
		type Filter,
		type InviteField,
		type RemovalStep,
		type Signup,
		type SignupPage,
		type SignupStep,
	} from '$lib/api/signups';
	import Callout from '$lib/ui/Callout.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { toast } from '$lib/ui/toaster.svelte';

	const allowed = $derived(session.superAdmin);
	const when = (iso: string | null) => (iso ? new Date(iso).toLocaleString('en-GB') : '—');

	// ---- the queue --------------------------------------------------------------------------------------------
	const initialFilter = page.url.searchParams.get('status');
	let filter = $state<Filter>(isFilter(initialFilter) ? initialFilter : 'pending');
	let list = $state<
		| { state: 'loading' }
		| { state: 'off'; message: string }
		| { state: 'error'; message: string; retryable: boolean }
		| { state: 'ok'; page: SignupPage; items: Signup[] }
	>({ state: 'loading' });
	let loadingMore = $state(false);
	const listReq = new Latest();
	const rows = $derived(list.state === 'ok' ? visible(filter, list.items) : []);
	const meta = $derived(list.state === 'ok' ? list.page : null);

	function offOrError(e: unknown): { state: 'off'; message: string } | null {
		return e instanceof ApiError && e.code === 'signup_not_configured'
			? { state: 'off', message: e.message }
			: null;
	}

	/** The first page of the filter; an answer for an older filter or an older load is dropped. */
	async function load() {
		const t = listReq.next();
		list = { state: 'loading' };
		try {
			const p = await api().signups({ status: serverStatus(filter) });
			if (!listReq.current(t)) return;
			list = { state: 'ok', page: p, items: p.items };
		} catch (e) {
			if (!listReq.current(t)) return;
			list = offOrError(e) ?? {
				state: 'error',
				message:
					e instanceof ApiError && e.status === 403
						? 'Only a platform super admin reviews signups.'
						: (e as Error).message,
				retryable: !(e instanceof ApiError && e.status === 403),
			};
		}
	}

	async function more() {
		if (list.state !== 'ok' || !list.page.next || loadingMore) return;
		const t = listReq.next();
		loadingMore = true;
		try {
			const p = await api().signups({ status: serverStatus(filter), before: list.page.next });
			if (!listReq.current(t) || list.state !== 'ok') return;
			list = { state: 'ok', page: p, items: [...list.items, ...p.items] };
		} catch (e) {
			if (listReq.current(t)) toast(`Not loaded: ${(e as Error).message}`, 'err');
		} finally {
			loadingMore = false;
		}
	}

	function show(f: Filter) {
		filter = f;
		void load();
	}

	/** An action's answer, folded into the loaded rows (the row may have left the filter: the reload decides). */
	function patchRow(s: Signup) {
		if (list.state === 'ok') list = { ...list, items: replaceRow(list.items, s) };
	}

	// ---- the detail drawer ------------------------------------------------------------------------------------
	type Mode = 'view' | 'approve' | 'reject' | 'retry' | 'remove';
	type Action = 'approve' | 'reject' | 'retry' | 'remove' | 'removal_retry';
	let selectedId = $state<string | null>(null);
	let drawerOpen = $state(false);
	// The drawer closed itself (Escape, scrim, close button): the selection goes with it.
	$effect(() => {
		if (!drawerOpen && selectedId !== null) closeDetail();
	});
	let detail = $state<Signup | null>(null);
	let detailState = $state<'idle' | 'loading' | 'ok' | 'missing' | 'error'>('idle');
	let detailError = $state('');
	let mode = $state<Mode>('view');
	/** Why the mode a link asked for was not opened (the signup was decided already). */
	let modeNote = $state('');
	const detailReq = new Latest();
	let tenantNameInput = $state('');
	let reasonInput = $state('');
	/** The action running, and for which signup: its button says so and every action button waits. */
	let busy = $state<{ id: string; action: Action } | null>(null);
	let confirmInput = $state('');
	let fault = $state<Fault | null>(null);
	let notice = $state<{ title: string; text: string; steps: SignupStep[] } | null>(null);
	let linkError = $state('');

	function setUrl(id: string | null) {
		try {
			const u = new URL(page.url);
			if (id) u.searchParams.set('signup', id);
			else u.searchParams.delete('signup');
			u.searchParams.delete('action');
			replaceState(u, {});
		} catch {
			/* before the router is ready: the URL just stays */
		}
	}

	function enter(m: Mode, s: Signup | null) {
		modeNote = '';
		if ((m === 'approve' || m === 'reject') && s && s.status !== 'pending') {
			modeNote = `This signup was ${s.status} already, so it cannot be ${m === 'approve' ? 'approved' : 'rejected'}.`;
			m = 'view';
		}
		if (m === 'retry' && s && !canRetry(s)) m = 'view';
		if (m === 'remove' && s && !canRemove(s)) m = 'view';
		if (m === 'remove') confirmInput = '';
		mode = m;
		fault = null;
		if (m === 'approve' && s) tenantNameInput = s.suggested_tenant_name;
		if (m === 'reject') reasonInput = '';
	}

	/** Re-reads one signup. Its answer applies only while it is still the selected one and the latest read. */
	async function readDetail(id: string): Promise<Signup | null> {
		const t = detailReq.next();
		if (!detail || detail.id !== id) detailState = 'loading';
		try {
			const s = await api().signup(id);
			if (!applies(detailReq, t, id, selectedId)) return null;
			detail = s;
			detailState = 'ok';
			patchRow(s);
			return s;
		} catch (e) {
			if (!applies(detailReq, t, id, selectedId)) return null;
			if (e instanceof ApiError && e.status === 404) detailState = 'missing';
			else {
				detailState = detail?.id === id ? 'ok' : 'error';
				detailError = (e as Error).message;
				if (detailState === 'ok') toast(`Not refreshed: ${(e as Error).message}`, 'err');
			}
			return null;
		}
	}

	async function select(id: string, m: Mode = 'view') {
		selectedId = id;
		drawerOpen = true;
		notice = null;
		const known = list.state === 'ok' ? (list.items.find((x) => x.id === id) ?? null) : null;
		detail = known;
		detailState = known ? 'ok' : 'loading';
		enter(m, known);
		setUrl(id);
		const fresh = await readDetail(id);
		// The fresh state decides whether the asked-for decision can still be taken.
		if (fresh && selectedId === id && mode === m) enter(m, fresh);
	}

	function closeDetail() {
		selectedId = null;
		drawerOpen = false;
		detail = null;
		detailState = 'idle';
		mode = 'view';
		fault = null;
		modeNote = '';
		setUrl(null);
	}

	/** Shared by approve / reject / retry: in-flight state, uncertain answers re-read, never resent. */
	async function act(s: Signup, action: Action, run: () => Promise<void>): Promise<void> {
		if (busy) return;
		busy = { id: s.id, action };
		fault = null;
		try {
			await run();
		} catch (e) {
			if (uncertainFailure(e)) {
				if (selectedId === s.id)
					fault = { field: null, code: 'no_answer', message: UNCERTAIN_ACTION_TEXT, steps: [] };
				else toast(UNCERTAIN_ACTION_TEXT, 'err');
				if (selectedId === s.id) {
					await readDetail(s.id);
					mode = 'view';
				}
			} else {
				const f = inviteFault(e);
				if (selectedId === s.id) fault = { ...f, field: null };
				else toast(`${action} failed: ${f.message}`, 'err');
				if (
					e instanceof ApiError &&
					[
						'signup_not_pending',
						'approval_in_progress',
						'removal_exists',
						'removal_in_progress',
						'removal_done',
						'signup_not_approved',
					].includes(e.code)
				) {
					if (selectedId === s.id) await readDetail(s.id);
				}
			}
			void load();
		} finally {
			busy = null;
		}
	}

	function approve(e: SubmitEvent) {
		e.preventDefault();
		const s = detail;
		if (!s) return;
		const name = tenantNameInput.trim();
		if (name.length < 2 || name.length > 120) {
			fault = {
				field: null,
				code: 'invalid_body',
				message: 'The workspace name is 2 to 120 characters.',
				steps: [],
			};
			return;
		}
		void act(s, 'approve', async () => {
			const r = await api().approveSignup(s.id, name === s.suggested_tenant_name ? undefined : name);
			notice = {
				title: 'Approved',
				text: `${s.company ?? s.name}: workspace “${r.tenant.name}” created with ${maskEmail(s.email)} as its admin; ${emailNote(r.steps)}.`,
				steps: r.steps,
			};
			toast('Signup approved.');
			if (selectedId === s.id) {
				detail = r.signup;
				mode = 'view';
			}
			patchRow(r.signup);
			void load();
		});
	}

	function reject(e: SubmitEvent) {
		e.preventDefault();
		const s = detail;
		if (!s) return;
		const reason = reasonInput.trim();
		if (reason.length > 500) {
			fault = {
				field: null,
				code: 'invalid_body',
				message: 'The reason is at most 500 characters.',
				steps: [],
			};
			return;
		}
		void act(s, 'reject', async () => {
			const r = await api().rejectSignup(s.id, reason || undefined);
			const repaired = repairLine(r.reject_repair);
			notice = {
				title: 'Rejected',
				text: repaired
					? `${s.company ?? s.name}: rejected. An interrupted approve had started things; cleaned up: ${repaired}. Nothing was sent.`
					: `${s.company ?? s.name}: rejected. Nothing was created or sent.`,
				steps: [],
			};
			toast('Signup rejected.');
			if (selectedId === s.id) {
				detail = { ...s, ...r, steps: s.steps };
				mode = 'view';
			}
			void load();
		});
	}

	/** Retry from a row or the drawer. An uncertain email is confirmed in the drawer first: a resend kills the first link. */
	function retry(s: Signup, confirmed = false) {
		if (uncertainEmail(s) && !confirmed) {
			void select(s.id, 'retry');
			return;
		}
		void act(s, 'retry', async () => {
			const r = await api().retrySignup(s.id);
			const still = stepSummary(r.steps).failed.length;
			toast(
				still ? `Retried; ${still} step${still === 1 ? '' : 's'} still failing.` : 'Failed steps run again.',
				still ? 'err' : 'ok',
			);
			const next = { ...s, steps: r.steps };
			patchRow(next);
			if (selectedId === s.id) {
				detail = next;
				mode = 'view';
				await readDetail(s.id);
			}
		});
	}

	/** Remove an approved signup end to end, confirmed with the workspace slug typed back. */
	function remove(e: SubmitEvent) {
		e.preventDefault();
		const s = detail;
		if (!s) return;
		if (!removalConfirmed(confirmInput, s.tenant_slug)) {
			fault = {
				field: null,
				code: 'removal_confirm_mismatch',
				message: 'Type the workspace slug exactly as shown to confirm.',
				steps: [],
			};
			return;
		}
		void act(s, 'remove', async () => {
			const r = await api().removeSignup(s.id, confirmInput.trim());
			removalDone(r, 'Removal');
		});
	}

	function retryRemoval(s: Signup) {
		void act(s, 'removal_retry', async () => {
			const r = await api().retryRemoval(s.id);
			removalDone(r, 'Removal retried');
		});
	}

	function removalDone(r: Signup, what: string) {
		const state = r.removal?.state ?? r.removal_state;
		toast(
			state === 'done'
				? `${what}: workspace removed.`
				: `${what}: ${state === 'pending' ? 'still running, retried automatically' : `stopped (${state}); see the steps`}.`,
			state === 'done' || state === 'pending' ? 'ok' : 'err',
		);
		patchRow(r);
		if (selectedId === r.id) {
			detail = r;
			mode = 'view';
		}
		void load();
	}

	const label = (s: Signup, action: Action, idle: string, running: string) =>
		busy?.id === s.id && busy.action === action ? running : idle;

	// ---- invite -----------------------------------------------------------------------------------------------
	let inviteOpen = $state(false);
	let inv = $state(emptyInvite());
	let inviteTouched = $state(false);
	let inviting = $state(false);
	let inviteError = $state<Fault | null>(null);
	const inviteErrors = $derived(validateInvite(inv));
	const fieldMsg = (f: InviteField) =>
		inviteError?.field === f ? inviteError.message : inviteTouched ? (inviteErrors[f] ?? '') : '';
	const faultAttrs = (f: InviteField) =>
		fieldMsg(f) ? { 'aria-invalid': 'true' as const, 'aria-describedby': `inv-err-${f}` } : {};
	const inviteDirty = $derived(
		!inviting && Object.values(inv).some((v) => typeof v === 'string' && v.trim() !== ''),
	);

	function openInvite() {
		inv = emptyInvite();
		inviteTouched = false;
		inviteError = null;
		inviteOpen = true;
	}

	async function sendInvite(e: SubmitEvent) {
		e.preventDefault();
		if (inviting) return;
		inviteTouched = true;
		inviteError = null;
		if (Object.keys(inviteErrors).length) return;
		const input = toInviteInput(inv);
		inviting = true;
		try {
			const r = await api().inviteSignup(input);
			notice = {
				title: 'Invited',
				text: `${input.name}: workspace “${r.tenant.name}” created with ${maskEmail(input.email)} as its admin${input.locale ? ` (email in ${LOCALE_NAMES[input.locale]})` : ''}; ${emailNote(r.steps)}.${r.reused_pending ? ' Their earlier pending request was approved instead of a new one.' : ''}`,
				steps: r.steps,
			};
			toast('Invite sent.');
			inviteOpen = false;
			show('approved');
		} catch (err) {
			inviteError = uncertainFailure(err)
				? {
						field: null,
						code: 'no_answer',
						message:
							'No answer came back, so the invite may have gone through. It is not sent again by itself: look for this address in the queue (Approved) before sending it again.',
						steps: [],
					}
				: inviteFault(err);
			if (uncertainFailure(err)) void load();
		} finally {
			inviting = false;
		}
	}

	onMount(() => {
		if (!allowed) return;
		const target = page.url.searchParams.get('signup');
		const action = page.url.searchParams.get('action');
		void load();
		if (target) {
			void select(target, action === 'approve' || action === 'reject' ? action : 'view').then(() => {
				if (detailState === 'missing')
					linkError = `No signup ${target}: the link is wrong or the request is gone.`;
			});
		}
	});
</script>

{#snippet journal(list: SignupStep[])}
	{#if list.length}
		<ol class="journal" aria-label="Provisioning steps" data-testid="provisioning-steps">
			{#each list as st (st.step)}
				<li data-testid="provisioning-step" data-step={st.step} data-status={st.status}>
					<Chip text={st.status} variant={STEP_VARIANT[st.status] ?? 'muted'} />
					<b>{stepLabel(st.step)}</b>
					{#if st.attempts > 1}<span class="cell-sub">{st.attempts} attempts</span>{/if}
					{#if st.status === 'pending' && st.next_attempt_at}<span class="cell-sub"
							>next try {when(st.next_attempt_at)}</span
						>{/if}
					{#if st.code || st.message}
						<details class="raw">
							<summary>Error details</summary>
							<code class="mono">{st.code ?? ''}</code>{#if st.message}<span class="cell-sub"
									>{st.message}</span
								>{/if}
						</details>
					{/if}
				</li>
			{/each}
		</ol>
	{/if}
{/snippet}

{#snippet removalJournal(list: RemovalStep[])}
	<ol class="journal" aria-label="Removal steps" data-testid="removal-steps">
		{#each list as st (st.step)}
			<li data-testid="removal-step" data-step={st.step} data-status={st.status}>
				<Chip text={st.status} variant={REMOVAL_STEP_VARIANT[st.status] ?? 'muted'} />
				<b>{removalStepLabel(st.step)}</b>
				{#if st.attempts > 1}<span class="cell-sub">{st.attempts} attempts</span>{/if}
				{#if st.code || st.message}
					<details class="raw">
						<summary>Details</summary>
						<code class="mono">{st.code ?? ''}</code>{#if st.message}<span class="cell-sub">{st.message}</span
							>{/if}
					</details>
				{/if}
			</li>
		{/each}
	</ol>
{/snippet}

{#snippet faultBox(f: Fault)}
	<Callout kind="err" title={f.code === 'no_answer' ? 'Outcome unknown' : f.code}>{f.message}</Callout>
	{#if f.steps.length}
		<p class="note">Nothing was decided; approving again resumes from the step that failed.</p>
		{@render journal(f.steps)}
	{/if}
{/snippet}

<header class="page-head">
	<div>
		<h1>Onboarding</h1>
		<p>
			Early-access requests waiting for a workspace. Approve one and its workspace is founded with the
			requester as tenant admin, each step journaled here; reject it and nothing is sent.
		</p>
	</div>
	{#if allowed && list.state !== 'off'}
		<button class="btn primary" type="button" data-testid="invite-open" onclick={openInvite}>Invite…</button>
	{/if}
</header>

{#if !allowed}
	<Callout kind="err" title="Super-admin only"
		>Only a platform super admin reviews early-access signups.</Callout
	>
{:else}
	{#if linkError}<Callout kind="err" title="Link not found">{linkError}</Callout>{/if}
	{#if notice}
		<Callout kind="info" title={notice.title}>
			<span data-testid="onboarding-notice">{notice.text}</span>
			{@render journal(notice.steps)}
		</Callout>
	{/if}

	{#if list.state === 'off'}
		<Card>
			<div class="empty" role="status" data-testid="signups-off">
				<h2>Early access is off</h2>
				<p>
					Self-signup is not enabled on this deployment (carebus-api answers <code>signup_not_configured</code
					>), so nobody can request a workspace and nothing can be approved here. It is switched on with
					<code>CAREBUS_SIGNUP_ENABLED</code> once every part the approve needs is configured.
				</p>
				<p class="cell-sub">{list.message}</p>
			</div>
		</Card>
	{:else}
		<div class="split bar">
			<div class="seg" role="group" aria-label="Signup status">
				{#each FILTERS as [f, name] (f)}
					<button
						type="button"
						class="tab"
						class:on={filter === f}
						aria-pressed={filter === f}
						onclick={() => show(f)}
						>{name}{#if meta && f === 'pending'}<span class="count">{meta.pending}</span
							>{:else if meta && f === 'approved'}<span class="count">{meta.counts.approved}</span
							>{:else if meta && f === 'rejected'}<span class="count">{meta.counts.rejected}</span
							>{:else if meta && f === 'removed' && meta.counts.removed !== undefined}<span class="count"
								>{meta.counts.removed}</span
							>{/if}</button
					>
				{/each}
			</div>
			{#if meta}
				<span class="muted" data-testid="queue-meta"
					>{meta.pending} of {meta.queue_cap} waiting · {meta.counts.received} received · as of {when(
						meta.as_of,
					)}</span
				>
			{/if}
		</div>

		{#if list.state === 'loading'}
			<p class="muted" role="status">Loading signups…</p>
		{:else if list.state === 'error'}
			<Unavailable reason={list.message} retry={list.retryable ? () => void load() : undefined} />
		{:else if rows.length}
			<Card cls="flush">
				<div class="tbl-wrap">
					<table class="tbl" data-testid="signup-rows">
						<thead>
							<tr
								><th>Requested</th><th>Who</th><th>Company</th><th>Via</th><th>State</th><th>Steps</th><th
									aria-label="Actions"
								></th></tr
							>
						</thead>
						<tbody>
							{#each rows as s (s.id)}
								{@const st = rowState(s)}
								{@const sum = stepSummary(s.steps)}
								<tr data-testid="signup-row" data-id={s.id} class:sel={selectedId === s.id}>
									<td class="nowrap">{when(s.created_at)}</td>
									<td>
										<button class="linkish" type="button" onclick={() => select(s.id)}>{s.name}</button>
										<span class="cell-sub mono" title="Shown in full in the detail">{maskEmail(s.email)}</span
										>
									</td>
									<td>{s.company ?? '—'}</td>
									<td>
										<Chip text={s.source} />
										<span class="cell-sub">{localeLabel(s.locale)}</span>
									</td>
									<td><Chip text={ROW_STATE[st].label} variant={ROW_STATE[st].variant} /></td>
									<td>
										{#if sum.total}
											<span data-testid="step-summary">{sum.done}/{sum.total} done</span>
											{#if sum.failed.length}<span class="cell-sub err-text"
													>{sum.failed.map((x) => stepLabel(x.step)).join(', ')} failed</span
												>{:else if sum.pending.length}<span class="cell-sub"
													>{stepLabel(sum.pending[0]!.step)} next</span
												>{/if}
										{:else}
											<span class="muted">—</span>
										{/if}
										{#if uncertainEmail(s)}
											<span class="cell-sub warn-text" data-testid="uncertain-email"
												>⚠ email outcome unknown</span
											>
										{/if}
									</td>
									<td class="r nowrap">
										{#if s.status === 'pending'}
											<button
												class="btn sm primary"
												type="button"
												disabled={!!busy || s.approving}
												title={s.approving ? 'An approve is running for this signup' : ''}
												onclick={() => select(s.id, 'approve')}>Approve…</button
											>
											<button
												class="btn sm"
												type="button"
												disabled={!!busy}
												onclick={() => select(s.id, 'reject')}>Reject…</button
											>
										{:else if canRetry(s)}
											<button
												class="btn sm"
												type="button"
												data-testid="signup-retry"
												disabled={!!busy}
												onclick={() => retry(s)}
												>{label(s, 'retry', uncertainEmail(s) ? 'Retry…' : 'Retry', 'Retrying…')}</button
											>
										{/if}
									</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>
			{#if list.page.next}
				<button class="btn" type="button" disabled={loadingMore} onclick={more}
					>{loadingMore ? 'Loading…' : 'Load more'}</button
				>
			{/if}
		{:else}
			<Card>
				<p class="empty">
					{filter === 'pending'
						? 'No pending signups. New requests land here and in the ops Telegram chat.'
						: filter === 'failed'
							? list.page.next
								? 'No failed steps among the signups loaded so far.'
								: 'No approved signup has a failed step.'
							: `No ${filter === 'all' ? '' : `${filter} `}signups.`}
				</p>
				{#if filter === 'failed' && list.page.next}
					<button class="btn sm" type="button" disabled={loadingMore} onclick={more}
						>{loadingMore ? 'Loading…' : 'Look further'}</button
					>
				{/if}
			</Card>
		{/if}
	{/if}
{/if}

<Drawer bind:open={drawerOpen} title={detail ? (detail.company ?? detail.name) : 'Signup'} wide>
	{#snippet sub()}{#if detail}<span class="mono">{detail.email}</span>{/if}{/snippet}
	{#if detailState === 'loading'}
		<p class="muted" role="status">Loading…</p>
	{:else if detailState === 'missing'}
		<Callout kind="err" title="Not found"
			>No signup {selectedId}: the link is wrong or the request is gone.</Callout
		>
	{:else if detailState === 'error' || !detail}
		<Unavailable
			reason={detailError || 'Not loaded.'}
			retry={() => selectedId && void readDetail(selectedId)}
		/>
	{:else}
		{@const s = detail}
		{@const st = rowState(s)}
		<div class="stack" data-testid="signup-detail" data-id={s.id}>
			{#if modeNote}<Callout kind="info" title="Already decided">{modeNote}</Callout>{/if}
			<div class="row">
				<Chip text={ROW_STATE[st].label} variant={ROW_STATE[st].variant} />
				<Chip text={`via ${s.source}`} />
				{#if s.invited_by}<Chip text={`invited by ${s.invited_by}`} />{/if}
				<Chip text={localeLabel(s.locale)} variant="accent" />
				{#if s.trust}<Chip text={`trust: ${s.trust}`} variant="ok" />{/if}
			</div>
			<dl class="kv">
				<dt>Name</dt>
				<dd>{s.name}</dd>
				<dt>Company</dt>
				<dd>{s.company ?? '—'}</dd>
				<dt>Requested</dt>
				<dd>{when(s.created_at)}</dd>
				{#if s.status !== 'pending'}
					<dt>Decided</dt>
					<dd>
						{when(s.decided_at)}{s.decided_by ? ` · ${s.decided_by}` : ''}{s.decided_via
							? ` · ${s.decided_via}`
							: ''}
					</dd>
				{/if}
				{#if s.tenant_id}
					<dt>Workspace</dt>
					<dd class="mono">{s.tenant_id}</dd>
				{/if}
				{#if s.reason}
					<dt>Reason</dt>
					<dd>{s.reason}</dd>
				{/if}
				{#if repairLine(s.reject_repair)}
					<dt>Cleaned up</dt>
					<dd>{repairLine(s.reject_repair)}</dd>
				{/if}
			</dl>
			{#if s.message}<blockquote class="msg">{s.message}</blockquote>{/if}
			{#if uncertainEmail(s)}
				<Callout kind="gap" title="Email outcome unknown">{UNCERTAIN_EMAIL_TEXT}</Callout>
			{/if}
			{#if s.steps.length}
				<div>
					<span class="label">Provisioning</span>
					{@render journal(s.steps)}
				</div>
			{/if}
			{#if s.removal}
				<div data-testid="removal" data-state={s.removal.state}>
					<span class="label"
						>Removal · {s.removal.state} · asked by {s.removal.requested_by} at {when(
							s.removal.requested_at,
						)}</span
					>
					{#if s.removal.state === 'blocked' || s.removal.state === 'failed'}
						<Callout kind="err" title="Removal stopped"
							>A step was refused. Look at its details, fix the cause, then Retry removal: the steps that
							already landed are not run again.</Callout
						>
					{/if}
					{@render removalJournal(s.removal.steps)}
				</div>
			{/if}

			{#if mode === 'approve'}
				<form class="stack confirm" onsubmit={approve} data-testid="approve-form">
					<div class="field">
						<label for="tenant-name">Workspace name</label>
						<input
							id="tenant-name"
							class="input"
							bind:value={tenantNameInput}
							minlength="2"
							maxlength="120"
							required
						/>
					</div>
					<div>
						<span class="label">When you confirm</span>
						<ol class="plan">
							<li>A care-bus login for {s.email} (never one that already exists).</li>
							<li>Workspace “{tenantNameInput.trim() || '…'}” with {s.name} as its tenant admin.</li>
							<li>
								Its licences, then one set-password email in {s.locale ? localeLabel(s.locale) : 'English'}.
							</li>
						</ol>
					</div>
					{#if fault}{@render faultBox(fault)}{/if}
					<div class="row">
						<button class="btn primary" type="submit" disabled={!!busy}
							>{label(s, 'approve', 'Approve and create workspace', 'Approving…')}</button
						>
						<button class="btn" type="button" disabled={!!busy} onclick={() => enter('view', s)}
							>Cancel</button
						>
					</div>
				</form>
			{:else if mode === 'reject'}
				<form class="stack confirm" onsubmit={reject} data-testid="reject-form">
					<p class="note">
						Nothing is sent to {s.name}. If an earlier approve was interrupted, what it founded is undone
						(workspace suspended, login retired).
					</p>
					<div class="field">
						<label for="reject-reason">Reason (optional, kept for the audit)</label>
						<textarea id="reject-reason" class="input" rows="3" maxlength="500" bind:value={reasonInput}
						></textarea>
					</div>
					{#if fault}{@render faultBox(fault)}{/if}
					<div class="row">
						<button class="btn danger" type="submit" disabled={!!busy}
							>{label(s, 'reject', 'Reject signup', 'Rejecting…')}</button
						>
						<button class="btn" type="button" disabled={!!busy} onclick={() => enter('view', s)}
							>Cancel</button
						>
					</div>
				</form>
			{:else if mode === 'remove'}
				<form class="stack confirm" onsubmit={remove} data-testid="remove-form">
					<div>
						<span class="label">Removing this workspace</span>
						<ol class="plan">
							<li>Suspends the workspace and stops serving it.</li>
							<li>Deletes every member of it.</li>
							<li>Revokes all of its licences, then archives it in licences.</li>
							<li>Deletes the care-bus login created for {s.email}, so the address can sign up again.</li>
						</ol>
						<p class="note">This cannot be undone from here.</p>
					</div>
					<div class="field">
						<label for="remove-confirm"
							>Type the workspace slug <code class="mono">{s.tenant_slug ?? '…'}</code> to confirm</label
						>
						<input
							id="remove-confirm"
							class="input mono"
							autocomplete="off"
							bind:value={confirmInput}
							data-testid="remove-confirm"
						/>
					</div>
					{#if fault}{@render faultBox(fault)}{/if}
					<div class="row">
						<button
							class="btn danger"
							type="submit"
							disabled={!!busy || !removalConfirmed(confirmInput, s.tenant_slug)}
							>{label(s, 'remove', 'Remove workspace', 'Removing…')}</button
						>
						<button class="btn" type="button" disabled={!!busy} onclick={() => enter('view', s)}
							>Cancel</button
						>
					</div>
				</form>
			{:else if mode === 'retry'}
				<div class="stack confirm" data-testid="retry-confirm">
					<p>
						Retry runs every failed step again{uncertainEmail(s)
							? ', including the email: the requester gets a new link and the first one stops working'
							: ''}.
					</p>
					{#if fault}{@render faultBox(fault)}{/if}
					<div class="row">
						<button class="btn primary" type="button" disabled={!!busy} onclick={() => retry(s, true)}
							>{label(s, 'retry', 'Retry failed steps', 'Retrying…')}</button
						>
						<button class="btn" type="button" disabled={!!busy} onclick={() => enter('view', s)}
							>Cancel</button
						>
					</div>
				</div>
			{:else}
				{#if fault}{@render faultBox(fault)}{/if}
				<div class="row">
					{#if s.status === 'pending'}
						<button
							class="btn primary"
							type="button"
							disabled={!!busy || s.approving}
							onclick={() => enter('approve', s)}>Approve…</button
						>
						<button class="btn" type="button" disabled={!!busy} onclick={() => enter('reject', s)}
							>Reject…</button
						>
					{:else if canRetry(s)}
						<button
							class="btn"
							type="button"
							disabled={!!busy}
							onclick={() => (uncertainEmail(s) ? enter('retry', s) : retry(s, true))}
							>{label(s, 'retry', uncertainEmail(s) ? 'Retry…' : 'Retry failed steps', 'Retrying…')}</button
						>
					{/if}
					{#if canRemove(s)}
						<button
							class="btn danger"
							type="button"
							data-testid="signup-remove"
							disabled={!!busy || !s.tenant_slug}
							title={s.tenant_slug ? '' : 'Loading the workspace slug…'}
							onclick={() => enter('remove', s)}>Remove…</button
						>
					{/if}
					{#if canRetryRemoval(s)}
						<button
							class="btn"
							type="button"
							data-testid="removal-retry"
							disabled={!!busy}
							onclick={() => retryRemoval(s)}
							>{label(s, 'removal_retry', 'Retry removal', 'Retrying…')}</button
						>
					{/if}
					<button class="btn" type="button" onclick={() => readDetail(s.id)}>Reload</button>
				</div>
			{/if}
		</div>
	{/if}
</Drawer>

<Drawer bind:open={inviteOpen} title="Invite to early access" dirty={inviteDirty}>
	{#snippet sub()}A pre-approved request: the workspace is founded and its admin invited at once.{/snippet}
	<form class="stack" onsubmit={sendInvite} data-testid="invite-form" novalidate>
		<div class="field">
			<label for="inv-email">Email</label>
			<input
				id="inv-email"
				class="input"
				type="email"
				autocomplete="off"
				bind:value={inv.email}
				{...faultAttrs('email')}
			/>
			{#if fieldMsg('email')}<span class="field-err" id="inv-err-email">{fieldMsg('email')}</span>{/if}
		</div>
		<div class="field">
			<label for="inv-name">Name</label>
			<input id="inv-name" class="input" bind:value={inv.name} {...faultAttrs('name')} />
			{#if fieldMsg('name')}<span class="field-err" id="inv-err-name">{fieldMsg('name')}</span>{/if}
		</div>
		<div class="field">
			<label for="inv-company">Company (optional)</label>
			<input id="inv-company" class="input" bind:value={inv.company} {...faultAttrs('company')} />
			{#if fieldMsg('company')}<span class="field-err" id="inv-err-company">{fieldMsg('company')}</span>{/if}
		</div>
		<div class="field">
			<label for="inv-tenant">Workspace name (optional)</label>
			<input
				id="inv-tenant"
				class="input"
				bind:value={inv.tenantName}
				placeholder={inv.company || inv.name || 'From the company or name'}
				{...faultAttrs('tenantName')}
			/>
			{#if fieldMsg('tenantName')}<span class="field-err" id="inv-err-tenantName"
					>{fieldMsg('tenantName')}</span
				>{/if}
		</div>
		<div class="field">
			<label for="inv-locale">Email language</label>
			<select id="inv-locale" class="input" bind:value={inv.locale} {...faultAttrs('locale')}>
				<option value="">English (not told)</option>
				{#each SIGNUP_LOCALES as l (l)}<option value={l}>{LOCALE_NAMES[l]} ({l})</option>{/each}
			</select>
			{#if fieldMsg('locale')}<span class="field-err" id="inv-err-locale">{fieldMsg('locale')}</span>{/if}
		</div>
		<p class="note">
			An address with a pending request is approved from that request (the language chosen here wins). An
			address that already has a login this signup did not create is refused.
		</p>
		{#if inviteError && !inviteError.field}{@render faultBox(inviteError)}{/if}
		<div class="row">
			<button class="btn primary" type="submit" disabled={inviting}
				>{inviting ? 'Creating workspace and sending…' : 'Create workspace and send invite'}</button
			>
		</div>
	</form>
</Drawer>

<style>
	.page-head {
		display: flex;
		justify-content: space-between;
		gap: 12px;
		align-items: flex-start;
	}
	.bar {
		margin-bottom: 10px;
	}
	.linkish {
		border: 0;
		background: none;
		padding: 0;
		font: inherit;
		font-weight: 600;
		color: var(--accent);
		cursor: pointer;
		text-align: left;
	}
	.linkish:hover,
	.linkish:focus-visible {
		text-decoration: underline;
	}
	tr.sel {
		background: var(--sunk);
	}
	.nowrap {
		white-space: nowrap;
	}
	.err-text,
	.field-err {
		color: var(--err);
	}
	.field-err {
		font-size: 12px;
	}
	.warn-text {
		color: var(--warn);
	}
	.journal {
		margin: 6px 0 0;
		padding: 0;
		list-style: none;
		display: grid;
		gap: 6px;
		font-size: 13px;
	}
	.journal li {
		display: flex;
		gap: 8px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.raw {
		flex-basis: 100%;
		margin-left: 4px;
	}
	.raw summary {
		cursor: pointer;
		font-size: 12px;
		color: var(--muted);
	}
	.kv {
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 4px 14px;
		margin: 0;
		font-size: 13px;
	}
	.kv dt {
		color: var(--muted);
	}
	.kv dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.msg {
		margin: 0;
		padding: 8px 12px;
		border-left: 3px solid var(--line);
		color: var(--muted);
		white-space: pre-wrap;
	}
	.confirm {
		border-top: 1px solid var(--line);
		padding-top: 12px;
	}
	.plan {
		margin: 6px 0 0;
		padding-left: 20px;
		display: grid;
		gap: 4px;
	}
</style>
