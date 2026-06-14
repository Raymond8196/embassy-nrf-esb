//! Linker script memory.x injection.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    // embedded-test ships its own linker script (`embedded-test.x`), needed only
    // by the on-target `hw` test binary. Scope it to test targets (not
    // examples/bins) and only for the HIL chip (nrf52833). Host lib tests build
    // with nrf52840 and are unaffected.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NRF52833");
    if env::var("CARGO_FEATURE_NRF52833").is_ok() {
        println!("cargo:rustc-link-arg-tests=-Tembedded-test.x");
    }

    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PROVIDE_MEMORY_X");
    // A board crate that supplies its own memory.x opts out via
    // `default-features = false`, so this fragment must not be emitted then —
    // two memory.x on the link search path would collide nondeterministically.
    if env::var("CARGO_FEATURE_PROVIDE_MEMORY_X").is_err() {
        return;
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    // nRF52833 (Elytra over SWD, full-flash bare layout) takes priority so the
    // `hw` test links for the right part. Otherwise pick the nRF52840 board.
    let memory_x = if env::var("CARGO_FEATURE_NRF52833").is_ok() {
        include_str!("memory-nrf52833.x")
    } else if env::var("CARGO_FEATURE_BOARD_NICENANO").is_ok() {
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
    println!("cargo:rerun-if-changed=memory-nrf52833.x");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BOARD_NICENANO");
}
