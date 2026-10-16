// Toolbar (top right on hover; bottom right and always visible on touch devices):
// on-screen keyboard, files, microphone and fullscreen buttons.
import type { App } from './app';
import { el } from './app';

export function setupToolbar(app: App, onFullscreenChange: () => void) {
  const { tools, kbd } = app;
  const kbBtn = el<HTMLButtonElement>('kb-btn');
  const fsBtn = el<HTMLButtonElement>('fs-btn');
  let hideTimer: ReturnType<typeof setTimeout> | undefined;
  function show(ms: number) {
    tools.classList.add('show');
    clearTimeout(hideTimer);
    if (ms) hideTimer = setTimeout(() => tools.classList.remove('show'), ms);
  }
  // Touch devices keep the toolbar visible; mice reveal it near the top edge.
  if (matchMedia('(pointer: coarse)').matches) { tools.classList.add('touch'); show(0); }
  else document.addEventListener('pointermove', (e) => { if (e.pointerType === 'mouse' && e.clientY < 48 && e.clientX > window.innerWidth - 160) show(2500); });
  // Clicks on the toolbar must not reach the desktop surface underneath.
  for (const b of tools.querySelectorAll('button')) b.addEventListener('pointerdown', (e) => e.stopPropagation());

  kbBtn.addEventListener('click', () => {
    if (document.activeElement === kbd && kbd.dataset.soft) { kbd.blur(); delete kbd.dataset.soft; }
    else { kbd.dataset.soft = '1'; kbd.focus({ preventScroll: true }); }
  });

  if (!document.fullscreenEnabled) fsBtn.style.display = 'none';
  fsBtn.addEventListener('click', () => {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else document.documentElement.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
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
}
