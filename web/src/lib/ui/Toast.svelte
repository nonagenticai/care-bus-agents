<script lang="ts">
	import { toastState } from './toaster.svelte';
</script>

<!-- Two live regions so the role never flips on a mounted node (screen readers ignore role changes). -->
<div class="toast" role="status" hidden={!toastState.visible || toastState.kind === 'err'}>
	{toastState.kind === 'err' ? '' : toastState.msg}
</div>
<div class="toast err" role="alert" hidden={!toastState.visible || toastState.kind !== 'err'}>
	{toastState.kind === 'err' ? toastState.msg : ''}
</div>
