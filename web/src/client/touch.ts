// Touch: tap = click, drag = left-drag, long press = right click, two fingers =
// scroll (content follows the fingers).
import { cmd } from '../protocol';
import type { App } from './app';

const SLOP = 10;
const LONG_PRESS = 550;

interface Hooks {
  /** Move the remote pointer to a client (CSS pixel) position. */
  moveTo(x: number, y: number): void;
  queueScroll(dx: number, dy: number): void;
  deskPerPx(): number;
}

export function createTouch(app: App, hooks: Hooks) {
  const { send } = app;
  const pts = new Map<number, { x: number; y: number }>();
  let mode: 'pending' | 'drag' | 'scroll' | 'done' | null = null;
  let start = { x: 0, y: 0 };
  let timer: ReturnType<typeof setTimeout> | undefined;
  let lastMid: [number, number] = [0, 0];
  const mid = (): [number, number] => {
    let x = 0, y = 0;
    for (const p of pts.values()) { x += p.x; y += p.y; }
    return [x / pts.size, y / pts.size];
  };
  const click = (button: number) => { send(cmd.button(button, true)); send(cmd.button(button, false)); };

  function down(e: PointerEvent) {
    try { app.surface.setPointerCapture(e.pointerId); } catch {}
    pts.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pts.size === 1) {
      mode = 'pending';
      start = { x: e.clientX, y: e.clientY };
      timer = setTimeout(() => {
        if (mode !== 'pending') return;
        hooks.moveTo(start.x, start.y);
        click(3);
        mode = 'done';
        navigator.vibrate?.(10);
      }, LONG_PRESS);
    } else if (pts.size === 2) {
      clearTimeout(timer);
      if (mode === 'drag') send(cmd.button(1, false));
      mode = 'scroll';
      lastMid = mid();
    }
  }
  function move(e: PointerEvent) {
    const p = pts.get(e.pointerId);
    if (!p) return;
    p.x = e.clientX;
    p.y = e.clientY;
    if (mode === 'pending' && Math.hypot(p.x - start.x, p.y - start.y) > SLOP) {
      clearTimeout(timer);
      mode = 'drag';
      hooks.moveTo(start.x, start.y);
      send(cmd.button(1, true));
    }
    if (mode === 'drag') hooks.moveTo(p.x, p.y);
    else if (mode === 'scroll' && pts.size >= 2) {
      const m = mid(), k = hooks.deskPerPx();
      hooks.queueScroll((lastMid[0] - m[0]) * k, (lastMid[1] - m[1]) * k);
      lastMid = m;
    }
  }
  function up(e: PointerEvent) {
    if (!pts.delete(e.pointerId)) return;
    if (mode === 'pending') { clearTimeout(timer); hooks.moveTo(start.x, start.y); click(1); mode = 'done'; }
    else if (mode === 'drag') { send(cmd.button(1, false)); mode = 'done'; }
    else if (mode === 'scroll' && pts.size < 2) mode = 'done';
    if (pts.size === 0) mode = null;
  }
  function cancel(e: PointerEvent) {
    clearTimeout(timer);
    if (mode === 'drag') send(cmd.button(1, false));
    pts.delete(e.pointerId);
    if (pts.size === 0) mode = null;
  }
  return { down, move, up, cancel };
}
