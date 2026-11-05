// Several viewers on one desktop: who else is here, their pointers in their own colors,
// and handing control over. Only the viewer in control drives the desktop (the server
// drops everyone else's input), but every viewer's pointer is shown to the others.
// The toolbar gets a people button once someone else joins; the list and short notices
// drop down beneath it.
import { cmd } from '../protocol';
import type { PeerPointer, Users } from '../protocol';
import type { App } from './app';
import { el } from './app';

/** One color per viewer, picked by id, so every viewer sees the same colors. */
const COLORS = [
	'#118af3',
	'#f15555',
	'#25b134',
	'#f5a524',
	'#a66cf6',
	'#14b8a6',
	'#ec4899',
	'#84cc16',
];
export const userColor = (id: number) => COLORS[(id - 1) % COLORS.length];
const userName = (id: number) => `User ${id}`;

/** How long a notice stays up; one with a button stays twice as long. */
const NOTE_MS = 4000;

function dot(id: number) {
	const d = document.createElement('span');
	d.className = 'u-dot';
	d.style.background = userColor(id);
	d.textContent = String(id);
	return d;
}

function button(label: string, primary: boolean, onClick: () => void) {
	const b = document.createElement('button');
	b.type = 'button';
	b.className = primary ? 'btn primary' : 'btn';
	const s = document.createElement('span');
	s.textContent = label;
	b.append(s);
	b.addEventListener('click', (e) => {
		e.stopPropagation();
		onClick();
	});
	return b;
}

