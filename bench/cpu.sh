#!/usr/bin/env bash
# cpu.sh <pid> <secs> : average CPU cores used by a process tree root (all threads)
t() { awk '{print $14+$15}' /proc/$1/stat; }
a=$(t $1); sleep $2; b=$(t $1)
python3 -c "print(f'{($b-$a)/100/$2:.2f} cores')"
