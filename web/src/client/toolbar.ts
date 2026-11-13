// Toolbar (top right on hover; bottom right and always visible on touch devices):
// on-screen keyboard, files, microphone and fullscreen buttons.
import { cmd } from '../protocol';
import type { Hello } from '../protocol';
import type { App } from './app';
import { el } from './app';

export function setupToolbar(app: App, onFullscreenChange: () => void) {
	const { tools, kbd } = app;
	const kbBtn = el<HTMLButtonElement>('kb-btn');
	const fsBtn = el<HTMLButtonElement>('fs-btn');
	const displays = el<HTMLSelectElement>('display-select');
	displays.addEventListener('change', () => {
		app.send(cmd.releaseAll);
		app.send(cmd.display(Number(displays.value)));
		displays.disabled = true;
		// Keys go to the desktop again; touch devices skip this to keep the soft keyboard closed.
		if (!tools.classList.contains('touch')) kbd.focus({ preventScroll: true });
	});
	let hideTimer: ReturnType<typeof setTimeout> | undefined;
	function show(ms: number) {
		tools.classList.add('show');
		clearTimeout(hideTimer);
		if (ms) hideTimer = setTimeout(() => tools.classList.remove('show'), ms);
	}
	// Touch devices keep the toolbar visible; mice reveal it near the top edge.
	if (matchMedia('(pointer: coarse)').matches) {
		tools.classList.add('touch');
		show(0);
	} else
		document.addEventListener('pointermove', (e) => {
			if (
				e.pointerType === 'mouse' &&
				e.clientY < 48 &&
				e.clientX > window.innerWidth - 160
			)
				show(2500);
		});
	// Clicks on the toolbar must not reach the desktop surface underneath.
	for (const b of tools.querySelectorAll('button, select'))
		b.addEventListener('pointerdown', (e) => e.stopPropagation());

	kbBtn.addEventListener('click', () => {
		if (document.activeElement === kbd && kbd.dataset.soft) {
			kbd.blur();
			delete kbd.dataset.soft;
		} else {
			kbd.dataset.soft = '1';
			kbd.focus({ preventScroll: true });
		}
	});

	if (!document.fullscreenEnabled) fsBtn.style.display = 'none';
	fsBtn.addEventListener('click', () => {
		if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
		else
			document.documentElement
				.requestFullscreen({ navigationUI: 'hide' })
				.catch(() => {});
	});
	// In fullscreen, capture browser shortcuts (Cmd/Ctrl+W, T, L, ...) for the desktop.
	// Keyboard Lock is Chromium-only and only allowed for the top-level page (an
	// embedding page has to lock on our behalf); hold Esc to leave fullscreen.
	document.addEventListener('fullscreenchange', () => {
		const kl = navigator.keyboard;
		if (document.fullscreenElement) kl?.lock().catch(() => {});
		else kl?.unlock();
		fsBtn.classList.toggle('on', !!document.fullscreenElement);
		onFullscreenChange();
	});
	return (info: Hello['displays']) => {
		displays.hidden = !info || info.available.length < 2 || app.readOnly;
		displays.disabled = false;
		displays.replaceChildren();
		for (const display of info?.available || []) {
			displays.add(new Option(display.name, String(display.id)));
		}
		if (info) displays.value = String(info.selected);
	};
}
