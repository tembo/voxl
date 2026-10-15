#!/usr/bin/env bash
# killpat.sh <substring> : kill processes whose cmdline contains substring, excluding this script's ancestry.
pat="$1"; skip=" "; p=$$
while [ "$p" -gt 1 ]; do skip="$skip$p "; p=$(awk '{print $4}' /proc/$p/stat 2>/dev/null || echo 1); done
for d in /proc/[0-9]*; do pid=${d#/proc/}; case "$skip" in *" $pid "*) continue;; esac
  tr '\0' ' ' < $d/cmdline 2>/dev/null | grep -qF -- "$pat" && kill $pid 2>/dev/null && echo "killed $pid"; done; true
