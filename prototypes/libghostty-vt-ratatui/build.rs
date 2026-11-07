use std::env;
use std::path::PathBuf;

fn main() {
    let prefix = env::var_os("GHOSTTY_VT_PREFIX")
        .map(PathBuf::from)
        .expect("set GHOSTTY_VT_PREFIX to a libghostty-vt installation prefix");
    let include = prefix.join("include");
    let lib = prefix.join("lib");
    assert!(
        include.join("ghostty/vt.h").is_file(),
        "missing libghostty-vt headers under {}",
        include.display()
    );
    assert!(
        lib.join("libghostty-vt.so").exists(),
        "missing libghostty-vt shared library under {}",
        lib.display()
    );

    cc::Build::new()
        .file("src/ghostty_shim.c")
        .include(include)
        .flag_if_supported("-std=c11")
        .warnings(true)
        .compile("rootglass_ghostty_vt_shim");

    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_PREFIX");
    println!("cargo:rerun-if-changed=src/ghostty_shim.c");
}
