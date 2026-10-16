#!/usr/bin/env bash
exec nix-shell -p "python312.withPackages(ps: [ps.av ps.websockets ps.pillow ps.numpy])" --run "python3 $*"
