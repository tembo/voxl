{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [pkg-config clang rustPlatform.bindgenHook];
  buildInputs = with pkgs; [x264 libyuv libopus libpulseaudio xorg.libX11 xorg.libXext xorg.libxcb];
}
