"""perfab.py: interleaved A/B of voxl builds on the benchmark display, measuring the
native client's FPS and latency together with the server's CPU (voxl + Xvfb) and a
per-thread breakdown of voxl, in the same window.

Usage: perfab.py [--reps N] [--secs S] [--workloads scroll,anim1080,animfull]
                 [--net lan,10/40] name=/path/to/voxl[:extra args] ...
Start Xvfb and openbox on BENCH_DISPLAY (default :2) first; voxl is (re)started per run
on port 6902. Appends JSON lines to $OUT (default /tmp/perfab.jsonl)."""
import argparse, json, os, re, subprocess, sys, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BD = os.environ.get('BENCH_DISPLAY', ':2')
PORT = 6902
HZ = os.sysconf('SC_CLK_TCK')


def pid_of(pat):
    out = subprocess.run(['pgrep', '-f', '--', pat], capture_output=True, text=True).stdout.split()
    return int(out[0]) if out else None


def ticks(pid):
    with open(f'/proc/{pid}/stat') as f:
        s = f.read().rsplit(')', 1)[1].split()
    return int(s[11]) + int(s[12])


def thread_ticks(pid):
    out = {}
    for tid in os.listdir(f'/proc/{pid}/task'):
        try:
            with open(f'/proc/{pid}/task/{tid}/stat') as f:
                raw = f.read()
            name = raw[raw.index('(') + 1:raw.rindex(')')]
            s = raw.rsplit(')', 1)[1].split()
            out[(tid, name)] = int(s[11]) + int(s[12])
        except (FileNotFoundError, ProcessLookupError):
            pass
    return out


def stats():
    with urllib.request.urlopen(f'http://127.0.0.1:{PORT}/stats', timeout=2) as r:
        return json.loads(r.read())


def start_voxl(binary, extra):
    subprocess.run([f'{HERE}/env/killpat.sh', f'--port={PORT}'], capture_output=True)
    time.sleep(1)
    env = dict(os.environ, DISPLAY=BD, RUST_LOG=os.environ.get('RUST_LOG', 'info'))
    log = open('/tmp/voxl-bench.log', 'w')
    args = [binary, f'--port={PORT}', '--framerate=60', '--preset=ultrafast', '--max-kbps=30000',
            '--audio-enabled=false'] + extra
    p = subprocess.Popen(args, env=env, stdout=log, stderr=log, stdin=subprocess.DEVNULL, start_new_session=True)
    for _ in range(50):
        try:
            stats()
            return p
        except Exception:
            time.sleep(0.2)
    raise SystemExit(f'voxl did not start: {open("/tmp/voxl-bench.log").read()}')


def workload(wl, secs):
    env = dict(os.environ, DISPLAY=BD, BENCH_DISPLAY=BD)
    if wl == 'scroll':
        env.setdefault('CHROMIUM', subprocess.run(['which', 'chromium'], capture_output=True, text=True).stdout.strip())
        return subprocess.Popen(['node', f'{HERE}/scrolltarget.mjs', str(secs)], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL), (96, 265)
    if wl == 'anim1080':
        return subprocess.Popen([f'{HERE}/loadgen', '1920', '1080', '60', str(secs), '300', '150'], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL), (300, 150)
    if wl == 'animfull':
        return subprocess.Popen([f'{HERE}/loadgen', '2560', '1600', '60', str(secs), '0', '0'], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL), (0, 0)
    if wl == 'animsmall':
        # A small update (a spinner, typing): 512x288 of the screen.
        return subprocess.Popen([f'{HERE}/loadgen', '512', '288', '60', str(secs), '600', '400'], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL), (600, 400)
    if wl == 'anim720':
        return subprocess.Popen([f'{HERE}/loadgen', '1280', '720', '60', str(secs), '600', '400'], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL), (600, 400)
    raise SystemExit(f'unknown workload {wl}')


