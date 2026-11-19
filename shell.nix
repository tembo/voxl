# Development shell: the native libraries, plus esbuild and TypeScript for the web
# client. `nix-shell --arg withRust true` also brings Rust (rustc, cargo, clippy and
# rustfmt from nixpkgs), as CI does; otherwise your own toolchain is used.
{
  pkgs ? import <nixpkgs> {},
  withRust ? false,
}:
pkgs.mkShell {
  nativeBuildInputs = with pkgs;
    [pkg-config clang rustPlatform.bindgenHook esbuild typescript]
    ++ lib.optionals withRust [rustc cargo clippy rustfmt];
  # x264 with patches/x264, as default.nix builds voxl.
  buildInputs = with pkgs; [(import ./x264.nix {inherit x264;}) libyuv libopus libpulseaudio xorg.libX11 xorg.libXext xorg.libxcb];
}
