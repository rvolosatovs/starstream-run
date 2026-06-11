//! Export the mutable `__stack_pointer` global from the linked module: the
//! JSPI fiber glue (`web/fiber-env.js`) swaps the LLVM shadow-stack pointer
//! on every fiber switch and needs the global by export. Done here — not via
//! `.cargo/config.toml` `rustflags` — because a `RUSTFLAGS` environment
//! variable (e.g. from a nix shell) silently replaces config-file rustflags
//! entirely, whereas `cargo:rustc-link-arg` is additive and travels with the
//! crate.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("wasm") {
        println!("cargo:rustc-link-arg=--export=__stack_pointer");
    }
}
