// Trackpad mode for touchscreens (the toolbar's trackpad button): instead of the
// pointer jumping under the finger, the screen works like a laptop's trackpad, so the
// finger never covers what it points at. Sliding moves the pointer (a quick slide goes
// further than a slow one), a tap clicks where it is, holding still grabs (slide to
// drag, lift to drop) and a two-finger tap right-clicks. Pinch and two-finger scroll
// work as with direct touch (touch.ts), and a zoomed-in view follows the pointer.
// Touch browsers show no CSS cursor, so the pointer is drawn here.
import { cmd } from '../protocol';
import type { App } from './app';
import { el } from './app';
import type { CursorImage } from './cursor';
import type { Zoom } from './zoom';

/** Moving less than this (CSS px) keeps a touch a tap or a hold. */
const STILL = 4;
/** A touch that moved less than this and lifted within TAP_MS still counts as a tap. */
const TAP_SLOP = 10;
const TAP_MS = 200;
/** Holding still this long grabs (presses the left button). */
const HOLD = 400;
const KEY = 'voxl.trackpad';
/** Size (CSS px) of the dot drawn when the desktop hides its cursor; see #tp-pointer.dot. */
const DOT = 12;
/** A zoomed-in view pans to keep the pointer at least this far (CSS px) inside the window. */
const EDGE = 32;

interface Hooks {
	queueScroll(dx: number, dy: number): void;
	deskPerPx(): number;
	cursor: { image(): CursorImage | null; onChange(f: () => void): void };
	zoom: Zoom;
}

