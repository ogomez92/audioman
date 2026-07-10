use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // prism is vendored in-repo (vendor/prism/) so a fresh clone builds with no
    // external SDK on some machine-specific path. prism.lib is the x64 dynamic
    // import library; prism.dll is copied next to the exe below.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let prism_dir = manifest_dir.join("vendor").join("prism");

    println!("cargo:rustc-link-search=native={}", prism_dir.display());
    println!("cargo:rustc-link-lib=prism");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    if let Some(target_dir) = out_dir.ancestors().nth(3) {
        let src = prism_dir.join("prism.dll");
        let dst = target_dir.join("prism.dll");
        if src.exists() {
            let _ = fs::copy(&src, &dst);
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/prism/prism.lib");
    // The link-search path above is absolute; rerun if the repo moves, or the
    // cached output keeps pointing at the old checkout location.
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
}
