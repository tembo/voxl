const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('typescript');

const listenerTarget = () => ({
	listeners: {},
	value: '',
	focus() {},
	addEventListener(name, fn) {
		this.listeners[name] = fn;
	},
});

function fixture(isMac = false, remoteMac = false) {
	const kbd = listenerTarget(),
		surface = listenerTarget(),
		window = listenerTarget();
	const sent = [];
	function load(file) {
		const module = { exports: {} };
		const source = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
			compilerOptions: {
				module: ts.ModuleKind.CommonJS,
				target: ts.ScriptTarget.ES2022,
			},
		}).outputText;
		vm.runInNewContext('(function(require,module,exports){' + source + '\n})', {
			window,
			setTimeout,
			clearTimeout,
			console,
		})(
			(name) => load(path.resolve(path.dirname(file), name + '.ts')),
			module,
			module.exports
		);
		return module.exports;
	}
	const keyboard = load(
		path.resolve(__dirname, '../src/client/keyboard.ts')
	).setupKeyboard(
		{
			kbd,
			surface,
			isMac,
			remoteMac,
			inIframe: true,
			send: (value) => sent.push(value),
		},
		{ noteLocal() {} },
		() => {}
	);
	function event(type, code, key, flags = {}) {
		kbd.listeners[type]({
			code,
			key,
			ctrlKey: false,
			metaKey: false,
			shiftKey: false,
			altKey: false,
			isComposing: false,
			repeat: false,
			getModifierState: (name) => name === 'AltGraph' && !!flags.altGraph,
			preventDefault() {},
			...flags,
		});
	}
	return { sent, event, keyboard, kbd, window };
}
const key = (ks, down) => `k,${ks},${down ? 1 : 0}`;

test('character-only Ctrl shortcut reconciles flags and releases inferred modifier', () => {
	const f = fixture(true, false);
	f.event('keydown', 'KeyA', 'a', { ctrlKey: true });
	f.event('keyup', 'KeyA', 'a', { ctrlKey: true });
	assert.deepEqual(f.sent, [
		key(0xffe3, true),
		key(97, true),
		key(97, false),
		key(0xffe3, false),
	]);
	f.event('keydown', 'KeyX', 'X');
	assert.equal(f.sent.at(-1), key(88, true));
});
test('Mac Command maps to remote Linux Control, and Windows Control to Mac Command', () => {
	for (const [isMac, remoteMac, flags, expected] of [
		[true, false, { metaKey: true }, 0xffe3],
		[false, true, { ctrlKey: true }, 0xffeb],
		[true, true, { metaKey: true }, 0xffeb],
	]) {
		const f = fixture(isMac, remoteMac);
		f.event('keydown', 'KeyA', 'a', flags);
		assert.equal(f.sent[0], key(expected, true));
	}
});
test('explicit right modifier is retained without a spurious left modifier', () => {
	const f = fixture();
	f.event('keydown', 'ControlRight', 'Control', { ctrlKey: true });
	f.event('keydown', 'KeyA', 'a', { ctrlKey: true });
	f.event('keyup', 'KeyA', 'a', { ctrlKey: true });
	f.event('keyup', 'ControlRight', 'Control');
	assert.deepEqual(f.sent, [
		key(0xffe4, true),
		key(97, true),
		key(97, false),
		key(0xffe4, false),
	]);
});
test('authoritative false flags release a modifier with a missing keyup', () => {
	const f = fixture();
	f.event('keydown', 'ControlRight', 'Control', { ctrlKey: true });
	f.event('keydown', 'KeyX', 'x');
	assert.deepEqual(f.sent, [
		key(0xffe4, true),
		key(0xffe4, false),
		key(120, true),
	]);
});
test('inferred side is replaced when the physical right side becomes known', () => {
	const f = fixture();
	f.event('keydown', 'KeyA', 'a', { ctrlKey: true });
	f.event('keydown', 'ControlRight', 'Control', { ctrlKey: true });
	assert.deepEqual(f.sent.slice(-2), [key(0xffe3, false), key(0xffe4, true)]);
});
test('AltGraph and Option/Shift-resolved symbols are not turned into Ctrl/Alt chords', () => {
	for (const flags of [
		{ ctrlKey: true, altKey: true, altGraph: true },
		{ altKey: true },
		{ shiftKey: true },
	]) {
		const f = fixture();
		f.event('keydown', 'Digit2', '@', flags);
		f.event('keyup', 'Digit2', '@', flags);
		assert.deepEqual(f.sent, [key(64, true), key(64, false)]);
	}
});
test('navigation recovers Shift selection and combined shortcuts', () => {
	const f = fixture();
	f.event('keydown', 'ArrowLeft', 'ArrowLeft', { shiftKey: true });
	f.event('keyup', 'ArrowLeft', 'ArrowLeft', { shiftKey: true });
	assert.deepEqual(f.sent, [
		key(0xffe1, true),
		key(0xff51, true),
		key(0xff51, false),
		key(0xffe1, false),
	]);
});
test('dead-key composition does not infer Option, composition sends Unicode normally', () => {
	const f = fixture();
	f.event('keydown', 'KeyE', 'Dead', { altKey: true });
	assert.deepEqual(f.sent, []);
	f.kbd.listeners.compositionend({ data: 'é' });
	assert.deepEqual(f.sent, [key(233, true), key(233, false)]);
});
test('blur releases state and cancels delayed paste before it can press a key', async () => {
	const f = fixture();
	f.event('keydown', 'KeyV', 'v', { ctrlKey: true });
	f.window.listeners.blur();
	const atBlur = [...f.sent];
	await new Promise((resolve) => setTimeout(resolve, 180));
	assert.deepEqual(f.sent, atBlur);
	assert.equal(f.sent.at(-1), 'r');
});
