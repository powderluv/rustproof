//! Link the RISC-V nucleus image with its custom layout: use link.ld and force a
//! non-PIE executable (OpenSBI loads it at a fixed physical address).
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let script = manifest.join("link.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    println!("cargo:rustc-link-arg-bins=-no-pie");
    println!("cargo:rerun-if-changed=link.ld");
}
