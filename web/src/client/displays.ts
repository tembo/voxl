// Native selects work with touch, mouse and keyboard without a second menu system.
import { cmd } from '../protocol';
import type { DisplayConfig } from '../protocol';
import type { App } from './app';
import { el } from './app';

export function createDisplays(app: App, status: HTMLElement) {
	const picker = el<HTMLSelectElement>('display-select');
	let config: DisplayConfig | undefined;
	let timer: ReturnType<typeof setTimeout> | undefined;

	function switching() {
		clearTimeout(timer);
		app.switchingDisplay = true;
		picker.disabled = true;
		status.textContent = 'Switching display…';
		status.style.display = '';
		timer = setTimeout(() => {
			status.textContent =
				'Still switching displays… Reconnect if this persists.';
		}, 12000);
	}

	picker.addEventListener('change', () => {
		if (!config || app.readOnly || app.switchingDisplay) return;
		const id = Number(picker.value);
		if (!config.available.some((d) => d.id === id)) return;
		app.send(cmd.releaseAll);
		app.send(cmd.display(id));
		switching();
	});

	function setup(next: DisplayConfig | undefined) {
		clearTimeout(timer);
		config = next;
		app.switchingDisplay = !!next?.switching;
		picker.hidden = !next || next.available.length < 2 || app.readOnly;
		picker.disabled = app.readOnly || app.switchingDisplay;
		picker.replaceChildren();
		for (const display of next?.available || []) {
			const option = document.createElement('option');
			option.value = String(display.id);
			option.textContent = `${display.name}${display.main ? ' · Main' : ''}`;
			option.title = `${display.width} × ${display.height}`;
			picker.append(option);
		}
		if (next) picker.value = String(next.selected);
		if (app.switchingDisplay) switching();
		else status.style.display = 'none';
	}

	function error(message: string) {
		clearTimeout(timer);
		app.switchingDisplay = false;
		picker.disabled = app.readOnly;
		if (config) picker.value = String(config.selected);
		status.textContent = message;
		status.style.display = '';
		timer = setTimeout(() => {
			status.style.display = 'none';
		}, 4000);
	}

	function disconnected() {
		clearTimeout(timer);
		app.switchingDisplay = false;
		picker.disabled = true;
	}

	return { setup, switching, error, disconnected };
}
