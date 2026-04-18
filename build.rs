use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let prism_root = PathBuf::from("t:/code/prism/prism-windows-x64/dynamic/release");
    let lib_dir = prism_root.join("lib");
    let bin_dir = prism_root.join("bin");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=prism");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    if let Some(target_dir) = out_dir.ancestors().nth(3) {
        let src = bin_dir.join("prism.dll");
        let dst = target_dir.join("prism.dll");
        if src.exists() {
            let _ = fs::copy(&src, &dst);
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
}
