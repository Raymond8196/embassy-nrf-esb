//! Inject this board's selected memory layout for the linker.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let memory = if env::var_os("ELYTRA_UF2").is_some() {
        include_bytes!("memory-uf2.x").as_slice()
    } else {
        include_bytes!("memory-swd.x").as_slice()
    };
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(memory)
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory-swd.x");
    println!("cargo:rerun-if-changed=memory-uf2.x");
    println!("cargo:rerun-if-env-changed=ELYTRA_UF2");
}
