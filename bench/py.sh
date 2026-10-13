#!/usr/bin/env bash
exec nix-shell -p "python312.withPackages(ps: [ps.av ps.websockets])" --run "python3 $*"
