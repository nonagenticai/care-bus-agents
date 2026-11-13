export function debounce<A extends unknown[]>(fn: (...a: A) => void, ms: number): (...a: A) => void {
	let t: ReturnType<typeof setTimeout> | undefined;
	return (...a: A) => {
		clearTimeout(t);
		t = setTimeout(() => fn(...a), ms);
	};
}
