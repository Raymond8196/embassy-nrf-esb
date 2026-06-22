//! Task runner for embassy-nrf-esb.
//!
//! Usage:
//!   cargo xtask test               # host unit tests + on-target HIL tests
//!   cargo xtask test-host [triple] # host unit tests only
//!   cargo xtask test-hw            # on-target register HIL tests (needs a probe)
//!
//! `cargo xtask` works via the alias in `.cargo/config.toml`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Default target for host unit tests. nRF chip features pull `nrf-pac` with its
/// `rt` (cortex-m-rt) section attributes, which only compile for an ELF target —
/// not mach-o — so host tests must use an ELF triple. This matches the Makefile's
/// `check-host`. Override per-run: `cargo xtask test-host <triple>`.
const HOST_TARGET: &str = "x86_64-unknown-linux-gnu";

/// Embedded target the library/examples build for (matches `.cargo/config.toml`).
const EMBEDDED_TARGET: &str = "thumbv7em-none-eabihf";

/// probe-rs chip for the HIL target. The register tests run on the Elytra
/// nRF52833 over SWD; the two nRF52840 dongles are DFU-only and cannot be
/// driven by probe-rs / embedded-test.
const HIL_CHIP: &str = "nRF52833_xxAA";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ok = match args.first().map(String::as_str) {
        Some("test") => run_test_host(HOST_TARGET) && run_test_hw(),
        Some("test-host") => {
            let target = args.get(1).map(String::as_str).unwrap_or(HOST_TARGET);
            run_test_host(target)
        }
        Some("test-hw") => run_test_hw(),
        _ => {
            eprintln!("Usage: cargo xtask <test|test-host|test-hw> [triple]");
            eprintln!();
            eprintln!("  test               host unit tests + on-target HIL tests");
            eprintln!("  test-host [triple] host unit tests only (default {HOST_TARGET})");
            eprintln!("  test-hw            on-target register HIL tests (requires a probe)");
            return ExitCode::FAILURE;
        }
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is not under the workspace root")
        .to_path_buf()
}

/// Host unit tests: equivalent to `make check-host`.
fn run_test_host(target: &str) -> bool {
    println!("==> host unit tests ({target})");
    cargo(
        &[
            "test",
            "--lib",
            "--target",
            target,
            "--features",
            "nrf52840",
        ],
        &[],
    )
}

/// On-target register HIL tests via embedded-test + probe-rs.
///
/// Overrides the cargo runner so the thumbv7em test binary is flashed to the
/// nRF52833 (root `.cargo/config.toml` hard-codes nRF52840 for examples).
fn run_test_hw() -> bool {
    println!("==> HIL register tests ({HIL_CHIP}, requires a connected probe)");
    let runner = format!("probe-rs run --chip {HIL_CHIP}");
    cargo(
        &[
            "test",
            "--test",
            "hw",
            "--target",
            EMBEDDED_TARGET,
            "--features",
            "nrf52833,defmt,_cs-cortex",
        ],
        &[("CARGO_TARGET_THUMBV7EM_NONE_EABIHF_RUNNER", runner.as_str())],
    )
}

fn cargo(args: &[&str], env: &[(&str, &str)]) -> bool {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(workspace_root()).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    match cmd.status() {
        Ok(status) => status.success(),
        Err(e) => {
            eprintln!("failed to spawn cargo: {e}");
            false
        }
    }
}
