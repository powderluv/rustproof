//! Link the nucleus image with its custom layout: use linker.ld and force a
//! non-PIE executable (PVH loads at fixed physical addresses).
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let script = manifest.join("linker.ld");
    println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    println!("cargo:rustc-link-arg-bins=-no-pie");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.s");

    // The nucleus embeds the `init` user ELF via include_bytes!. tools/run-qemu.sh stages
    // the real program here; guarantee the file exists so a bare `cargo build` compiles
    // (an empty image => the kernel skips userland).
    let user_elf = manifest.join("user.elf");
    if !user_elf.exists() {
        std::fs::write(&user_elf, b"").expect("create placeholder user.elf");
    }
    println!("cargo:rerun-if-changed=user.elf");
}
