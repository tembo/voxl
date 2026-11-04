// Pinch to zoom on touchscreens. Spreading or closing two fingers zooms the view of the
// desktop (not the desktop itself) around them, from fitting the window up to two
// on-screen pixels per desktop pixel, and moving them while pinching pans. Two fingers
// moving together still scroll the desktop. The view is a CSS transform on the
// surface, so mapping touches to desktop pixels (getBoundingClientRect) follows it.
import type { App } from './app';

/** Fingers spreading or closing this much (CSS px) make a two-finger gesture a pinch. */
const PINCH_SLOP = 16;
/** Most on-screen CSS pixels per desktop pixel. */
const MAX_PX = 2;
/** reveal() keeps a point at least this far (CSS px) inside the window. */
const MARGIN = 32;

type Pt = { x: number; y: number };

const two = (pts: Iterable<Pt>): [Pt, Pt] => {
	const [a, b] = pts;
	return [a, b];
};
const spreadOf = ([a, b]: [Pt, Pt]) => Math.hypot(a.x - b.x, a.y - b.y);
const midOf = ([a, b]: [Pt, Pt]) => ({
	x: (a.x + b.x) / 2,
	y: (a.y + b.y) / 2,
});

/** Keeps the zoomed desktop covering the window (or centered where it's smaller). */
function clampAxis(t: number, base: number, size: number, view: number) {
	if (size <= view) return (view - size) / 2 - base;
	return Math.min(-base, Math.max(view - size - base, t));
}

export function createZoom(app: App) {
	const { surface } = app;
	let z = 1,
		tx = 0,
		ty = 0;
	// The gesture's start: finger spread, zoom, and the desktop point (in unzoomed
	// surface px) under the fingers' midpoint. Each pinch works from these rather than
	// from the last (clamped) view, so the point stays under the fingers.
	let spread0 = 0,
		z0 = 1;
	let anchor: Pt = { x: 0, y: 0 };
	const listeners: (() => void)[] = [];

	function apply() {
		if (z < 1.02) {
			z = 1;
			tx = ty = 0;
		} else {
			tx = clampAxis(
				tx,
				surface.offsetLeft,
				surface.offsetWidth * z,
				window.innerWidth
			);
			ty = clampAxis(
				ty,
				surface.offsetTop,
				surface.offsetHeight * z,
				window.innerHeight
			);
		}
		surface.style.transform =
			z === 1 ? '' : `translate(${tx}px, ${ty}px) scale(${z})`;
		for (const f of listeners) f();
	}
	function reset() {
		if (z === 1) return;
		z = 1;
		apply();
	}
	// A new layout (rotation, resize, fullscreen, a new desktop size) starts unzoomed.
	new ResizeObserver(reset).observe(surface);
	window.addEventListener('resize', reset);

	return {
		get z() {
			return z;
		},
		/** Starts tracking a two-finger gesture. */
		begin(pts: Iterable<Pt>) {
			const p = two(pts);
			const m = midOf(p);
			spread0 = spreadOf(p);
			z0 = z;
			anchor = {
				x: (m.x - surface.offsetLeft - tx) / z,
				y: (m.y - surface.offsetTop - ty) / z,
			};
		},
		/** Whether the fingers have spread or closed enough since begin() to pinch. Once
		 * they've also `moved` half as much is enough, so a pinch that drifts sideways
		 * isn't taken for a scroll. */
		pinching(pts: Iterable<Pt>, moved: boolean) {
			const d = Math.abs(spreadOf(two(pts)) - spread0);
			return d > PINCH_SLOP || (moved && d > PINCH_SLOP / 2);
		},
		/** Zooms by how much the fingers spread since begin(), keeping the desktop point
		 * that was under them under their midpoint (so moving them pans). */
		pinch(pts: Iterable<Pt>) {
			const p = two(pts);
			const m = midOf(p);
			const max = Math.max(1, MAX_PX / app.view.scale);
			z = Math.min(max, Math.max(1, (z0 * spreadOf(p)) / (spread0 || 1)));
			tx = m.x - surface.offsetLeft - z * anchor.x;
			ty = m.y - surface.offsetTop - z * anchor.y;
			apply();
		},
		/** Pans, when zoomed in, so a point (client CSS px) is inside the window. */
		reveal(x: number, y: number) {
			if (z === 1) return;
			const vw = window.innerWidth,
				vh = window.innerHeight;
			const dx =
				x < MARGIN ? MARGIN - x : x > vw - MARGIN ? vw - MARGIN - x : 0;
			const dy =
				y < MARGIN ? MARGIN - y : y > vh - MARGIN ? vh - MARGIN - y : 0;
			if (!dx && !dy) return;
			tx += dx;
			ty += dy;
			apply();
		},
		onChange(f: () => void) {
			listeners.push(f);
		},
	};
}
