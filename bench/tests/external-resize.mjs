// Run with a voxl server and X display already running. Set DISPLAY, VOXL_URL,
// VOXL_TOKEN, RESIZE_MODE_A and RESIZE_MODE_B; the modes must already exist.
// Example: DISPLAY=:97 VOXL_URL=ws://127.0.0.1:6997/ws VOXL_TOKEN=... \
//   RESIZE_MODE_A=1920x1200 RESIZE_MODE_B=1280x800_60.00 node bench/tests/external-resize.mjs
import { execFileSync } from 'node:child_process';

const { VOXL_URL, VOXL_TOKEN, RESIZE_MODE_A, RESIZE_MODE_B } = process.env;
if (!VOXL_URL || !VOXL_TOKEN || !RESIZE_MODE_A || !RESIZE_MODE_B || !process.env.DISPLAY) {
  throw new Error('Set DISPLAY, VOXL_URL, VOXL_TOKEN, RESIZE_MODE_A and RESIZE_MODE_B');
}
const url = new URL(VOXL_URL);
url.searchParams.set('token', VOXL_TOKEN);
const ws = new WebSocket(url);
let latest = '';
let frameSize = '';
const frames = new Map();
ws.binaryType = 'arraybuffer';
ws.onmessage = ({ data }) => {
  if (typeof data === 'string') {
    const msg = JSON.parse(data);
    if (msg.t === 'hello') {
      latest = `${msg.w}x${msg.h}`;
      frames.clear();
      ws.send('v,1');
    }
    return;
  }

  const packet = new Uint8Array(data);
  if (packet.length < 16 || packet[0] !== 0x01) return;
  const view = new DataView(data);
  const id = view.getUint32(2);
  const part = packet[10];
  const partCount = packet[12];
  let received = frames.get(id);
  if (!received) {
    received = new Set();
    frames.set(id, received);
  }
  received.add(part);
  if (received.size === partCount) {
    frames.delete(id);
    frameSize = latest;
    ws.send(`a,${id}`);
  }
};
const waitFor = async (condition) => {
  const deadline = Date.now() + 5000;
  while (!condition()) {
    if (ws.readyState === WebSocket.CLOSED || Date.now() > deadline) {
      throw new Error(`viewer disconnected or timed out; last size ${latest}, last frame size ${frameSize}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
};
try {
  await waitFor(() => latest !== '');
  for (let i = 0; i < 50; i++) {
    const mode = i % 2 ? RESIZE_MODE_B : RESIZE_MODE_A;
    const expected = mode.match(/^(\d+x\d+)/)?.[1];
    if (!expected) throw new Error(`invalid mode name: ${mode}`);
    const output = process.env.RESIZE_OUTPUT || 'screen';
    frameSize = '';
    execFileSync('xrandr', ['--output', output, '--mode', mode]);
    await waitFor(() => latest === expected);
    await waitFor(() => frameSize === expected);
  }
  console.log('50 external resizes completed; received and acknowledged a video frame at each size');
} finally {
  ws.close();
}
