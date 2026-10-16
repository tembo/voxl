// Microphone end to end: a private PulseAudio whose default source is a pipe source
// reading voxl's FIFO (as in the Computer preview), voxl with
// --microphone-enabled, and headless Chromium whose fake microphone plays a 440 Hz
// tone. Turns the mic on from the toolbar, records the VM's default source and checks
// the tone arrives; then checks turning it off and a second client taking it over.
// Usage: node mictest.mjs   (VOXL_BIN, PULSEAUDIO = pulseaudio package, BENCH_DISPLAY,
// CHROMIUM, PLAYWRIGHT_NODE_MODULES)
import { createRequire } from 'module';
import { spawn, execFileSync } from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';
const require = createRequire(
	process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/'
);
const { chromium } = require('playwright');

const HERE = new URL('.', import.meta.url).pathname;
const bin =
	process.env.VOXL_BIN || path.join(HERE, '../../target/release/voxl');
const pa = process.env.PULSEAUDIO || '/run/current-system/sw';
const port = process.env.VOXL_PORT || '6907';
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'voxl-mic-'));
const sock = path.join(dir, 'pulse.sock');
const env = {
	...process.env,
	PULSE_SERVER: `unix:${sock}`,
	HOME: dir,
	XDG_RUNTIME_DIR: dir,
	DISPLAY: process.env.BENCH_DISPLAY || ':2',
};

// 440 Hz tone for Chromium's fake microphone.
const wav = path.join(dir, 'tone.wav');
{
	const n = 48000 * 30,
		data = Buffer.alloc(n * 2);
	for (let i = 0; i < n; i++)
		data.writeInt16LE(
			Math.round(Math.sin((2 * Math.PI * 440 * i) / 48000) * 12000),
			i * 2
		);
	const h = Buffer.alloc(44);
	h.write('RIFF', 0);
	h.writeUInt32LE(36 + data.length, 4);
	h.write('WAVEfmt ', 8);
	h.writeUInt32LE(16, 16);
	h.writeUInt16LE(1, 20);
	h.writeUInt16LE(1, 22);
	h.writeUInt32LE(48000, 24);
	h.writeUInt32LE(96000, 28);
	h.writeUInt16LE(2, 32);
	h.writeUInt16LE(16, 34);
	h.write('data', 36);
	h.writeUInt32LE(data.length, 40);
	fs.writeFileSync(wav, Buffer.concat([h, data]));
}
const paConf = path.join(dir, 'mic.pa');
fs.writeFileSync(
	paConf,
	[
		`load-module module-native-protocol-unix socket=${sock} auth-anonymous=1`,
		`load-module module-pipe-source source_name=microphone file=${path.join(dir, 'mic.fifo')} format=s16le rate=48000 channels=1`,
		'set-default-source microphone',
		'',
	].join('\n')
);

