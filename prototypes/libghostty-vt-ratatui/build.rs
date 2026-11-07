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

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("Cargo sets target OS");
    let (library_file, library_name) = match target_os.as_str() {
        "linux" => ("libghostty-vt.so", "ghostty-vt"),
        "macos" => ("libghostty-vt.dylib", "ghostty-vt"),
        // Ghostty's Windows package uses a GNU-ABI DLL and provides an import
        // library that MSVC applications can link.
        "windows" => ("ghostty-vt.lib", "ghostty-vt"),
        other => panic!("unsupported libghostty-vt host target: {other}"),
    };
    assert!(
        lib.join(library_file).exists(),
        "missing {library_file} under {}",
        lib.display()
    );

    cc::Build::new()
        .file("src/ghostty_shim.c")
        .include(include)
        .std("c11")
        .warnings(true)
        .compile("rootglass_ghostty_vt_shim");

    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib={library_name}");
    if target_os != "windows" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    }
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_PREFIX");
    println!("cargo:rerun-if-changed=src/ghostty_shim.c");
}
