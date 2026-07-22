# Pinned verifier toolchain

Verus proofs are not reproducible across Z3 versions or Rust nightlies. This
directory pins the verifier bit-for-bit. See `docs/verification.md` for the why.

- `verus.lock` — Verus release/SHA + bundled Z3 version + Z3 SHA-256.
- `fetch-verus.sh` — downloads + verifies + installs under `toolchain/verus/` (gitignored).

Never bump the Verus release, the Z3, `rust-toolchain.toml`, or `Cargo.lock`
independently — move them as one unit and re-run `cargo xtask verify`.