def run_one(name, binary, extra, wl, net, secs):
    vp = start_voxl(binary, extra)
    xpid = pid_of(f'Xvfb {BD} ')
    wp, (mx, my) = workload(wl, secs + 14)
    time.sleep(4)
    target = PORT
    shaper = None
    if net != 'lan':
        mbps, rtt = net.split('/')
        target = PORT + 1000
        shaper = subprocess.Popen([f'{HERE}/py.sh', 'shaper.py', str(target), f'localhost:{PORT}', mbps, rtt],
                                  cwd=HERE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  start_new_session=True)
        time.sleep(3)
    cli = subprocess.Popen([f'{HERE}/py.sh', 'nclient.py', 'voxl', f'localhost:{target}', str(secs), str(mx), str(my)],
                           cwd=HERE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    # nix-shell start + the client's 4 s warmup.
    time.sleep(7)
    win = max(secs - 4, 4)
    v0, x0, t0, s0 = ticks(vp.pid), ticks(xpid), thread_ticks(vp.pid), stats()
    w0 = time.time()
    time.sleep(win)
    v1, x1, t1, s1 = ticks(vp.pid), ticks(xpid), thread_ticks(vp.pid), stats()
    el = time.time() - w0
    out, _ = cli.communicate(timeout=secs + 60)
    if shaper:
        subprocess.run([f'{HERE}/env/killpat.sh', f'shaper.py {target}'], capture_output=True)
    try:
        res = json.loads(out.strip().splitlines()[-1])
    except Exception:
        res = None
    threads = {}
    for k, v in t1.items():
        nm = re.sub(r'\d+$', '#', k[1])
        threads[nm] = threads.get(nm, 0) + (v - t0.get(k, 0)) / HZ / el
    d = {k: s1[k] - s0[k] for k in s1 if isinstance(s1[k], int)}
    n = max(d['encoded'], 1)
    srv = {
        'captured_fps': round(d['captured'] / el, 1), 'encoded_fps': round(d['encoded'] / el, 1),
        'enc_ms': round(d['encode_us_total'] / n / 1000, 2), 'conv_ms': round(d['convert_us_total'] / n / 1000, 2),
        'send_lat_ms': round(d['send_lat_us_total'] / n / 1000, 2), 'mbps': round(d['bytes'] * 8 / el / 1e6, 2),
        'keyframes': d['keyframes'], 'flow_blocked': d['flow_blocked'],
        'qp': round(d['qp_total'] / d['qp_frames'], 1) if d.get('qp_frames') else None,
    }
    rec = {'name': name, 'workload': wl, 'net': net,
           'voxl_cores': round((v1 - v0) / HZ / el, 3), 'xvfb_cores': round((x1 - x0) / HZ / el, 3),
           'threads': {k: round(v, 3) for k, v in sorted(threads.items(), key=lambda x: -x[1]) if v >= 0.005},
           'srv': srv, 'r': res}
    wp.wait(timeout=secs + 60)
    vp.terminate()
    vp.wait()
    return rec


def summary(rec):
    r = rec['r'] or {}
    f, l = r.get('uniqueFps', {}), r.get('latencyMs', {})
    return (f"{rec['name']:>10} {rec['workload']:>8} {rec['net']:>6}  fps {f.get('avg')} (min {f.get('min')})  "
            f"lat {l.get('p50')}/{l.get('p90')}/{l.get('p99')}  cores voxl {rec['voxl_cores']:.2f} + X {rec['xvfb_cores']:.2f}"
            f" = {rec['voxl_cores'] + rec['xvfb_cores']:.2f}  enc {rec['srv']['enc_ms']}ms conv {rec['srv']['conv_ms']}ms"
            f" {rec['srv']['mbps']}Mbps" + (f" qp {rec['srv']['qp']}" if rec['srv'].get('qp') else ''))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--reps', type=int, default=2)
    ap.add_argument('--secs', type=int, default=16)
    ap.add_argument('--workloads', default='anim1080,animfull,scroll')
    ap.add_argument('--net', default='lan')
    ap.add_argument('builds', nargs='+')
    a = ap.parse_args()
    builds = []
    for b in a.builds:
        name, rest = b.split('=', 1)
        path, _, extra = rest.partition(':')
        builds.append((name, path, extra.split() if extra else []))
    out = open(os.environ.get('OUT', '/tmp/perfab.jsonl'), 'a')
    for rep in range(a.reps):
        for wl in a.workloads.split(','):
            for net in a.net.split(','):
                for name, path, extra in (builds if rep % 2 == 0 else builds[::-1]):
                    rec = run_one(name, path, extra, wl, net, a.secs)
                    rec['rep'] = rep
                    out.write(json.dumps(rec) + '\n')
                    out.flush()
                    print(summary(rec), flush=True)
                    if os.environ.get('THREADS'):
                        print('   ', rec['threads'], flush=True)
                    time.sleep(2)


if __name__ == '__main__':
    main()
