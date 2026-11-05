# moonlight-embedded with dump.patch: `-platform fake` writes each frame it receives to
# the FIFO MOONLIGHT_DUMP names, for mlclient.py. nix-build bench/moonlight
{ pkgs ? import <nixpkgs> {} }:
pkgs.moonlight-embedded.overrideAttrs (o: { patches = (o.patches or []) ++ [ ./dump.patch ]; })
