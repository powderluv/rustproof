//! Link the `riscv-init` U-mode user binary with its fixed layout: use `link.ld`
//! (base 0x1_0000_0000, i.e. 4 GiB) and force a non-PIE ET_EXEC so the kernel
//! loader can map the segments at their link-time virtual addresses and jump to
//! `_start`. Mirrors the x86 `init` build script for the RISC-V target.
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let script = manifest.join("link.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    println!("cargo:rustc-link-arg-bins=-no-pie");
    println!("cargo:rerun-if-changed=link.ld");
}
