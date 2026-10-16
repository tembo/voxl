#!/usr/bin/env bash
# suite.sh : streaming servers across configs, workloads and links on the benchmark displays.
# Appends JSON lines to $OUT. Start the servers under test first:
#   selkies: env/xvfb.sh, env/openbox.sh, env/selkies.sh (display :2, port 6912)
#   voxl:    env/xvfb.sh, env/openbox.sh, env/voxl.sh    (display :2, port 6902)
#   kasmvnc: env/kasmvnc.sh, BENCH_DISPLAY=:5 env/openbox.sh (display :5, port 6922)
# Configs: py_picom py_nopicom (selkies), voxl voxl_picom, kasm kasm_picom.
# *_picom runs picom from env/picom-start.sh (PICOM, PICOM_CONF; selkies' picom config
# by default, PICOM_CONF=picom-damage.conf for today's Computer preview).
cd "$(dirname "$(readlink -f "$0")")"
OUT=${OUT:-/tmp/suite.jsonl}
SECS=${SECS:-20}
run_one() { # config client port workload net
  local cfg=$1 cli=$2 port=$3 wl=$4 net=$5 mbps rtt mx my target=$3
  case $wl in
    scroll) mx=96; my=265; (node scrolltarget.mjs $((SECS+14)) >/dev/null 2>&1 &) ;;
    anim1080) mx=300; my=150; (DISPLAY=$BD ./loadgen 1920 1080 60 $((SECS+12)) 300 150 >/dev/null 2>&1 &) ;;
    animfull) mx=0; my=0; (DISPLAY=$BD ./loadgen 2560 1600 60 $((SECS+12)) 0 0 >/dev/null 2>&1 &) ;;
  esac
  if [ "$net" != "lan" ]; then
    mbps=${net%/*}; rtt=${net#*/}; target=$((port+1000))
    ./py.sh shaper.py $target localhost:$port $mbps $rtt >/dev/null 2>&1 &
  fi
  sleep 4
  case $cli in
    kasmvnc) res=$(./py.sh kclient.py localhost:$target $SECS $mx $my 2>/dev/null | tail -1) ;;
    *) res=$(./py.sh nclient.py $cli localhost:$target $SECS $mx $my 2>/dev/null | tail -1) ;;
  esac
  [ "$net" != "lan" ] && env/killpat.sh "shaper.py $target" >/dev/null
  echo "{\"config\":\"$cfg\",\"workload\":\"$wl\",\"net\":\"$net\",\"r\":${res:-null}}" | tee -a $OUT
  sleep 9
}
for cfg in ${CONFIGS:-py_picom py_nopicom voxl}; do
  case $cfg in
    kasm*) cli=kasmvnc; port=6922; BD=${KASM_DISPLAY:-:5} ;;
    voxl*) cli=voxl; port=6902; BD=${BD_X:-:2} ;;
    *) cli=selkies; port=6912; BD=${BD_X:-:2} ;;
  esac
  export BD BENCH_DISPLAY=$BD
  env/picom-stop.sh
  case $cfg in *_picom) env/picom-start.sh ;; esac
  sleep 2
  for wl in ${WORKLOADS:-scroll anim1080 animfull}; do
    for net in ${NETS:-lan 10/40 5/60}; do
      [ $wl = animfull ] && [ $net != lan ] && continue
      run_one $cfg $cli $port $wl $net
    done
  done
done
env/picom-stop.sh
