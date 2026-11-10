// Touch: tap = click, drag = left-drag, long press = right click, pinch = zoom the
// view in or out, two fingers = scroll. The content follows the fingers: two fingers
// move a zoomed-in view, and scroll the desktop once the view reaches its edge.
import { cmd } from '../protocol';
import type { App } from './app';
import type { Zoom } from './zoom';

const SLOP = 10;
const LONG_PRESS = 550;
/** A tap this soon (GTK's and Qt's double-click time) and this close (CSS px) after
 * the last one clicks the same spot, so double-taps double-click even though the
 * finger lands a little off. */
const MULTI_TAP_MS = 400;
const MULTI_TAP_PX = 24;

interface Hooks {
	/** Move the remote pointer to a client (CSS pixel) position. */
	moveTo(x: number, y: number): void;
	queueScroll(dx: number, dy: number): void;
	deskPerPx(): number;
	zoom: Zoom;
}

export function createTouch(app: App, hooks: Hooks) {
	const { send } = app;
	const pts = new Map<number, { x: number; y: number }>();
	/** 'two': two fingers down, not yet moved enough to tell a scroll from a pinch. */
	let mode: 'pending' | 'drag' | 'two' | 'scroll' | 'pinch' | 'done' | null =
		null;
	let start = { x: 0, y: 0 };
	let timer: ReturnType<typeof setTimeout> | undefined;
	let lastMid: [number, number] = [0, 0];
	let startMid: [number, number] = [0, 0];
	let startSpread = 0;
	let lastTap = { x: 0, y: 0, t: -Infinity };
	const mid = (): [number, number] => {
		let x = 0,
			y = 0;
		for (const p of pts.values()) {
			x += p.x;
			y += p.y;
		}
		return [x / pts.size, y / pts.size];
	};
	/** Mean distance of the fingers from their centroid (half the gap for two). */
	const spread = () => {
		const [mx, my] = mid();
		let d = 0;
		for (const p of pts.values()) d += Math.hypot(p.x - mx, p.y - my);
		return d / pts.size;
	};
	/** Measure the gesture from where the fingers are now (they were added or lifted). */
	const rebase = () => {
		startMid = lastMid = mid();
		startSpread = spread();
		if (mode === 'pinch') hooks.zoom.begin(...startMid, startSpread);
	};
	const click = (button: number) => {
		send(cmd.button(button, true));
		send(cmd.button(button, false));
	};

	function down(e: PointerEvent) {
		try {
			app.surface.setPointerCapture(e.pointerId);
		} catch {}
		pts.set(e.pointerId, { x: e.clientX, y: e.clientY });
		if (pts.size === 1) {
			mode = 'pending';
			start = { x: e.clientX, y: e.clientY };
			timer = setTimeout(() => {
				if (mode !== 'pending') return;
				hooks.moveTo(start.x, start.y);
				click(3);
				mode = 'done';
				navigator.vibrate?.(10);
			}, LONG_PRESS);
		} else {
			if (pts.size === 2) {
				clearTimeout(timer);
				if (mode === 'drag') send(cmd.button(1, false));
				mode = 'two';
			}
			rebase();
		}
	}
	function move(e: PointerEvent) {
		const p = pts.get(e.pointerId);
		if (!p) return;
		p.x = e.clientX;
		p.y = e.clientY;
		if (mode === 'pending' && Math.hypot(p.x - start.x, p.y - start.y) > SLOP) {
			clearTimeout(timer);
			mode = 'drag';
			hooks.moveTo(start.x, start.y);
			send(cmd.button(1, true));
		}
		if (mode === 'drag') hooks.moveTo(p.x, p.y);
		if (pts.size < 2) return;
		if (mode === 'two') {
			// A pinch changes the fingers' gap faster than it moves their centroid
			// (twice as fast with one finger still); a scroll moves them together.
			const [mx, my] = mid();
			const moved = Math.hypot(mx - startMid[0], my - startMid[1]);
			const gap = 2 * Math.abs(spread() - startSpread);
			if (gap > SLOP && gap > moved) {
				mode = 'pinch';
				rebase();
			} else if (moved > SLOP) mode = 'scroll';
		}
		if (mode === 'pinch') hooks.zoom.pinch(...mid(), spread());
		else if (mode === 'scroll') {
			const m = mid(),
				k = hooks.deskPerPx();
			const [rx, ry] = hooks.zoom.pan(m[0] - lastMid[0], m[1] - lastMid[1]);
			hooks.queueScroll(-rx * k, -ry * k);
			lastMid = m;
		}
	}
	function up(e: PointerEvent) {
		if (!pts.delete(e.pointerId)) return;
		if (mode === 'pending') {
			clearTimeout(timer);
			const now = performance.now();
			const at =
				now - lastTap.t < MULTI_TAP_MS &&
				Math.hypot(start.x - lastTap.x, start.y - lastTap.y) < MULTI_TAP_PX
					? lastTap
					: start;
			lastTap = { x: at.x, y: at.y, t: now };
			hooks.moveTo(at.x, at.y);
			click(1);
			mode = 'done';
		} else if (mode === 'drag') {
			send(cmd.button(1, false));
			mode = 'done';
		} else if (mode === 'two' || mode === 'scroll' || mode === 'pinch') {
			if (pts.size < 2) mode = 'done';
			else rebase();
		}
		if (pts.size === 0) mode = null;
	}
	function cancel(e: PointerEvent) {
		clearTimeout(timer);
		if (mode === 'drag') send(cmd.button(1, false));
		pts.delete(e.pointerId);
		mode = pts.size ? 'done' : null;
	}
	return { down, move, up, cancel };
}
