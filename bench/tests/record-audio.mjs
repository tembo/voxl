// Linux regression test: record a known tone/silence/tone through PulseAudio.
// Start voxl against a running X display and a PulseAudio monitor source, then run:
// VOXL_URL=http://127.0.0.1:6901 VOXL_TOKEN=... PULSE_SINK=... node bench/tests/record-audio.mjs
// Requires ffmpeg, ffprobe and paplay. Use a dedicated sink so other audio cannot affect the test.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const { VOXL_URL, VOXL_TOKEN, PULSE_SINK } = process.env;
if (!VOXL_URL || !VOXL_TOKEN || !PULSE_SINK) {
  throw new Error('Set VOXL_URL, VOXL_TOKEN and PULSE_SINK');
}
const dir = mkdtempSync(join(tmpdir(), 'voxl-record-audio-'));
const wav = join(dir, 'tone.wav');
const mp4 = join(dir, 'recording.mp4');
const auth = { Authorization: `Bearer ${VOXL_TOKEN}` };
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
function command(program, args) {
  const result = spawnSync(program, args, { encoding: 'utf8' });
  if (result.status !== 0) throw new Error(`${program}: ${result.stderr || result.error}`);
  return result.stdout;
}
async function request(path, method = 'GET') {
  const response = await fetch(new URL(path, VOXL_URL), { method, headers: auth });
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status}: ${await response.text()}`);
  return response;
}
try {
  // 440 Hz for a second, digital silence for a second, then 880 Hz for a second.
  command('ffmpeg', ['-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000:duration=1',
    '-f', 'lavfi', '-i', 'anullsrc=r=48000:cl=stereo:d=1',
    '-f', 'lavfi', '-i', 'sine=frequency=880:sample_rate=48000:duration=1',
    '-filter_complex', '[0:a]aformat=channel_layouts=stereo[a];[2:a]aformat=channel_layouts=stereo[b];[a][1:a][b]concat=n=3:v=0:a=1[out]',
    '-map', '[out]', '-y', wav]);
  const { id } = await (await request('/record/start', 'POST')).json();
  // Allow capture to start before playing; keep a little padding at each end.
  await sleep(800);
  const player = spawn('paplay', ['--device', PULSE_SINK, wav], { stdio: 'inherit' });
  await new Promise((resolve, reject) => {
    player.on('error', reject);
    player.on('exit', (code) => code === 0 ? resolve() : reject(new Error(`paplay exited ${code}`)));
  });
  await sleep(400);
  writeFileSync(mp4, Buffer.from(await (await request(`/record/stop?id=${id}`, 'POST')).arrayBuffer()));
  const streams = JSON.parse(command('ffprobe', ['-v', 'error', '-show_entries',
    'stream=codec_name,codec_type', '-of', 'json', mp4])).streams;
  if (!streams.some((s) => s.codec_type === 'video' && s.codec_name === 'h264')) {
    throw new Error('recording has no H.264 video stream');
  }
  if (!streams.some((s) => s.codec_type === 'audio' && s.codec_name === 'opus')) {
    throw new Error(`recording has no Opus audio stream: ${JSON.stringify(streams)}`);
  }
  const decoded = spawnSync('ffmpeg', ['-v', 'error', '-i', mp4, '-map', '0:a:0',
    '-ac', '1', '-ar', '48000', '-f', 'f32le', '-'], { maxBuffer: 16 * 1024 * 1024 });
  if (decoded.status !== 0) throw new Error(`audio decode failed: ${decoded.stderr}`);
  function rms(start, end) {
    let sum = 0;
    let count = 0;
    for (let i = Math.floor(start * 48000); i < Math.floor(end * 48000); i++) {
      if (i * 4 + 4 > decoded.stdout.length) throw new Error('audio ends early');
      const value = decoded.stdout.readFloatLE(i * 4);
      sum += value * value;
      count++;
    }
    return Math.sqrt(sum / count);
  }
  // A PulseAudio monitor may begin producing packets only when playback starts.
  // Look for tone, one second of silence, and the second tone relative to the
  // first audible samples rather than assuming a fixed PulseAudio startup delay.
  let firstTone = null;
  for (let start = 0; start < 2; start += 0.1) {
    if ((start + 0.2) * 48000 * 4 > decoded.stdout.length) break;
    if (rms(start, start + 0.2) > 0.02) { firstTone = start; break; }
  }
  if (firstTone === null) throw new Error('no audible tone in recorded audio');
  const first = rms(firstTone + 0.2, firstTone + 0.7);
  const silence = rms(firstTone + 1.1, firstTone + 1.5);
  const second = rms(firstTone + 2.1, firstTone + 2.6);
  if (first < 0.02 || second < 0.02 || silence > 0.005) {
    throw new Error(`unexpected tone/silence timing: rms=${first},${silence},${second}`);
  }
  console.log('PASS: H.264 and Opus tracks; tone, silence and tone retain their timing');
} finally {
  if (process.env.KEEP_RECORDING) console.log(`saved ${dir}`);
  else rmSync(dir, { recursive: true, force: true });
}
