// Toast state. Named toaster.svelte.ts (not toast.svelte.ts): on case-insensitive
// macOS, './toast.svelte' resolves to Toast.svelte.
export type ToastKind = 'ok' | 'err';
export const toastState = $state<{ msg: string; visible: boolean; kind: ToastKind }>({
	msg: '',
	visible: false,
	kind: 'ok',
});
let timer: ReturnType<typeof setTimeout> | undefined;
/** Shows a transient message. `kind: 'err'` is announced assertively and stays a little longer. */
export function toast(msg: string, kind: ToastKind = 'ok') {
	toastState.msg = msg;
	toastState.kind = kind;
	toastState.visible = true;
	clearTimeout(timer);
	timer = setTimeout(() => (toastState.visible = false), kind === 'err' ? 5000 : 2600);
}
