"""Sequential A/B measurements against running, isolated voxl/Xvfb/labwc servers.
Usage: compare.py wayland_port wayland_voxl_pid labwc_pid x11_port x11_voxl_pid Xvfb_pid
Set WAYLAND_DISPLAY, XDG_RUNTIME_DIR and DISPLAY for the two source sessions.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import time
import urllib.request

ROOT = Path(__file__).resolve().parent
OUT = ROOT / "results"
OUT.mkdir(exist_ok=True)
settings = {"wayland": sys.argv[1:4], "x11": sys.argv[4:7]}


def cpu(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")",1)[1].split()
    return (int(fields[11])+int(fields[12])) / os.sysconf("SC_CLK_TCK")


def stats(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/stats") as response:
        return json.load(response)


for trial in range(2):
    for mode in ["idle", "sparse", "scroll"]:
        # Reverse backend order on the second pass to reduce warmup/order bias.
        for backend in (["x11", "wayland"] if trial == 0 else ["wayland", "x11"]):
            port, server, compositor = settings[backend]
            label = f"{trial+1}-{mode}-{backend}"
            with (OUT/f"{label}-source.log").open("w") as log:
                source = subprocess.Popen([str(ROOT/"build/loadgen"),backend,mode,"2560","1600","60","45",label],stdout=log,stderr=log)
            try:
                time.sleep(1)
                assert source.poll() is None, "load generator exited"
                pids = [server, compositor, str(source.pid)]
                before = stats(port)
                assert not before["clients"], "measurement requires no other viewers"
                t0, c0 = time.monotonic(), [cpu(p) for p in pids]
                time.sleep(4)
                c1, t1, after = [cpu(p) for p in pids], time.monotonic(), stats(port)
                unattended = {
                    "seconds": t1-t0,
                    "cpu_percent": [round((b-a)/(t1-t0)*100,2) for a,b in zip(c0,c1)],
                    "encoded": after["encoded"]-before["encoded"],
                    "damage_events": after["damage_events"]-before["damage_events"],
                }
                result = subprocess.run([sys.executable,str(ROOT/"measure.py"),f"127.0.0.1:{port}","12",*pids],capture_output=True,text=True,timeout=30,check=True)
                sample = {"backend":backend,"mode":mode,"trial":trial+1,"unattended":unattended,"streaming":json.loads(result.stdout)}
                (OUT/f"{label}.json").write_text(json.dumps(sample,indent=2)+"\n")
                print(json.dumps(sample),flush=True)
            finally:
                source.terminate()
                source.wait()
            time.sleep(.3)
