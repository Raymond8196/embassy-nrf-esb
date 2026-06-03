//! Linker script memory.x injection.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PROVIDE_MEMORY_X");
    // A board crate that supplies its own memory.x opts out via
    // `default-features = false`, so this fragment must not be emitted then —
    // two memory.x on the link search path would collide nondeterministically.
    if env::var("CARGO_FEATURE_PROVIDE_MEMORY_X").is_err() {
        return;
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let memory_x = if env::var("CARGO_FEATURE_BOARD_NICENANO").is_ok() {
        include_str!("memory-nicenano.x")
    } else {
        include_str!("memory-dongle.x")
    };

    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(memory_x.as_bytes())
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory-dongle.x");
    println!("cargo:rerun-if-changed=memory-nicenano.x");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BOARD_NICENANO");
}
