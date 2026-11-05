// Settings panel (the toolbar's cog). Up top, the few choices most people want: a
// quality level, smoothness, sound, and what may cross between this device and the
// desktop. Everything voxl can tune while it runs (src/settings.rs) sits under
// Advanced. A server setting changes for everyone watching and lasts until voxl
// restarts; the server checks every value and sends all viewers the result.
import { cmd } from '../protocol';
import type { SettingsMessage } from '../protocol';
import type { App } from './app';
import { el } from './app';

type Limits = SettingsMessage['limits'];
type Values = SettingsMessage['values'];
type Value = number | string | boolean;

/** Quality levels, sharpest first: what each sets. High is voxl's defaults. */
const LEVELS = [
	{
		name: 'Ultra',
		hint: 'Near lossless, for fast networks',
		crf: 12,
		max_kbps: 80000,
		min_kbps: 4000,
		paintover_crf: 8,
	},
	{
		name: 'High',
		hint: 'Sharp text and smooth motion',
		crf: 24,
		max_kbps: 30000,
		min_kbps: 2000,
		paintover_crf: 16,
	},
	{
		name: 'Medium',
		hint: 'Lighter on bandwidth, still readable text',
		crf: 30,
		max_kbps: 8000,
		min_kbps: 1000,
		paintover_crf: 21,
	},
	{
		name: 'Low',
		hint: 'For slow or busy connections: softer, but keeps up',
		crf: 36,
		max_kbps: 3000,
		min_kbps: 500,
		paintover_crf: 28,
	},
] as const;
type LevelKey = 'crf' | 'max_kbps' | 'min_kbps' | 'paintover_crf';
/** Sent in this order so the bitrate floor never passes the ceiling on the way. */
const LEVEL_KEYS: LevelKey[] = ['crf', 'paintover_crf', 'max_kbps', 'min_kbps'];

/** Auto: voxl's rate control picks the quality (src/video.rs, RateCtl). */
const AUTO_HINT =
	'Adjusts to your connection: sharper while there’s room, lighter as soon as latency rises';

/** The picker's segment for these values: 0 for Auto, 1-4 a level (Ultra first), -1 custom. */
const level = (v: Values) => {
	if (v.auto_quality === true) return 0;
	const i = LEVELS.findIndex((l) =>
		LEVEL_KEYS.every((k) => Number(v[k]) === l[k])
	);
	return i < 0 ? -1 : i + 1;
};

/** The Auto choice in the Smoothness and Sound menus. */
const AUTO = 'auto';
const FRAMERATES: [string, string][] = [
	['30', '30 fps'],
	['60', '60 fps'],
	['120', '120 fps'],
];
const AUDIO: [string, string][] = [
	['64000', 'Standard · 64 kbps'],
	['128000', 'High · 128 kbps'],
	['256000', 'Studio · 256 kbps'],
];
const PRESETS = [
	'ultrafast',
	'superfast',
	'veryfast',
	'faster',
	'fast',
	'medium',
].map((p): [string, string] => [p, p[0].toUpperCase() + p.slice(1)]);

/** Familiar names for common sizes. */
const SIZE_NAMES: Record<string, string> = {
	'1280x720': 'HD',
	'1920x1080': 'Full HD',
	'2560x1440': 'QHD',
	'3840x2160': '4K',
};
const sizeLabel = (s: string) => {
	const [w, ht] = s.split('x');
	const name = SIZE_NAMES[s];
	return `${w} × ${ht}${name ? ` · ${name}` : ''}`;
};

/** An Advanced stepper: limits and units as shown, and the server's units. */
interface Num {
	key: string;
	label: string;
	hint?: string;
	min: number;
	max: number | ((l: Limits) => number);
	step: number;
	unit?: string;
	/** Value shown for 0 ("No limit"). */
	zero?: string;
	show?: (l: Limits) => boolean;
	/** Server units -> shown, and back. */
	view?: (v: number) => number;
	wire?: (v: number) => number;
}

const mbps = (key: string, label: string, hint: string): Num => ({
	key,
	label,
	hint,
	min: 0.5,
	max: 200,
	step: 0.5,
	unit: 'Mbps',
	view: (v) => v / 1000,
	wire: (v) => Math.round(v * 1000),
});

