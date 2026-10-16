fn main() {
    let x264 = pkg_config::probe_library("x264").expect("x264");
    let others = ["libyuv", "opus", "libpulse-simple"].map(|l| pkg_config::probe_library(l).expect(l));
    // Embed library dirs so the binary runs outside the build shell.
    for lib in std::iter::once(&x264).chain(others.iter()) {
        for p in &lib.link_paths {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
        }
    }
    let mut b = bindgen::Builder::default()
        .header_contents("wrap.h", "#include <stdint.h>\n#include <x264.h>\n")
        .allowlist_function("x264_.*")
        .allowlist_type("x264_.*")
        .allowlist_var("X264_.*")
        .layout_tests(false);
    for p in &x264.include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    b.generate().expect("bindgen x264").write_to_file(out.join("x264.rs")).unwrap();
    bundle_web(&out);
    println!("cargo:rerun-if-changed=build.rs");
}

/// Bundles the TypeScript web client (web/src) into OUT_DIR/web, which the server
/// embeds. Needs esbuild on PATH (or ESBUILD); shell.nix and default.nix provide it.
fn bundle_web(out: &std::path::Path) {
    let esbuild = std::env::var("ESBUILD").unwrap_or_else(|_| "esbuild".into());
    let status = std::process::Command::new(&esbuild)
        .args([
            "client=web/src/client/main.ts",
            "worker=web/src/worker.ts",
            "audio=web/src/worklets/audio.ts",
            "mic=web/src/worklets/mic.ts",
            "--bundle",
            "--format=iife",
            "--target=es2022",
            "--minify",
            "--log-level=warning",
        ])
        .arg(format!("--outdir={}", out.join("web").display()))
        .status()
        .unwrap_or_else(|e| panic!("running {esbuild} to bundle web/src failed ({e}); use nix-shell, or set ESBUILD"));
    assert!(status.success(), "esbuild failed to bundle web/src");
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-env-changed=ESBUILD");
}