export function createUsers(app: App, showToolbar: (ms: number) => void) {
	const btn = el<HTMLButtonElement>('users-btn');
	const count = el('users-count');
	const drop = el('users');
	const noteEl = el('users-note');
	const panel = el('users-panel');
	const status = el('users-status');
	const list = el('users-list');
	const peers = el('peers');

	let state: Users | null = null;
	let open = false;
	let noting = false;
	const cursors = new Map<number, HTMLElement>();
	/** Requests this viewer has already been told about, so each shows once. */
	const announced = new Set<number>();

	const multi = () => !!state && state.users.length > 1;
	const mine = () =>
		!state || state.control === null || state.control === state.you;

	/** The dropdown (and the toolbar above it) stays up while it shows anything. */
	function updateDrop() {
		drop.classList.toggle('show', open || noting);
		panel.hidden = !open;
		noteEl.hidden = !noting;
		app.tools.classList.toggle('users-open', open || noting);
		btn.classList.toggle('on', open);
		btn.setAttribute('aria-expanded', String(open));
	}

	let noteTimer: ReturnType<typeof setTimeout> | undefined;
	function note(text: string, action?: { label: string; run: () => void }) {
		noteEl.replaceChildren();
		const t = document.createElement('span');
		t.textContent = text;
		noteEl.append(t);
		if (action)
			noteEl.append(
				button(action.label, true, () => {
					action.run();
					hideNote();
				})
			);
		noting = true;
		updateDrop();
		clearTimeout(noteTimer);
		noteTimer = setTimeout(hideNote, action ? NOTE_MS * 2 : NOTE_MS);
	}
	function hideNote() {
		clearTimeout(noteTimer);
		noting = false;
		updateDrop();
		// Let the toolbar fade on its own rather than vanish under the pointer.
		showToolbar(1500);
	}

	const give = (id: number) => app.send(cmd.giveControl(id));
	const request = (on: boolean) => app.send(cmd.requestControl(on));

	function render() {
		const s = state;
		app.tools.classList.toggle('users', multi());
		if (!s || !multi()) {
			open = false;
			noting = false;
			updateDrop();
			return;
		}
		count.textContent = String(s.users.length);
		// Someone waiting on this viewer to hand over control.
		const asked = s.control === s.you && s.users.some((u) => u.req);
		btn.classList.toggle('ask', asked);
		const label =
			s.control === null
				? `${s.users.length} people watching`
				: s.control === s.you
					? 'You’re in control'
					: `${userName(s.control)} is in control`;
		btn.title = `${s.users.length} people here · ${label}`;
		status.textContent = label;

		list.replaceChildren();
		for (const u of s.users) {
			const row = document.createElement('div');
			row.className = 'u-row';
			const name = document.createElement('span');
			name.className = 'u-name';
			name.textContent = userName(u.id) + (u.id === s.you ? ' (you)' : '');
			row.append(dot(u.id), name);
			if (u.id === s.control || u.req) {
				const b = document.createElement('span');
				b.className = u.id === s.control ? 'u-badge' : 'u-badge req';
				b.textContent = u.id === s.control ? 'In control' : 'Asked';
				row.append(b);
			}
			if (s.control === s.you && u.id !== s.you)
				row.append(button('Give control', u.req, () => give(u.id)));
			list.append(row);
		}
		if (s.control !== null && s.control !== s.you) {
			const me = s.users.find((u) => u.id === s.you);
			const foot = document.createElement('div');
			foot.className = 'u-foot';
			foot.append(
				me?.req
					? button('Cancel request', false, () => request(false))
					: button('Request control', true, () => request(true))
			);
			list.append(foot);
		}
		updateDrop();
	}

	function onUsers(m: Users) {
		const prev = state;
		state = m;
		app.inControl = mine();
		// Someone joining or leaving brings the toolbar up for a moment.
		if (multi() && prev?.users.length !== m.users.length) showToolbar(2500);

		// Tell this viewer when control changes hands (not on first connect).
		if (prev && prev.control !== m.control && m.control !== null && multi())
			note(
				m.control === m.you
					? 'You’re in control'
					: `${userName(m.control)} is in control`
			);
		// The viewer in control hears about each new request once.
		for (const u of m.users) {
			if (!u.req) announced.delete(u.id);
			else if (m.control === m.you && !announced.has(u.id)) {
				announced.add(u.id);
				note(`${userName(u.id)} asked for control`, {
					label: 'Give control',
					run: () => give(u.id),
				});
			}
		}
		// Drop the pointers of viewers who left.
		const ids = new Set(m.users.map((u) => u.id));
		for (const [id, c] of cursors)
			if (!ids.has(id)) {
				c.remove();
				cursors.delete(id);
			}
		render();
	}

	function cursorFor(id: number) {
		let c = cursors.get(id);
		if (!c) {
			c = document.createElement('div');
			c.className = 'peer';
			c.style.setProperty('--c', userColor(id));
			c.innerHTML =
				'<svg viewBox="0 0 16 20" width="16" height="20"><path d="M1 1L1 16L5 12L8 19L11 17.6L8 11L14 11Z" fill="var(--c)" stroke="#fff" stroke-width="1.5" stroke-linejoin="round"/></svg>';
			const tag = document.createElement('span');
			tag.textContent = userName(id);
			c.append(tag);
			peers.append(c);
			cursors.set(id, c);
		}
		return c;
	}

	function onPointer(m: PeerPointer) {
		if (state && m.id === state.you) return;
		if (m.x === undefined || m.y === undefined || !app.desk.w || !app.desk.h) {
			cursors.get(m.id)?.classList.remove('on');
			return;
		}
		const c = cursorFor(m.id);
		// Percentages of the desktop, so the pointer stays put through layout changes.
		c.style.left = `${(m.x / app.desk.w) * 100}%`;
		c.style.top = `${(m.y / app.desk.h) * 100}%`;
		c.classList.add('on');
	}

	btn.addEventListener('click', () => {
		open = !open;
		if (open) noting = false;
		render();
	});
	// Keep clicks on the dropdown off the desktop underneath.
	drop.addEventListener('pointerdown', (e) => e.stopPropagation());
	app.surface.addEventListener('pointerdown', () => {
		// Clicking the desktop closes the list.
		if (open) {
			open = false;
			updateDrop();
		}
		// A viewer who isn't in control learns why nothing happens.
		const s = state;
		if (!s || mine() || s.control === null) return;
		const me = s.users.find((u) => u.id === s.you);
		if (me?.req) note(`Waiting for ${userName(s.control)} to give you control`);
		else
			note(`${userName(s.control)} is in control`, {
				label: 'Request control',
				run: () => request(true),
			});
	});

	return {
		onUsers,
		onPointer,
		/** The socket dropped: everyone here is from the old connection. */
		reset() {
			state = null;
			app.inControl = true;
			for (const c of cursors.values()) c.remove();
			cursors.clear();
			announced.clear();
			render();
		},
	};
}
