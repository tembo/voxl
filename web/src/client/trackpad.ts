// Trackpad mode for touchscreens (the toolbar's trackpad button): instead of the
// pointer jumping under the finger, the screen works like a laptop's trackpad, so the
// finger never covers what it points at. Sliding moves the pointer (a quick slide goes
// further than a slow one), a tap clicks where it is, holding still grabs (slide to
// drag, lift to drop), a two-finger tap right-clicks, two fingers scroll and a pinch
// zooms (see zoom.ts; the view follows the pointer). Touch browsers show no CSS cursor,
// so the pointer is drawn here, at the same size whatever the zoom.
import { cmd } from '../protocol';
import type { App } from './app';
import { el } from './app';
import type { CursorImage } from './cursor';
import type { createZoom } from './zoom';

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

interface Hooks {
	queueScroll(dx: number, dy: number): void;
	deskPerPx(): number;
	cursor: { image(): CursorImage | null; onChange(f: () => void): void };
	zoom: ReturnType<typeof createZoom>;
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
	// The pointer in desktop pixels; -1 until it's placed in the middle of the desktop.
	const pos = { x: -1, y: -1 };
	let sent = '';
	const pts = new Map<number, { x: number; y: number }>();
	let mode:
		| 'pending'
		| 'move'
		| 'drag'
		| 'two'
		| 'scroll'
		| 'pinch'
		| 'done'
		| null = null;
	let start = { x: 0, y: 0, t: 0 };
	let from = { x: 0, y: 0 };
	let far = 0;
	let lastT = 0;
	let lastMid: [number, number] = [0, 0];
	let timer: ReturnType<typeof setTimeout> | undefined;
	let shown = '';

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
	function sendPos() {
		const s = cmd.move(Math.round(pos.x), Math.round(pos.y));
		if (s === sent) return;
		sent = s;
		send(s);
	}

	function draw() {
		const { w, h } = app.desk;
		if (on && w && pos.x < 0) {
			pos.x = w / 2;
			pos.y = h / 2;
			sendPos();
		}
		ptr.style.display = on && w ? 'block' : 'none';
		if (!on || !w) return;
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
		const s = app.view.scale;
		ptr.style.transform = `translate(${pos.x * s - hx}px, ${pos.y * s - hy}px) scale(${1 / hooks.zoom.z})`;
		ptr.classList.toggle('down', mode === 'drag');
	}
	hooks.cursor.onChange(draw);
	hooks.zoom.onChange(draw);

	/** Moves the pointer by a finger movement (CSS px) made over `dt` ms. */
	function nudge(dx: number, dy: number, dt: number) {
		// Slow slides move the pointer less than the finger, for precision; fast ones more.
		const speed = Math.hypot(dx, dy) / Math.max(dt, 1);
		const gain = Math.min(3, Math.max(0.6, speed * 1.5)) * hooks.deskPerPx();
		pos.x = Math.max(0, Math.min(app.desk.w - 1, pos.x + dx * gain));
		pos.y = Math.max(0, Math.min(app.desk.h - 1, pos.y + dy * gain));
		sendPos();
		draw();
		const r = app.surface.getBoundingClientRect();
		hooks.zoom.reveal(
			r.left + (pos.x / app.desk.w) * r.width,
			r.top + (pos.y / app.desk.h) * r.height
		);
	}

	function set(v: boolean) {
		on = v;
		try {
			localStorage.setItem(KEY, on ? '1' : '0');
		} catch {}
		btn.classList.toggle('on', on);
		btn.setAttribute('aria-pressed', String(on));
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
		} else if (pts.size === 2) {
			clearTimeout(timer);
			if (mode === 'drag') send(cmd.button(1, false));
			mode = mode === 'pending' || mode === 'move' ? 'two' : 'scroll';
			lastMid = mid();
			start = { x: lastMid[0], y: lastMid[1], t: e.timeStamp };
			hooks.zoom.begin(pts.values());
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
		} else if (pts.size === 2) {
			const m = mid();
			if (mode === 'two') {
				const moved = Math.hypot(m[0] - start.x, m[1] - start.y) > TAP_SLOP;
				if (hooks.zoom.pinching(pts.values(), moved)) mode = 'pinch';
				else if (moved) mode = 'scroll';
			}
			if (mode === 'pinch') hooks.zoom.pinch(pts.values());
			else if (mode === 'scroll') {
				const k = hooks.deskPerPx();
				hooks.queueScroll((lastMid[0] - m[0]) * k, (lastMid[1] - m[1]) * k);
				lastMid = m;
			}
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
		} else if (mode === 'two') {
			click(3);
			mode = 'done';
		} else if (mode === 'scroll' || mode === 'pinch' || mode === 'move')
			mode = 'done';
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
			return on;
		},
		down,
		move,
		up,
		cancel,
	};
}
