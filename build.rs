//! Linker script memory.x injection.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let memory_x = if env::var("CARGO_FEATURE_BOARD_NICENANO").is_ok() {
        include_str!("memory-nicenano.x")
    } else {
        include_str!("memory.x")
    };

    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(memory_x.as_bytes())
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=memory-nicenano.x");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BOARD_NICENANO");
}