const ADVANCED: { title: string; items: (Num | Pick)[] }[] = [
	{
		title: 'Stream',
		items: [
			{
				key: 'crf',
				label: 'Motion quality',
				hint: 'x264 CRF: lower is sharper',
				min: 0,
				max: 51,
				step: 1,
			},
			mbps('max_kbps', 'Max bitrate', 'Ceiling the stream adapts under'),
			mbps('min_kbps', 'Min bitrate', 'Floor it never drops below'),
		],
	},
	{
		title: 'Sharpening when still',
		items: [
			{
				key: 'paintover_crf',
				label: 'Still quality',
				hint: 'CRF still areas are redrawn at',
				min: 0,
				max: 51,
				step: 1,
			},
			{
				key: 'paintover_burst',
				label: 'Passes',
				min: 0,
				max: 10,
				step: 1,
			},
			{
				key: 'paintover_delay_ms',
				label: 'Wait first',
				hint: 'How long the screen must be still',
				min: 0,
				max: 2000,
				step: 50,
				unit: 'ms',
			},
		],
	},
	{
		title: 'Encoder',
		items: [
			{
				key: 'preset',
				label: 'Preset',
				hint: 'Slower uses more CPU for the same quality',
				options: PRESETS,
				restart: true,
			},
			{
				key: 'encoder_threads',
				label: 'Threads',
				min: 1,
				max: 32,
				step: 1,
				restart: true,
			} as Num & { restart: boolean },
			{
				key: 'adaptive_threads',
				label: 'Adaptive threads',
				hint: 'One thread until motion needs more',
				toggle: true,
				restart: true,
			},
			{
				key: 'stripes',
				label: 'Stripes',
				hint: 'Encode in bands (experimental)',
				min: 1,
				max: 16,
				step: 1,
				restart: true,
			} as Num & { restart: boolean },
			{
				key: 'convert_threads',
				label: 'Color conversion threads',
				min: 1,
				max: 32,
				step: 1,
			},
			{
				key: 'coalesce_us',
				label: 'Batch changes',
				hint: 'Wait to merge screen changes into one frame',
				min: 0,
				max: 20000,
				step: 500,
				unit: 'µs',
			},
		],
	},
	{
		title: 'Files and recording',
		items: [
			{
				key: 'max_upload_mb',
				label: 'Largest upload',
				min: 0,
				max: (l) => l.max_upload_mb || 100000,
				step: 50,
				unit: 'MB',
				zero: 'No limit',
				show: (l) => l.upload,
			},
			{
				key: 'max_record_seconds',
				label: 'Longest recording',
				hint: 'For agents using /record',
				min: 10,
				max: (l) => l.max_record_seconds,
				step: 10,
				unit: 's',
			},
		],
	},
];

/** An Advanced dropdown or switch. */
interface Pick {
	key: string;
	label: string;
	hint?: string;
	options?: [string, string][];
	toggle?: boolean;
	restart?: boolean;
	show?: (l: Limits) => boolean;
}

const isNum = (i: Num | Pick): i is Num => 'step' in i;

const STATS_KEY = 'voxl.stats';

function load(key: string): boolean {
	try {
		return localStorage.getItem(key) === '1';
	} catch {
		return false;
	}
}
function save(key: string, on: boolean) {
	try {
		if (on) localStorage.setItem(key, '1');
		else localStorage.removeItem(key);
	} catch {}
}

/** This viewer's stats overlay preference. */
export const savedStats = () => load(STATS_KEY);

const ICON_MINUS =
	'<svg class="i" viewBox="0 0 24 24" fill="none"><path d="M6 12H18" stroke="currentColor" stroke-width="2" stroke-linecap="round"/></svg>';
const ICON_PLUS =
	'<svg class="i" viewBox="0 0 24 24" fill="none"><path d="M12 6V18M6 12H18" stroke="currentColor" stroke-width="2" stroke-linecap="round"/></svg>';
const ICON_CHEVRON =
	'<svg class="i" viewBox="0 0 24 24" fill="none"><path d="M9 6L15 12L9 18" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>';

