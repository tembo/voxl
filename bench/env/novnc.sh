#!/usr/bin/env bash
# noVNC as E2B Desktop and Daytona ship it: x11vnc on the benchmark display (E2B's
# flags) behind websockify serving noVNC's web client, port 6932 (VNC on 5902).
cd "$(dirname "$(readlink -f "$0")")"
X11VNC=${X11VNC:-x11vnc}; NOVNC=${NOVNC:?path to the novnc package}; WEBSOCKIFY=${WEBSOCKIFY:-websockify}
./killpat.sh "rfbport 5902" >/dev/null 2>&1; ./killpat.sh "6932 localhost:5902" >/dev/null 2>&1; sleep 0.5
"$X11VNC" -bg -display ${BENCH_DISPLAY:-:2} -forever -wait 50 -shared -rfbport 5902 -nopw -o /tmp/x11vnc-bench.log >/dev/null 2>&1
setsid nohup "$WEBSOCKIFY" --web "$NOVNC/share/webapps/novnc" 6932 localhost:5902 >/tmp/novnc-bench.log 2>&1 < /dev/null &
for i in $(seq 1 50); do ss -ltn | grep -q ":6932 " && break; sleep 0.2; done
