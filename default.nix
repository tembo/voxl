{
  lib,
  rustPlatform,
  pkg-config,
  x264,
  libyuv,
  libopus,
  libpulseaudio,
  xorg,
}:
rustPlatform.buildRustPackage {
  pname = "voxl";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./build.rs
      ./src
      ./web
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    x264
    libyuv
    libopus
    libpulseaudio
    xorg.libxcb
  ];

  meta = {
    description = "Low-latency X11 desktop streaming server (H.264 over WebSocket) for the Computer preview";
    license = lib.licenses.mit;
    mainProgram = "voxl";
    platforms = ["x86_64-linux"];
  };
}