function h<K extends keyof HTMLElementTagNameMap>(
	tag: K,
	cls?: string,
	text?: string
): HTMLElementTagNameMap[K] {
	const e = document.createElement(tag);
	if (cls) e.className = cls;
	if (text !== undefined) e.textContent = text;
	return e;
}

/** A row: label (and hint) on the left, its control on the right, an error below. */
function row(label: string, hint?: string, restart = false) {
	const r = h('div', 's-row');
	const text = h('div', 's-text');
	text.append(h('span', 's-label', label));
	const note = [hint, restart && 'Restarts the stream'].filter(Boolean);
	if (note.length) text.append(h('span', 's-hint', note.join(' · ')));
	const ctl = h('div', 's-ctl');
	const err = h('div', 's-err');
	err.setAttribute('role', 'alert');
	r.append(text, ctl, err);
	return { r, ctl, err };
}

/** A control the panel can refresh from the server's values. */
interface Control {
	err: HTMLElement;
	set(v: Value): void;
	disable(off: boolean): void;
}

function toggle(label: string, onChange: (on: boolean) => void) {
	const input = h('input', 's-switch');
	input.type = 'checkbox';
	input.setAttribute('role', 'switch');
	input.setAttribute('aria-label', label);
	input.addEventListener('change', () => onChange(input.checked));
	return input;
}

function select(
	label: string,
	options: [string, string][],
	onChange: (v: string) => void
) {
	const wrap = h('div', 's-select');
	const input = h('select');
	input.setAttribute('aria-label', label);
	for (const [v, text] of options) {
		const o = h('option', undefined, text);
		o.value = v;
		input.append(o);
	}
	input.addEventListener('change', () => onChange(input.value));
	wrap.append(input);
	wrap.insertAdjacentHTML(
		'beforeend',
		'<svg class="i" viewBox="0 0 24 24" fill="none" aria-hidden="true"><path d="M7 10L12 15L17 10" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>'
	);
	return {
		wrap,
		input,
		set(v: string) {
			// A value set by a flag that isn't in the list still shows.
			if (![...input.options].some((o) => o.value === v)) {
				const o = h('option', undefined, v);
				o.value = v;
				input.append(o);
			}
			input.value = v;
		},
	};
}

/**
 * − value + stepper. Holding a button repeats; the value can also be typed. The
 * server hears the value once clicking stops, so a burst of clicks is one change.
 */