export function createTrackpad(app: App, hooks: Hooks) {
	const { send } = app;
	const btn = el<HTMLButtonElement>('tp-btn');
	const ptr = document.createElement('div');
	ptr.id = 'tp-pointer';
	app.surface.appendChild(ptr);

	let on = false;
	try {
		on = localStorage.getItem(KEY) === '1';
	} catch {}
	// Read-only viewers have no pointer to move (and no button to turn this off).
	const active = () => on && !app.readOnly;
	// The pointer in desktop pixels; -1 until it's placed in the middle of the desktop.
	const pos = { x: -1, y: -1 };
	let sent = '';
	const pts = new Map<number, { x: number; y: number }>();
	/** 'two': two fingers down, not yet moved enough to tell a tap, scroll or pinch apart. */
	let mode:
		| 'pending'
		| 'move'
		| 'drag'
		| 'two'
		| 'scroll'
		| 'pinch'
		| 'done'
		| null = null;
	/** Lifting two fingers that neither scrolled nor pinched right-clicks. */
	let rightTap = false;
	let start = { x: 0, y: 0, t: 0 };
	let from = { x: 0, y: 0 };
	let far = 0;
	let lastT = 0;
	let lastMid: [number, number] = [0, 0];
	let startMid: [number, number] = [0, 0];
	let startSpread = 0;
	let timer: ReturnType<typeof setTimeout> | undefined;
	// The cursor image last drawn; undefined so the first draw sets it (or the dot).
	let shown: string | undefined;

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
	/** Moves the remote pointer to `pos`; `force` sends it even if it was the last sent,
	 * as direct touch, a mouse or another viewer may have moved the pointer since. */
	function sendPos(force = false) {
		if (pos.x < 0) return;
		const s = cmd.move(Math.round(pos.x), Math.round(pos.y));
		if (s === sent && !force) return;
		sent = s;
		send(s);
	}

	function draw() {
		const { w, h } = app.desk;
		const show = active() && w > 0;
		if (show && pos.x < 0) {
			pos.x = w / 2;
			pos.y = h / 2;
			sendPos();
		}
		ptr.style.display = show ? 'block' : 'none';
		if (!show) return;
		pos.x = Math.min(pos.x, w - 1);
		pos.y = Math.min(pos.y, h - 1);
		// A hidden remote cursor still gets a dot, or the pointer would be lost.
		const img = hooks.cursor.image();
		if ((img?.url || '') !== shown) {
			shown = img?.url || '';
			ptr.classList.toggle('dot', !img);
			ptr.style.backgroundImage = img ? `url(${img.url})` : '';
			ptr.style.width = img ? `${img.w}px` : '';
			ptr.style.height = img ? `${img.h}px` : '';
		}
		const hx = img ? img.hx : DOT / 2,
			hy = img ? img.hy : DOT / 2;
		ptr.style.setProperty('--hx', `${hx}px`);
		ptr.style.setProperty('--hy', `${hy}px`);
		// The surface is scaled by pinch zoom; keep the pointer its usual size.
		const s = app.view.scale;
		ptr.style.transform = `translate(${pos.x * s - hx}px, ${pos.y * s - hy}px) scale(${1 / hooks.zoom.scale()})`;
		ptr.classList.toggle('down', mode === 'drag');
	}
	hooks.cursor.onChange(draw);

	/** Pans a zoomed-in view so the pointer stays at least EDGE inside the window. */
	function follow() {
		if (hooks.zoom.scale() === 1) return;
		const r = app.surface.getBoundingClientRect();
		const off = (v: number, size: number) =>
			v < EDGE ? EDGE - v : v > size - EDGE ? size - EDGE - v : 0;
		hooks.zoom.pan(
			off(r.left + (pos.x * r.width) / app.desk.w, window.innerWidth),
			off(r.top + (pos.y * r.height) / app.desk.h, window.innerHeight)
		);
	}

	/** Moves the pointer by a finger movement (CSS px) made over `dt` ms. */
	function nudge(dx: number, dy: number, dt: number) {
		// Slow slides move the pointer less than the finger, for precision; fast ones more.
		const speed = Math.hypot(dx, dy) / Math.max(dt, 1);
		const gain = Math.min(3, Math.max(0.6, speed * 1.5)) * hooks.deskPerPx();
		pos.x = Math.max(0, Math.min(app.desk.w - 1, pos.x + dx * gain));
		pos.y = Math.max(0, Math.min(app.desk.h - 1, pos.y + dy * gain));
		sendPos();
		follow();
		draw();
	}

	function set(v: boolean) {
		on = v;
		try {
			localStorage.setItem(KEY, on ? '1' : '0');
		} catch {}
		btn.classList.toggle('on', on);
		btn.setAttribute('aria-pressed', String(on));
		if (active()) sendPos(true);
		draw();
	}
	btn.addEventListener('click', () => set(!on));
	set(on);

	function down(e: PointerEvent) {
		try {
			app.surface.setPointerCapture(e.pointerId);
		} catch {}
		pts.set(e.pointerId, { x: e.clientX, y: e.clientY });
		lastT = e.timeStamp;
		if (pts.size === 1) {
			sendPos(true);
			mode = 'pending';
			start = { x: e.clientX, y: e.clientY, t: e.timeStamp };
			from = { x: pos.x, y: pos.y };
			far = 0;
			timer = setTimeout(() => {
				if (mode !== 'pending') return;
				mode = 'drag';
				send(cmd.button(1, true));
				navigator.vibrate?.(10);
				draw();
			}, HOLD);
		} else {
			if (pts.size === 2) {
				clearTimeout(timer);
				if (mode === 'drag') send(cmd.button(1, false));
				rightTap = mode === 'pending' || mode === 'move';
				mode = 'two';
			}
			rebase();
			draw();
		}
	}
	function move(e: PointerEvent) {
		const p = pts.get(e.pointerId);
		if (!p) return;
		const dx = e.clientX - p.x,
			dy = e.clientY - p.y,
			dt = e.timeStamp - lastT;
		p.x = e.clientX;
		p.y = e.clientY;
		lastT = e.timeStamp;
		if (pts.size === 1) {
			far = Math.max(far, Math.hypot(p.x - start.x, p.y - start.y));
			if (mode === 'pending' && far > STILL) {
				clearTimeout(timer);
				mode = 'move';
				nudge(p.x - start.x, p.y - start.y, e.timeStamp - start.t);
			} else if (mode === 'move' || mode === 'drag') nudge(dx, dy, dt);
			return;
		}
		if (mode === 'two') {
			// As in touch.ts: a pinch changes the fingers' gap faster than it moves
			// their centroid; a scroll moves them together.
			const [mx, my] = mid();
			const moved = Math.hypot(mx - startMid[0], my - startMid[1]);
			const gap = 2 * Math.abs(spread() - startSpread);
			if (gap > TAP_SLOP && gap > moved) {
				mode = 'pinch';
				rebase();
			} else if (moved > TAP_SLOP) mode = 'scroll';
		}
		if (mode === 'pinch') {
			hooks.zoom.pinch(...mid(), spread());
			draw();
		} else if (mode === 'scroll') {
			// The content follows the fingers: a zoomed-in view moves, then the desktop scrolls.
			const m = mid(),
				k = hooks.deskPerPx();
			const [rx, ry] = hooks.zoom.pan(m[0] - lastMid[0], m[1] - lastMid[1]);
			hooks.queueScroll(-rx * k, -ry * k);
			lastMid = m;
		}
	}
	function up(e: PointerEvent) {
		if (!pts.delete(e.pointerId)) return;
		clearTimeout(timer);
		if (mode === 'pending') {
			click(1);
			mode = 'done';
		} else if (
			mode === 'move' &&
			far < TAP_SLOP &&
			e.timeStamp - start.t < TAP_MS
		) {
			// A tap that wobbled: undo the wobble and click where the pointer was.
			pos.x = from.x;
			pos.y = from.y;
			sendPos();
			click(1);
			mode = 'done';
		} else if (mode === 'drag') {
			send(cmd.button(1, false));
			mode = 'done';
		} else if (mode === 'two' || mode === 'scroll' || mode === 'pinch') {
			if (pts.size >= 2) rebase();
			else {
				if (mode === 'two' && rightTap) click(3);
				mode = 'done';
			}
		} else if (mode === 'move') mode = 'done';
		if (pts.size === 0) mode = null;
		draw();
	}
	function cancel(e: PointerEvent) {
		clearTimeout(timer);
		if (mode === 'drag') send(cmd.button(1, false));
		pts.delete(e.pointerId);
		mode = pts.size ? 'done' : null;
		draw();
	}

	return {
		get on() {
			return active();
		},
		down,
		move,
		up,
		cancel,
	};
}
