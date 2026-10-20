"""Daytona sandbox for the benchmarks: a default sandbox with a 2560x1600 desktop
(x11vnc + noVNC), the loadgen animation built in it and a self-scrolling copy of
scroll.html, measured through Daytona's signed preview proxy with kclient.py.
Needs the `daytona` SDK (pip) and DAYTONA_API_KEY. The sandbox id is kept in
/tmp/daytona-bench.id between commands.

  daytona_bench.py up                      create it, start the desktop, print the kclient env
  daytona_bench.py run '<shell command>'   run a command in it (e.g. start a workload)
  daytona_bench.py down                    delete it

Workloads (DISPLAY=:0 in the sandbox):
  anim1080: cd /tmp && nohup ./loadgen 1920 1080 60 40 300 150 &       (marker 300 150)
  animfull: cd /tmp && nohup ./loadgen 2560 1600 60 40 0 0 &           (marker 0 0)
  scroll:   nohup chromium --no-sandbox --kiosk --disable-gpu --force-device-scale-factor=1.5
              --user-data-dir=/tmp/chr file:///tmp/scroll-auto.html &  (marker 0 0)
Then: KC_URL=... KC_HEADERS=... ./py.sh kclient.py daytona 20 <markerX> <markerY>;
for slow links put shaper.py in front (`shaper.py 7990 <host>:443 ...`, KC_CONNECT=127.0.0.1:7990)."""
import os, shlex, sys
from daytona import CreateSandboxFromSnapshotParams, Daytona

HERE = os.path.dirname(os.path.abspath(__file__))
ID = '/tmp/daytona-bench.id'
# scroll.html, scrolled by the page itself one step per frame (like scrolltarget.mjs).
AUTOSCROLL = ('<script>addEventListener("load",()=>{let d=1;const step=()=>{scrollBy(0,6*d);'
              'if(scrollY+innerHeight>=document.body.scrollHeight-2||scrollY<=0)d=-d;'
              'requestAnimationFrame(step)};setTimeout(()=>requestAnimationFrame(step),1500)})</script>')
# CPU (cores) of Xvfb + x11vnc + websockify over 10 s: `run 'bash /tmp/cpu.sh'`.
CPU = '''p="$(pgrep -x Xvfb) $(pgrep -x x11vnc) $(pgrep -f '^/usr/bin/python3 /usr/bin/websockify')"
t() { local s=0; for i in $p; do s=$((s + $(awk '{print $14+$15}' /proc/$i/stat))); done; echo $s; }
a=$(t); sleep 10; b=$(t); python3 -c "print(round(($b-$a)/$(getconf CLK_TCK)/10, 2))"
'''

d = Daytona()
cmd = sys.argv[1] if len(sys.argv) > 1 else 'up'
if cmd == 'up':
    sb = d.create(CreateSandboxFromSnapshotParams(env_vars={'VNC_RESOLUTION': '2560x1600'},
                                                  labels={'purpose': 'voxl-bench'}, auto_stop_interval=60), timeout=300)
    open(ID, 'w').write(sb.id)
    sb.computer_use.start()
    sb.fs.upload_file(open(os.path.join(HERE, 'loadgen.c'), 'rb').read(), '/tmp/loadgen.c')
    page = open(os.path.join(HERE, 'scroll.html')).read()
    sb.fs.upload_file((page.replace('</body>', AUTOSCROLL + '</body>') if '</body>' in page else page + AUTOSCROLL).encode(), '/tmp/scroll-auto.html')
    sb.fs.upload_file(CPU.encode(), '/tmp/cpu.sh')
    print(sb.process.exec('cd /tmp && gcc -O2 -o loadgen loadgen.c -lX11 -lXext && nproc && cat /sys/fs/cgroup/cpu.max', timeout=120).result)
    p = sb.get_preview_link(6080)
    host = p.url.split('://', 1)[1]
    print(f"export KC_URL='wss://{host}/websockify' KC_HEADERS='{{\"x-daytona-preview-token\": \"{p.token}\"}}'")
elif cmd == 'run':
    sb = d.get(open(ID).read().strip())
    print(sb.process.exec("bash -c " + shlex.quote(sys.argv[2]), timeout=120).result)
elif cmd == 'down':
    d.delete(d.get(open(ID).read().strip()))
    os.remove(ID)
