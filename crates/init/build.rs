//! Link the `init` user binary with its fixed ring-3 layout: use `link.ld`
//! (base 0x80_0000_0000) and force a non-PIE ET_EXEC so the kernel loader can
//! map the segments at their link-time virtual addresses and jump to `_start`.
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let script = manifest.join("link.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    println!("cargo:rustc-link-arg-bins=-no-pie");
    println!("cargo:rerun-if-changed=link.ld");
}
