use std::process::Command;

fn pkg_config(args: &[&str]) -> Vec<String> {
    let out = Command::new("pkg-config")
        .args(args)
        .output()
        .expect("pkg-config not found — install pkgconf and poppler-cpp");
    if !out.status.success() {
        panic!(
            "pkg-config {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect()
}

fn main() {
    // Every C++ source must be listed: cargo will not re-run this script for a
    // file it was not told about, and the stale object links silently.
    for f in ["src/shim.cpp", "src/images.cpp", "build.rs"] {
        println!("cargo:rerun-if-changed={f}");
    }

    let mut build = cc::Build::new();
    build.cpp(true).file("src/shim.cpp").file("src/images.cpp").flag_if_supported("-std=c++20").opt_level(3);

    for f in pkg_config(&["--cflags", "poppler-cpp", "poppler", "zlib"]) {
        if let Some(inc) = f.strip_prefix("-I") {
            build.include(inc);
        } else {
            build.flag(&f);
        }
    }
    build.compile("gleanshim");

    // images.cpp uses poppler's core OutputDev, which pkg-config exposes as
    // `poppler` (poppler-cpp is only the stable wrapper), plus zlib for PNG.
    for f in pkg_config(&["--libs", "poppler-cpp", "poppler", "zlib"]) {
        if let Some(l) = f.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={l}");
        } else if let Some(p) = f.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={p}");
        }
    }
    println!("cargo:rustc-link-lib=stdc++");
}
