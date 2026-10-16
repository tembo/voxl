// Files panel: browse the shared folder and download, upload with the button or by
// dropping files anywhere on the page. Uses the server's /files HTTP API.
import type { FilesConfig } from '../protocol';
import type { App } from './app';
import { el } from './app';

interface Entry {
  name: string;
  dir: boolean;
  size: number;
  /** Modification time, ms since the epoch. */
  mtime: number;
}
interface Listing {
  path: string;
  entries: Entry[];
}
interface Upload {
  f: File;
  dir: string;
  row: HTMLElement;
}

// Icons from the Tembo UI package's icon set (16px with 2px padding).
const svg = (d: string) => `<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${d}</svg>`;
const ICON_DIR = svg('<path d="M3 6V17C3 18.1046 3.89543 19 5 19H19C20.1046 19 21 18.1046 21 17V9C21 7.89543 20.1046 7 19 7H12.5352C12.2008 7 11.8886 6.8329 11.7031 6.5547L10.5937 4.8906C10.2228 4.3342 9.59834 4 8.92963 4H5C3.89543 4 3 4.89543 3 6Z"/>');
const ICON_FILE = svg('<path d="M12.1716 3H7C5.89543 3 5 3.89543 5 5V19C5 20.1046 5.89543 21 7 21H17C18.1046 21 19 20.1046 19 19V9.82843C19 9.29799 18.7893 8.78929 18.4142 8.41421L13.5858 3.58579C13.2107 3.21071 12.702 3 12.1716 3Z"/><path d="M13 3.5V7C13 8.10457 13.8954 9 15 9H18.5"/>');
const ICON_CHEVRON = svg('<path d="M10 16L13.2929 12.7071C13.6834 12.3166 13.6834 11.6834 13.2929 11.2929L10 8"/>');

const api = (op: string, q: Record<string, string>) => `files/${op}?${new URLSearchParams(q)}`;
const label = (p: string) => (p ? p.split('/').pop()! : 'Home');
const join = (a: string, b: string) => (a ? a + '/' : '') + b;
const size = (n: number) =>
  n < 1024 ? `${n} B` : n < 1 << 20 ? `${(n / 1024).toFixed(1)} KB` : n < 1 << 30 ? `${(n / 1048576).toFixed(1)} MB` : `${(n / 1073741824).toFixed(2)} GB`;
function when(t: number) {
  const d = new Date(t), now = new Date();
  if (d.toDateString() === now.toDateString()) return d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  return d.toLocaleDateString([], { month: 'short', day: 'numeric', year: d.getFullYear() === now.getFullYear() ? undefined : 'numeric' });
}

