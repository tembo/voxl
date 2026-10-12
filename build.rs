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
    println!("cargo:rerun-if-changed=build.rs");
}