let failed = 0;
const check = (ok, what) => {
	console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`);
	if (!ok) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// Records the VM's default source; returns RMS and dominant frequency (zero crossings).
function record(secs) {
	let raw;
	// parec records until the timeout stops it, which execFileSync reports as an error.
	try {
		raw = execFileSync(
			`${pa}/bin/parec`,
			[
				'--raw',
				'--format=s16le',
				'--rate=48000',
				'--channels=1',
				'--latency-msec=20',
			],
			{ env, timeout: secs * 1000, killSignal: 'SIGINT', maxBuffer: 1 << 26 }
		);
	} catch (e) {
		raw = e.stdout;
	}
	return analyse(raw);
}
function analyse(raw) {
	// Skip the first 100 ms: what was left in the FIFO (at most ~40 ms) while nobody recorded.
	raw = raw.subarray(Math.min(raw.length, 9600));
	const n = Math.floor(raw.length / 2);
	// Upward crossings with hysteresis, so noise around zero isn't counted.
	let sum = 0,
		crossings = 0,
		low = false;
	for (let i = 0; i < n; i++) {
		const v = raw.readInt16LE(i * 2);
		sum += v * v;
		if (v < -1000) low = true;
		else if (low && v > 1000) {
			crossings++;
			low = false;
		}
	}
	return { rms: Math.sqrt(sum / Math.max(n, 1)), hz: crossings / (n / 48000) };
}
const pulse = spawn(
	`${pa}/bin/pulseaudio`,
	[
		'-n',
		'--daemonize=no',
		'--exit-idle-time=-1',
		'--disable-shm=yes',
		`--file=${paConf}`,
	],
	{ env, stdio: 'ignore' }
);
let voxl, b;
try {
	for (let i = 0; i < 50 && !fs.existsSync(sock); i++) await sleep(100);
	voxl = spawn(
		bin,
		[
			`--port=${port}`,
			'--audio-enabled=false',
			'--microphone-enabled=true',
			`--microphone-fifo=${path.join(dir, 'mic.fifo')}`,
		],
		{
			env,
			stdio: [
				'ignore',
				fs.openSync(process.env.VOXL_LOG || '/dev/null', 'w'),
				fs.openSync(process.env.VOXL_LOG || '/dev/null', 'w'),
			],
		}
	);
	for (let i = 0; i < 50; i++) {
		try {
			if ((await fetch(`http://127.0.0.1:${port}/healthz`)).ok) break;
		} catch {}
		await sleep(100);
	}

	b = await chromium.launch({
		executablePath: process.env.CHROMIUM || 'chromium',
		args: [
			'--no-sandbox',
			'--use-fake-ui-for-media-stream',
			'--use-fake-device-for-media-stream',
			`--use-file-for-fake-audio-capture=${wav}`,
			'--autoplay-policy=no-user-gesture-required',
		],
	});
	const ctx = await b.newContext({
		viewport: { width: 1400, height: 900 },
		permissions: ['microphone'],
	});
	const open = async () => {
		const p = await ctx.newPage();
		await p.goto(`http://127.0.0.1:${port}/`);
		await p.waitForFunction(
			() => document.getElementById('tools').classList.contains('mic'),
			null,
			{ timeout: 10000 }
		);
		return p;
	};
	const p = await open();
	check(true, 'mic button shown when the server enables the microphone');
	const quiet = record(1);
	check(
		quiet.rms < 50,
		`VM microphone is silent before turning it on (rms ${quiet.rms.toFixed(0)})`
	);

	await p.mouse.move(1380, 20);
	await sleep(300);
	await p.click('#mic-btn');
	await p.waitForFunction(
		() => document.getElementById('mic-btn').classList.contains('on'),
		null,
		{ timeout: 10000 }
	);
	check(
		await p.evaluate(() =>
			document.getElementById('tools').classList.contains('mic-on')
		),
		'toolbar stays visible while the mic is live'
	);
	await sleep(1500);
	const live = record(2);
	check(
		live.rms > 500 && Math.abs(live.hz - 440) < 25,
		`VM microphone hears the 440 Hz tone (rms ${live.rms.toFixed(0)}, ${live.hz.toFixed(0)} Hz)`
	);
	await p.screenshot({
		path: path.join(os.tmpdir(), 'voxl-mic-on.png'),
		clip: { x: 1240, y: 0, width: 160, height: 56 },
	});

	await p.click('#mic-btn');
	check(
		!(await p.evaluate(() =>
			document.getElementById('mic-btn').classList.contains('on')
		)),
		'turning the mic off updates the button'
	);
	const off = record(1);
	check(
		off.rms < 50,
		`VM microphone is silent after turning it off (rms ${off.rms.toFixed(0)})`
	);

	// A second viewer turning the mic on takes it over; the first one is told.
	await p.click('#mic-btn');
	await p.waitForFunction(() =>
		document.getElementById('mic-btn').classList.contains('on')
	);
	const p2 = await open();
	await p2.mouse.move(1380, 20);
	await sleep(300);
	await p2.click('#mic-btn');
	await p2.waitForFunction(() =>
		document.getElementById('mic-btn').classList.contains('on')
	);
	await p
		.waitForFunction(
			() => !document.getElementById('mic-btn').classList.contains('on'),
			null,
			{ timeout: 5000 }
		)
		.catch(() => {});
	check(
		!(await p.evaluate(() =>
			document.getElementById('mic-btn').classList.contains('on')
		)),
		'a second viewer takes the mic over and the first one turns off'
	);
	await sleep(1000);
	const taken = record(1.5);
	check(
		taken.rms > 500,
		`audio keeps flowing from the new owner (rms ${taken.rms.toFixed(0)})`
	);
	await p2.close();
	await sleep(800);
	const gone = record(1);
	check(
		gone.rms < 50,
		`closing the owner's tab releases the microphone (rms ${gone.rms.toFixed(0)})`
	);
} finally {
	if (b) await b.close();
	if (voxl) voxl.kill();
	pulse.kill();
	fs.rmSync(dir, { recursive: true, force: true });
}
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
