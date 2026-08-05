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
    println!("cargo:rerun-if-changed=src/shim.cpp");

    let mut build = cc::Build::new();
    build.cpp(true).file("src/shim.cpp").flag_if_supported("-std=c++17").opt_level(3);

    for f in pkg_config(&["--cflags", "poppler-cpp"]) {
        if let Some(inc) = f.strip_prefix("-I") {
            build.include(inc);
        } else {
            build.flag(&f);
        }
    }
    build.compile("gleanshim");

    for f in pkg_config(&["--libs", "poppler-cpp"]) {
        if let Some(l) = f.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={l}");
        } else if let Some(p) = f.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={p}");
        }
    }
    println!("cargo:rustc-link-lib=stdc++");
}
