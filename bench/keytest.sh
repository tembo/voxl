#!/usr/bin/env bash
# Types through the web client into an isolated xev window and prints what X apps see.
cd "$(dirname "$(readlink -f "$0")")"
nix-shell -p xorg.xev --run 'DISPLAY=:1 xev -geometry 700x500+1700+700 -event keyboard > /tmp/xev.log 2>&1 & echo $! > /tmp/xev.pid; sleep 1.5; DISPLAY=:1 xdotool search --name "Event Tester" | head -1 | xargs -I{} env DISPLAY=:1 xdotool windowraise {}' >/dev/null 2>&1
sleep 0.5
node keytest.mjs "$@"
sleep 0.5
kill $(cat /tmp/xev.pid) 2>/dev/null
grep -A4 "KeyPress" /tmp/xev.log | grep -oE "keysym 0x[0-9a-f]+, [A-Za-z_0-9]+|XLookupString gives [0-9]+ bytes: (\(.*\) )?\"[^\"]*\"" | paste - - | sed 's/XLookupString gives [0-9]* bytes: //' | awk -F'\t' '{print $1, $2}' | tr '\n' ';'
echo