function stepper(n: Num, limits: Limits, send: (v: number) => void) {
	const view = n.view ?? ((v: number) => v);
	const wire = n.wire ?? ((v: number) => v);
	const max = typeof n.max === 'function' ? n.max(limits) : n.max;
	const box = h('div', 's-step');
	const dec = h('button', 'ib sm');
	const inc = h('button', 'ib sm');
	dec.type = inc.type = 'button';
	dec.innerHTML = ICON_MINUS;
	inc.innerHTML = ICON_PLUS;
	dec.setAttribute('aria-label', `Decrease ${n.label}`);
	inc.setAttribute('aria-label', `Increase ${n.label}`);
	const field = h('input', 's-step-val');
	field.type = 'text';
	field.inputMode = 'decimal';
	field.setAttribute('aria-label', n.label);
	const unit = h('span', 's-unit', n.unit ?? '');
	const mid = h('label', 's-step-mid');
	mid.append(field, unit);
	box.append(dec, mid, inc);

	let value = 0;
	let pending = false;
	let timer: ReturnType<typeof setTimeout> | undefined;
	const round = (v: number) => Math.round(v / n.step) * n.step;
	const clamp = (v: number) => Math.min(max, Math.max(n.min, v));
	const decimals = String(n.step).split('.')[1]?.length ?? 0;

	function show() {
		const zero = n.zero && value === 0;
		field.value = zero ? n.zero! : value.toFixed(decimals);
		unit.style.display = zero || !n.unit ? 'none' : '';
		dec.disabled = field.disabled || value <= n.min;
		inc.disabled = field.disabled || value >= max;
	}
	function commit() {
		clearTimeout(timer);
		pending = true;
		timer = setTimeout(() => {
			send(wire(value));
			pending = false;
		}, 350);
	}
	function nudge(dir: number) {
		value = clamp(round(value + dir * n.step));
		show();
		commit();
	}
	// Hold to repeat, speeding up a little.
	for (const [b, dir] of [
		[dec, -1],
		[inc, 1],
	] as const) {
		let rep: ReturnType<typeof setTimeout> | undefined;
		const stop = () => clearTimeout(rep);
		b.addEventListener('pointerdown', (e) => {
			if (b.disabled || e.button !== 0) return;
			e.preventDefault();
			nudge(dir);
			let wait = 400;
			const again = () => {
				nudge(dir);
				wait = Math.max(60, wait * 0.8);
				rep = setTimeout(again, wait);
			};
			rep = setTimeout(again, wait);
		});
		for (const ev of ['pointerup', 'pointerleave', 'pointercancel'])
			b.addEventListener(ev, stop);
		// Keyboard: Enter and Space click.
		b.addEventListener('click', (e) => {
			if (e.detail === 0) nudge(dir);
		});
	}
	field.addEventListener('focus', () => field.select());
	field.addEventListener('keydown', (e) => {
		if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
			e.preventDefault();
			nudge(e.key === 'ArrowUp' ? 1 : -1);
		} else if (e.key === 'Enter') field.blur();
		else if (e.key === 'Escape') {
			show();
			field.blur();
		}
	});
	field.addEventListener('change', () => {
		const typed = field.value.trim().toLowerCase();
		const x = n.zero && typed === n.zero.toLowerCase() ? 0 : parseFloat(typed);
		if (Number.isFinite(x)) {
			value = clamp(round(x));
			commit();
		}
		show();
	});

	const c: Control & { el: HTMLElement } = {
		el: box,
		err: h('div'),
		set(v) {
			// Don't undo what is being typed or about to be sent.
			if (pending || document.activeElement === field) return;
			value = Math.round(view(Number(v)) * 1000) / 1000;
			show();
		},
		disable(off) {
			field.disabled = off;
			show();
		},
	};
	return c;
}

