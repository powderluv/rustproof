//! Link the RISC-V nucleus image with its custom layout: use link.ld and force a
//! non-PIE executable (OpenSBI loads it at a fixed physical address).
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let script = manifest.join("link.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    println!("cargo:rustc-link-arg-bins=-no-pie");
    println!("cargo:rerun-if-changed=link.ld");

    // Stage the riscv-init user ELF for embedding (tools/run-qemu-riscv.sh copies the real
    // program here; guarantee the file exists so include_bytes! compiles).
    let user_elf = manifest.join("user.elf");
    if !user_elf.exists() {
        std::fs::write(&user_elf, b"").expect("create placeholder user.elf");
    }
    println!("cargo:rerun-if-changed=user.elf");
}
