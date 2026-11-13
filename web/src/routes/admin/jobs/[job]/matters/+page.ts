import { redirect } from '@sveltejs/kit';
import { MATTERS_ENABLED } from '$lib/features';
import { jobPath } from '$lib/scope';

export const load = ({ params }: { params: { job: string } }) => {
	if (!MATTERS_ENABLED) redirect(307, jobPath(params.job));
};