export function createFiles(app: App) {
  const panel = el('files');
  const list = el('files-list');
  const crumbs = el('files-crumbs');
  const uploads = el('files-uploads');
  const input = el<HTMLInputElement>('files-input');
  const drop = el('drop');
  const dropDir = el('drop-dir');
  const uploadBtn = el('files-upload');
  const footer = panel.querySelector('footer')!;

  let cfg: FilesConfig | null = null;
  let cwd = '';
  let seq = 0;
  let shown = '';
  let poll: ReturnType<typeof setInterval> | undefined;

  function empty(title: string, text: string) {
    list.innerHTML = `<div class="f-empty">${ICON_DIR}<b></b><span></span></div>`;
    list.querySelector('b')!.textContent = title;
    list.querySelector('.f-empty span')!.textContent = text;
  }

  function setup(f: FilesConfig | undefined) {
    cfg = f && (f.upload || f.download) ? f : null;
    app.tools.classList.toggle('files', !!cfg);
    if (!cfg) { close(); return; }
    if (!cwd) cwd = cfg.dir || '';
    dropDir.textContent = label(cwd);
    uploadBtn.style.display = cfg.upload ? '' : 'none';
    footer.textContent = [cfg.upload && 'Drop files anywhere on the screen to upload them here.', cfg.download && 'Click a file to download it.']
      .filter(Boolean)
      .join(' ');
  }
  function open() {
    if (!cfg) return;
    panel.classList.add('open');
    load(cwd);
    clearInterval(poll);
    // Pick up files that appear in the VM (e.g. browser downloads) while open.
    poll = setInterval(() => { if (!document.hidden) load(cwd, true); }, 3000);
  }
  function close() {
    panel.classList.remove('open');
    clearInterval(poll);
    poll = undefined;
  }

  async function load(path: string, quiet = false) {
    const n = ++seq;
    let r: Response | null = null;
    let d: (Listing & { error?: string }) | null = null;
    try { r = await fetch(api('list', { path }), { cache: 'no-store' }); d = await r.json(); } catch {}
    if (n !== seq) return;
    if (!r || !r.ok || !d) {
      if (quiet) return;
      shown = '';
      empty('Can’t open this folder', d?.error || 'The computer did not respond. Try again in a moment.');
      return;
    }
    cwd = d.path;
    dropDir.textContent = label(cwd);
    const key = cwd + '\n' + JSON.stringify(d.entries);
    if (key !== shown) { shown = key; render(d.entries); }
  }

  function render(entries: Entry[]) {
    crumbs.innerHTML = '';
    const parts = cwd ? cwd.split('/') : [];
    const crumb = (text: string, path: string) => {
      const a = document.createElement('a');
      a.textContent = text;
      a.onclick = () => load(path);
      crumbs.append(a);
    };
    crumb('Home', '');
    parts.forEach((p, i) => { crumbs.insertAdjacentHTML('beforeend', ICON_CHEVRON); crumb(p, parts.slice(0, i + 1).join('/')); });
    const top = list.scrollTop;
    list.innerHTML = '';
    // Folders by name, then files newest first (downloads land at the top).
    entries.sort((a, b) => Number(b.dir) - Number(a.dir) || (a.dir ? a.name.localeCompare(b.name) : b.mtime - a.mtime || a.name.localeCompare(b.name)));
    if (!entries.length) empty('No files yet', cfg?.upload ? 'Drop files anywhere on the screen, or use Upload, to add them here.' : 'Files in this folder will show up here.');
    for (const en of entries) {
      const row = document.createElement('div');
      row.className = 'f-row' + (en.dir ? ' dir' : '');
      row.innerHTML = `<span class="f-ico">${en.dir ? ICON_DIR : ICON_FILE}</span><span class="f-name"></span><span class="f-meta"></span>`;
      row.querySelector('.f-name')!.textContent = en.name;
      row.querySelector('.f-meta')!.textContent = en.dir ? when(en.mtime) : `${size(en.size)} · ${when(en.mtime)}`;
      row.title = en.dir ? `Open ${en.name}` : cfg?.download ? `Download ${en.name}` : en.name;
      const path = join(cwd, en.name);
      if (en.dir) row.onclick = () => load(path);
      else if (cfg?.download) row.onclick = () => download(path);
      else row.style.cursor = 'default';
      list.append(row);
    }
    list.scrollTop = top;
  }

  function download(path: string) {
    const a = document.createElement('a');
    a.href = api('download', { path });
    a.download = '';
    document.body.append(a);
    a.click();
    a.remove();
  }

  // Uploads run one at a time, each to the folder that was open when it was added.
  const queue: Upload[] = [];
  let busy = false;
  function status(row: HTMLElement, text: string, cls?: 'done' | 'err') {
    row.querySelector('.f-meta')!.textContent = text;
    if (cls) row.classList.add(cls);
  }
  function upload(picked: File[], skipped = 0) {
    if (!cfg?.upload) return;
    open();
    for (const f of picked) {
      const row = document.createElement('div');
      row.className = 'f-up';
      row.innerHTML = '<div class="f-up-top"><span class="f-name"></span><span class="f-meta">Waiting…</span></div><div class="f-bar"><i></i></div>';
      row.querySelector('.f-name')!.textContent = f.name;
      uploads.prepend(row);
      queue.push({ f, dir: cwd, row });
    }
    if (skipped) {
      const row = document.createElement('div');
      row.className = 'f-up err';
      row.innerHTML = '<div class="f-up-top"><span class="f-name"></span><span class="f-meta"></span></div>';
      row.querySelector('.f-name')!.textContent = `${skipped} folder${skipped > 1 ? 's' : ''} skipped`;
      status(row, 'Upload the files inside instead');
      uploads.prepend(row);
      setTimeout(() => row.remove(), 8000);
    }
    pump();
  }
  function pump() {
    const u = busy ? undefined : queue.shift();
    if (!u) return;
    busy = true;
    const bar = u.row.querySelector<HTMLElement>('.f-bar i')!;
    const xhr = new XMLHttpRequest();
    const finish = () => { busy = false; pump(); };
    xhr.open('PUT', api('upload', { path: u.dir, name: u.f.name }));
    // Required by the server: browsers only send custom headers cross-origin after a
    // CORS preflight, which it never answers, so other sites can't upload.
    xhr.setRequestHeader('X-Voxl-Upload', '1');
    xhr.upload.onprogress = (e) => {
      if (!e.lengthComputable) return;
      bar.style.width = `${(e.loaded / e.total) * 100}%`;
      status(u.row, `${Math.floor((e.loaded / e.total) * 100)}% of ${size(e.total)}`);
    };
    xhr.onload = () => {
      let d: { path?: string; error?: string } = {};
      try { d = JSON.parse(xhr.responseText); } catch {}
      if (xhr.status === 200) {
        const saved = (d.path || '').split('/').pop();
        if (saved && saved !== u.f.name) u.row.querySelector('.f-name')!.textContent = saved;
        status(u.row, `Uploaded to ${label(u.dir)}`, 'done');
        if (u.dir === cwd) load(cwd, true);
        setTimeout(() => u.row.remove(), 6000);
      } else status(u.row, d.error || `Failed (${xhr.status})`, 'err');
      finish();
    };
    xhr.onerror = () => { status(u.row, 'Connection lost', 'err'); finish(); };
    xhr.send(u.f);
  }

  el('files-btn').addEventListener('click', () => (panel.classList.contains('open') ? close() : open()));
  el('files-close').onclick = close;
  el('files-refresh').onclick = () => load(cwd);
  uploadBtn.onclick = () => input.click();
  input.onchange = () => { upload([...(input.files ?? [])]); input.value = ''; };
  // Esc closes the panel unless the remote screen has focus (it goes to the desktop then).
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && e.target !== app.kbd && e.target !== app.surface) close(); });

  // Always swallow dropped files: otherwise the browser navigates away to open them.
  const hasFiles = (e: DragEvent) => !!e.dataTransfer && [...e.dataTransfer.types].includes('Files');
  let depth = 0;
  window.addEventListener('dragenter', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    if (cfg?.upload && ++depth === 1) drop.classList.add('show');
  });
  window.addEventListener('dragover', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.dataTransfer!.dropEffect = cfg?.upload ? 'copy' : 'none';
  });
  window.addEventListener('dragleave', (e) => {
    if (hasFiles(e) && --depth <= 0) { depth = 0; drop.classList.remove('show'); }
  });
  window.addEventListener('drop', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    depth = 0;
    drop.classList.remove('show');
    const picked: File[] = [];
    let skipped = 0;
    for (const it of e.dataTransfer!.items) {
      if (it.kind !== 'file') continue;
      if (it.webkitGetAsEntry?.()?.isDirectory) { skipped++; continue; }
      const f = it.getAsFile();
      if (f) picked.push(f);
    }
    upload(picked, skipped);
  });

  return { setup };
}
