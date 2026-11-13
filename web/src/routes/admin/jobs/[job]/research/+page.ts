import { redirect } from '@sveltejs/kit';
import { RESEARCH_ENABLED } from '$lib/features';
import { jobPath } from '$lib/scope';

export const load = ({ params }: { params: { job: string } }) => {
	if (!RESEARCH_ENABLED) redirect(307, jobPath(params.job));
};