export function createSettings(
	app: App,
	opts: {
		/** The Files panel shares the right side: only one is open. */
		closeFiles: () => void;
		onStats: (on: boolean) => void;
	}
) {
	const panel = el('settings');
	const list = el('settings-list');
	const note = el('settings-note');
	const btn = el<HTMLButtonElement>('settings-btn');

	let msg: SettingsMessage | null = null;
	/** Rebuilt only when what's allowed changes, so controls keep their state. */
	let built = '';
	const controls = new Map<string, Control>();
	let levelButtons: HTMLButtonElement[] = [];
	let levelHint: HTMLElement | null = null;
	/** The desktop's size, from the latest hello. */
	let desk = '';
	let resolution: ReturnType<typeof select> | null = null;
	let resolutionText: HTMLElement | null = null;
	let resolutionNote: HTMLElement | null = null;
	/** This page is on the screen it shows, so Auto can't follow its window. */
	let onShownScreen = false;
	const errTimers = new Map<string, ReturnType<typeof setTimeout>>();

	const send = (key: string, v: Value) => app.send(cmd.set(key, v));
	const has = (key: string) => msg?.values[key] != null;

	function section(title: string, cls = 's-sec') {
		const sec = h('section', cls);
		sec.append(h('h3', undefined, title));
		return sec;
	}

	/** The quality picker: Auto and four levels, and a note when values are custom. */
	function qualitySection() {
		const sec = section('Quality');
		const seg = h('div', 's-seg');
		seg.setAttribute('role', 'radiogroup');
		seg.setAttribute('aria-label', 'Quality');
		const segment = (name: string, i: number, pick: () => void) => {
			const b = h('button', undefined, name);
			b.type = 'button';
			b.setAttribute('role', 'radio');
			b.addEventListener('click', () => {
				pick();
				markLevel(i);
			});
			seg.append(b);
			return b;
		};
		levelButtons = [
			segment('Auto', 0, () => send('auto_quality', true)),
			// CRF first: picking a level turns Auto off.
			...LEVELS.map((l, i) =>
				segment(l.name, i + 1, () => {
					for (const k of LEVEL_KEYS) send(k, l[k]);
				})
			),
		];
		levelHint = h('p', 's-seg-hint');
		sec.append(seg, levelHint);
		return sec;
	}

	function markLevel(i: number) {
		levelButtons.forEach((b, j) => {
			b.classList.toggle('on', i === j);
			b.setAttribute('aria-checked', String(i === j));
		});
		if (levelHint)
			levelHint.textContent =
				i === 0
					? AUTO_HINT
					: i > 0
						? LEVELS[i - 1].hint
						: 'Custom: set under Advanced. Pick a level to go back.';
	}

	/**
	 * A menu whose first choice is Auto (`autoKey` on, the default) and whose others
	 * set `key` to a fixed value, which turns Auto off.
	 */
	function autoRow(
		key: string,
		autoKey: string,
		label: string,
		hint: string | undefined,
		options: [string, string][],
		restart = false
	) {
		const { r, ctl, err } = row(label, hint, restart);
		const s = select(label, options, (v) =>
			v === AUTO ? send(autoKey, true) : send(key, v)
		);
		ctl.append(s.wrap);
		controls.set(key, {
			err,
			set: () => {
				const v = msg?.values;
				if (v) s.set(v[autoKey] === true ? AUTO : String(v[key]));
			},
			disable: (off) => (s.input.disabled = off),
		});
		return r;
	}

	/**
	 * Resolution (Auto by default) and Scaling. They can change only with
	 * --enable-resize; otherwise the resolution is shown as the host set it.
	 */
	function displaySection(l: Limits) {
		const sec = section('Display');
		resolution = null;
		resolutionText = null;
		if (!l.resize) {
			const { r, ctl } = row('Resolution', 'Set by the host');
			resolutionText = h('span', 's-value');
			ctl.append(resolutionText);
			sec.append(r);
			return sec;
		}
		// Auto (the default) follows this window; the others are the display's modes,
		// the host's own marked as recommended.
		const { r, ctl, err } = row('Resolution');
		resolutionNote = h('div', 's-note');
		r.append(resolutionNote);
		resolution = select(
			'Resolution',
			[
				[AUTO, 'Auto · fits your window'],
				// Biggest first, right under Auto.
				...l.resolutions
					.map((_, i, all) => all[all.length - 1 - i])
					.map((x): [string, string] => [
						x,
						x === l.native ? `${sizeLabel(x)} · Recommended` : sizeLabel(x),
					]),
			],
			(v) => (v === AUTO ? send('auto_resize', true) : send('resolution', v))
		);
		ctl.append(resolution.wrap);
		controls.set('resolution', {
			err,
			set: () => showDesk(),
			disable: (off) => (resolution!.input.disabled = off),
		});
		sec.append(r);

		// Scaling, as an operating system has it: 200% shows everything twice as big,
		// at half the desktop pixels per window pixel.
		const base = l.scale_default;
		if (base) {
			const {
				r: sr,
				ctl: sctl,
				err: serr,
			} = row('Scaling', 'Bigger text and windows, at a lower resolution');
			const pcts = [75, 100, 125, 150, 175, 200];
			const sc = select(
				'Scaling',
				pcts.map((p): [string, string] => [
					String(p),
					p === 100 ? '100% · Default' : `${p}%`,
				]),
				(v) =>
					send('resize_scale', Math.round((base * 10000) / Number(v)) / 100)
			);
			sctl.append(sc.wrap);
			controls.set('resize_scale', {
				err: serr,
				set: (v) => {
					const p = Math.round(((base / Number(v)) * 100) / 5) * 5;
					sc.set(String(p));
					// A scale shows only while the desktop follows the window.
					sc.input.disabled = app.readOnly || msg?.values.auto_resize !== true;
				},
				disable: (off) =>
					(sc.input.disabled = off || msg?.values.auto_resize !== true),
			});
			sec.append(sr);
		}
		return sec;
	}

	/** Shows the desktop's size in the resolution row. */
	/**
	 * This window's size in desktop pixels, and the menu size Auto would pick for it
	 * (the biggest that fits, as the server does), from the latest settings.
	 */
	function windowFit(): { want: string; pick: string } | null {
		const scale = Number(msg?.values.resize_scale);
		if (!msg || !scale) return null;
		const w = Math.round(window.innerWidth * scale);
		const ht = Math.round(window.innerHeight * scale);
		const sizes = msg.limits.resolutions.map((s) => s.split('x').map(Number));
		const fits = sizes.filter(([a, b]) => a <= w && b <= ht);
		const pick = fits.length
			? fits.reduce((p, c) => (c[0] * c[1] > p[0] * p[1] ? c : p))
			: sizes[0];
		return { want: `${w}x${ht}`, pick: pick ? `${pick[0]}x${pick[1]}` : '' };
	}

	function showDesk() {
		const auto = msg?.values.auto_resize === true;
		if (resolutionNote) {
			// On the host, Auto holds its own resolution: show what the window would get.
			const fit = onShownScreen && auto ? windowFit() : null;
			resolutionNote.textContent = fit
				? `Your window is ${fit.want.replace('x', ' × ')}, which would get ${sizeLabel(fit.pick)} from another computer. This window is on the computer it shows, so Auto keeps that computer’s own resolution instead of shrinking its screen.`
				: '';
		}
		if (resolutionText)
			resolutionText.textContent = desk ? sizeLabel(desk) : '';
		if (!resolution) return;
		if (auto) {
			// Auto: say what it came to.
			resolution.set(AUTO);
			const o = resolution.input.options[0];
			o.textContent = !desk
				? 'Auto · fits your window'
				: onShownScreen
					? `Auto · Native ${desk.replace('x', ' × ')}`
					: `Auto · ${desk.replace('x', ' × ')}`;
			return;
		}
		resolution.input.options[0].textContent = 'Auto · fits your window';
		if (desk) resolution.set(desk);
	}

	/** A dropdown or switch row for a server setting. */
	function pickRow(p: Pick) {
		const { r, ctl, err } = row(p.label, p.hint, p.restart);
		let c: Control;
		if (p.toggle) {
			const t = toggle(p.label, (on) => send(p.key, on));
			ctl.append(t);
			c = {
				err,
				set: (v) => (t.checked = !!v),
				disable: (off) => (t.disabled = off),
			};
		} else {
			const s = select(p.label, p.options ?? [], (v) => send(p.key, v));
			ctl.append(s.wrap);
			c = {
				err,
				set: (v) => s.set(String(v)),
				disable: (off) => (s.input.disabled = off),
			};
		}
		controls.set(p.key, c);
		return r;
	}

	function numRow(n: Num & { restart?: boolean }, limits: Limits) {
		const { r, ctl, err } = row(n.label, n.hint, n.restart);
		const s = stepper(n, limits, (v) => send(n.key, v));
		s.err = err;
		ctl.append(s.el);
		controls.set(n.key, s);
		return r;
	}

	function build(m: SettingsMessage) {
		list.replaceChildren();
		controls.clear();
		const l = m.limits;

		if (has('crf')) list.append(qualitySection());
		list.append(displaySection(l));

		const general = section('Playback');
		if (has('framerate')) {
			const top = Number(m.values.auto_framerate_fps) || 60;
			general.append(
				autoRow(
					'framerate',
					'auto_framerate',
					'Smoothness',
					'Frames per second',
					[[AUTO, `Auto · up to ${top} fps`], ...FRAMERATES],
					true
				)
			);
		}
		if (l.audio && has('audio_bitrate'))
			general.append(
				autoRow('audio_bitrate', 'auto_audio', 'Sound', undefined, [
					[AUTO, 'Auto'],
					...AUDIO,
				])
			);
		if (general.childElementCount > 1) list.append(general);

		const sharing = section('Sharing');
		const shares: Pick[] = [
			{
				key: 'clipboard_in',
				label: 'Paste into the desktop',
				toggle: true,
				show: (x) => x.clipboard_in,
			},
			{
				key: 'clipboard_out',
				label: 'Copy from the desktop',
				toggle: true,
				show: (x) => x.clipboard_out,
			},
			{
				key: 'upload',
				label: 'Uploads',
				hint: 'Send files to the desktop',
				toggle: true,
				show: (x) => x.upload,
			},
			{
				key: 'download',
				label: 'Downloads',
				hint: 'Save files from the desktop',
				toggle: true,
				show: (x) => x.download,
			},
		];
		for (const p of shares)
			if (p.show!(l) && has(p.key)) sharing.append(pickRow(p));
		if (sharing.childElementCount > 1) list.append(sharing);

		// This viewer only: not sent to the server.
		const mine = section('This device');
		const { r, ctl } = row('Show stats', 'Frame rate, bitrate and latency');
		const t = toggle('Show stats', (on) => {
			save(STATS_KEY, on);
			opts.onStats(on);
		});
		t.checked = savedStats();
		ctl.append(t);
		mine.append(r);
		list.append(mine);

		// Advanced: collapsed whenever the panel opens.
		const adv = h('details', 's-adv');
		const sum = h('summary');
		sum.innerHTML = ICON_CHEVRON;
		sum.append(h('span', undefined, 'Advanced'));
		adv.append(sum);
		for (const g of ADVANCED) {
			const items = g.items.filter((i) => (!i.show || i.show(l)) && has(i.key));
			if (!items.length) continue;
			const sec = h('div', 's-adv-group');
			sec.append(h('h4', undefined, g.title));
			for (const i of items) sec.append(isNum(i) ? numRow(i, l) : pickRow(i));
			adv.append(sec);
		}
		list.append(adv);
	}

	function update(m: SettingsMessage) {
		msg = m;
		const shape = JSON.stringify([
			m.limits,
			Object.keys(m.values).filter((k) => m.values[k] != null),
		]);
		if (shape !== built) {
			built = shape;
			build(m);
		}
		for (const [key, c] of controls) {
			const v = m.values[key];
			if (v != null) c.set(v);
			// A watch-only viewer sees the settings but can't change them.
			c.disable(app.readOnly);
		}
		levelButtons.forEach((b) => (b.disabled = app.readOnly));
		if (!desk && typeof m.values.resolution === 'string')
			desk = m.values.resolution;
		showDesk();
		markLevel(level(m.values));
		note.textContent = app.readOnly
			? 'You’re watching only, so these can’t be changed here.'
			: 'Changes apply to everyone watching, until voxl restarts.';
	}

	function onError(key: string, error: string) {
		const c = controls.get(key);
		if (!c) return;
		c.err.textContent = error;
		clearTimeout(errTimers.get(key));
		errTimers.set(
			key,
			setTimeout(() => (c.err.textContent = ''), 5000)
		);
	}

	function open() {
		opts.closeFiles();
		if (!msg) list.textContent = 'Connecting…';
		const adv = list.querySelector<HTMLDetailsElement>('.s-adv');
		if (adv) adv.open = false;
		list.scrollTop = 0;
		panel.classList.add('open');
		btn.classList.add('on');
	}
	function close() {
		panel.classList.remove('open');
		btn.classList.remove('on');
	}

	btn.addEventListener('click', () =>
		panel.classList.contains('open') ? close() : open()
	);
	el('settings-close').addEventListener('click', close);
	// Keep the window size under Auto current while it changes.
	let resizeFrame = 0;
	window.addEventListener('resize', () => {
		if (!onShownScreen || resizeFrame) return;
		resizeFrame = requestAnimationFrame(() => {
			resizeFrame = 0;
			showDesk();
		});
	});
	el('files-btn').addEventListener('click', close);
	panel.addEventListener('pointerdown', (e) => e.stopPropagation());
	// Esc closes the panel unless the remote screen has focus (it goes to the desktop then).
	document.addEventListener('keydown', (e) => {
		if (e.key === 'Escape' && e.target !== app.kbd && e.target !== app.surface)
			close();
	});

	return {
		update,
		onError,
		/** The desktop's size changed (a hello). */
		/** The page turned out to be on the screen it shows (see resize.ts). */
		sameScreen() {
			onShownScreen = true;
			showDesk();
		},
		setDesk(w: number, ht: number) {
			desk = `${w}x${ht}`;
			showDesk();
		},
	};
}
