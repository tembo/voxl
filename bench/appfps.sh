#!/usr/bin/env bash
# appfps.sh "<chromium flags>" : X damage rate (= presented frames/s) while Chromium smooth-scrolls
cd "$(dirname "$(readlink -f "$0")")"
sed "s#'--disable-gpu'\]#...(process.env.CFLAGS||'').split(' ').filter(Boolean)]#" scrolltarget.mjs > /tmp/st2.mjs
(CFLAGS="$1" node /tmp/st2.mjs 16 >/dev/null 2>&1 &)
sleep 3; (./py.sh nclient.py voxl localhost:6902 12 > /dev/null 2>&1 &); sleep 6
a=$(curl -s localhost:6902/stats | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['damage_events'])"); sleep 5
b=$(curl -s localhost:6902/stats | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['damage_events'])")
echo "[$1] presented ~$(( (b-a)/5 )) fps"; sleep 7
