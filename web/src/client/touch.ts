// Touch: tap = click, drag = left-drag, long press = right click, two fingers =
// scroll (content follows the fingers). A tap right after another one near it clicks
// the same spot, so double-taps double-click despite the finger landing a little off.
import { cmd } from '../protocol';
import type { App } from './app';

const SLOP = 10;
const LONG_PRESS = 550;
/** Taps this close in time (GTK's and Qt's double-click time) and space hit one spot. */
const MULTI_TAP_MS = 400;
const MULTI_TAP_PX = 24;

interface Hooks {
	/** Move the remote pointer to a client (CSS pixel) position. */
	moveTo(x: number, y: number): void;
	queueScroll(dx: number, dy: number): void;
	deskPerPx(): number;
}

export function createTouch(app: App, hooks: Hooks) {
	const { send } = app;
	const pts = new Map<number, { x: number; y: number }>();
	let mode: 'pending' | 'drag' | 'scroll' | 'done' | null = null;
	let start = { x: 0, y: 0 };
	let timer: ReturnType<typeof setTimeout> | undefined;
	let lastMid: [number, number] = [0, 0];
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
		} else if (pts.size === 2) {
			clearTimeout(timer);
			if (mode === 'drag') send(cmd.button(1, false));
			mode = 'scroll';
			lastMid = mid();
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
		else if (mode === 'scroll' && pts.size >= 2) {
			const m = mid(),
				k = hooks.deskPerPx();
			hooks.queueScroll((lastMid[0] - m[0]) * k, (lastMid[1] - m[1]) * k);
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
		} else if (mode === 'scroll' && pts.size < 2) mode = 'done';
		if (pts.size === 0) mode = null;
	}
	function cancel(e: PointerEvent) {
		clearTimeout(timer);
		if (mode === 'drag') send(cmd.button(1, false));
		pts.delete(e.pointerId);
		if (pts.size === 0) mode = null;
	}
	return { down, move, up, cancel };
}
