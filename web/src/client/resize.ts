// Dynamic resolution: ask the server for a desktop that matches this window, so it
// is shown 1:1 instead of being scaled down. Sent on connect and, debounced, when
// the window changes size (sidebar drags, fullscreen, rotating a tablet), and again
// every few seconds so the desktop comes back to it if something else changed it.
//
// A page open on the very screen it shows can't follow its window: the desktop
// shrinks, the window shrinks with it, and that asks for a smaller desktop still,
// down to the smallest mode. Only then does this browser's own screen change size
// right after a resize, so when it does, the page stops following its window, and
// remembers (for this browser) so the next visit doesn't shrink the screen first.
import { cmd } from '../protocol';
import type { App } from './app';

const DEBOUNCE_MS = 250;
/** How often the window's size is sent again while Auto rescale is on. */
const RECHECK_MS = 3000;
/** How soon after asking for a size this screen changing means it's the one shown. */
const SAME_SCREEN_MS = 4000;

const screenSize = () => `${screen.width}x${screen.height}`;
const SAME_SCREEN_KEY = 'voxl.sameScreen';

function remembered(): boolean {
	try {
		return localStorage.getItem(SAME_SCREEN_KEY) === '1';
	} catch {
		return false;
	}
}

export function createResize(app: App, onSameScreen: () => void) {
	let scale = 0;
	let last = '';
	let timer: ReturnType<typeof setTimeout> | undefined;
	// ?scale= overrides the server's desktop-pixels-per-CSS-pixel (e.g. 2 for sharper text).
	const override = Number(new URLSearchParams(location.search).get('scale'));
	let seenScreen = screenSize();
	let askedAt = -Infinity;
	/** This page is on the screen it shows: it no longer follows its window. */
	let sameScreen = remembered();

	function checkScreen() {
		const now = screenSize();
		if (now === seenScreen) return;
		seenScreen = now;
		if (scale && performance.now() - askedAt < SAME_SCREEN_MS) {
			sameScreen = true;
			try {
				localStorage.setItem(SAME_SCREEN_KEY, '1');
			} catch {}
			onSameScreen();
		}
	}

	/** `recheck`: the periodic resend, which the server follows only from the viewer
	 * that last resized (see "sz" in server.rs). */
	function send(recheck = false) {
		checkScreen();
		if (sameScreen || !scale || !window.innerWidth || !window.innerHeight)
			return;
		const s = override > 0 ? override : scale;
		const w = Math.round(window.innerWidth * s);
		const h = Math.round(window.innerHeight * s);
		const key = `${w},${h}`;
		if (key === last && !recheck) return;
		if (key !== last) askedAt = performance.now();
		last = key;
		app.send(cmd.resize(w, h, recheck));
	}

	/** From each hello and settings change: whether to follow the window, at what scale. */
	function setup(cfg: { scale: number } | undefined) {
		scale = cfg?.scale ?? 0;
		last = '';
		// Auto chosen again on the screen it shows: say so again rather than loop.
		if (sameScreen && scale) onSameScreen();
		send();
	}

	window.addEventListener('resize', () => {
		checkScreen();
		clearTimeout(timer);
		timer = setTimeout(send, DEBOUNCE_MS);
	});
	setInterval(() => {
		if (!document.hidden) send(true);
	}, RECHECK_MS);
	return {
		setup,
		/** Whether this page turned out to be on the screen it shows. */
		get sameScreen() {
			return sameScreen;
		},
	};
}
