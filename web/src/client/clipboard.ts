// Clipboard from the desktop to the browser. Browsers only write the clipboard while
// the page has focus and the user has just pressed a key or clicked (Chrome asks for
// clipboard permission otherwise), so a pending write waits for the next interaction.
import type { App } from './app';

/** How long a copy shortcut waits for the desktop to report what it copied. */
const COPY_WAIT_MS = 2000;

interface Copy {
	resolve(text: Blob): void;
	reject(err: Error): void;
	text: string | null;
}

export function createClipboard(app: App) {
	let pending: string | Blob | null = null;
	let copy: Copy | null = null;

	function flush() {
		if (
			pending === null ||
			!navigator.clipboard ||
			(app.inIframe && !document.hasFocus()) ||
			navigator.userActivation?.isActive === false
		)
			return;
		void write(pending);
	}
	async function write(c: string | Blob) {
		try {
			if (typeof c === 'string') await navigator.clipboard.writeText(c);
			else if (typeof ClipboardItem !== 'undefined')
				await navigator.clipboard.write([
					new ClipboardItem({ 'image/png': c }),
				]);
			else return;
			if (pending === c) pending = null;
		} catch {
			// Not allowed right now (no focus); retried on the next interaction.
		}
	}
	async function writeCopy(req: Copy, text: Promise<Blob>) {
		try {
			await navigator.clipboard.write([
				new ClipboardItem({ 'text/plain': text }),
			]);
			if (pending === req.text) pending = null;
		} catch {
			// Nothing copied, or the browser refused: text that arrived stays pending.
		}
	}
	window.addEventListener('focus', flush);

	function cancelCopy() {
		copy?.reject(new Error('no text copied'));
		copy = null;
	}

	return {
		flush,
		/**
		 * A copy shortcut is on its way to the desktop. The keypress allows a clipboard
		 * write only for a moment (Safari: only while handling it), so the write starts
		 * now with text that arrives once the desktop reports the copy.
		 */
		expectCopy() {
			if (!navigator.clipboard?.write || typeof ClipboardItem === 'undefined')
				return;
			cancelCopy();
			let req!: Copy;
			const text = new Promise<Blob>((resolve, reject) => {
				req = { resolve, reject, text: null };
			});
			copy = req;
			const timer = setTimeout(() => {
				if (copy === req) cancelCopy();
			}, COPY_WAIT_MS);
			void writeCopy(req, text).finally(() => clearTimeout(timer));
		},
		onRemoteText(text: string) {
			pending = text;
			if (!copy) {
				flush();
				return;
			}
			copy.text = text;
			copy.resolve(new Blob([text], { type: 'text/plain' }));
			copy = null;
		},
		/** Clipboard image message: type byte, then PNG. */
		onRemoteImage(buf: ArrayBuffer) {
			cancelCopy();
			pending = new Blob([new Uint8Array(buf, 1)], { type: 'image/png' });
			flush();
		},
	};
}

export type Clipboard = ReturnType<typeof createClipboard>;
