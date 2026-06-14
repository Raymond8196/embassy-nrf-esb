EXAMPLES    := usb_minimal ptx_silent prx_usb
EXCLUSIVE_DEFFMT_EXAMPLES := ptx_basic prx_basic ptx_multipipe ptx_suspend
EXCLUSIVE_USB_EXAMPLES := prx_usb ptx_silent usb_minimal ptx_ack_echo ptx_multipipe_ack prx_multipipe_usb ptx_noack_usb prx_noack_usb ptx_suspend_usb prx_idle_usb
MPSL_EXAMPLES := mpsl_smoke mpsl_request_basic mpsl_request_chained mpsl_ptx_in_slot mpsl_prx_in_slot mpsl_prx_ble mpsl_ble_connectable mpsl_3mode_poll mpsl_3mode_central mpsl_3mode_event mpsl_ptx_continuous
RUSTFMT_CHECK_FILES := src/lib.rs src/mpsl_common.rs src/mpsl_profile.rs src/mpsl_radio.rs src/mpsl_timeslot.rs src/payload.rs src/transport.rs examples/mpsl_3mode_poll.rs examples/mpsl_3mode_central.rs
RELEASE_DIR := target/thumbv7em-none-eabihf/release/examples
FEATURES    ?= nrf52840,_cs-cortex
OBJCOPY     ?= arm-none-eabi-objcopy
HW_VERSION  := 52
SD_REQ      := 0x00
APP_VERSION := 1
DFU_BAUD    := 115200

# Default: build all DFU packages
all: $(addsuffix _dfu.zip,$(EXAMPLES))
	@echo "--- All DFU packages ready ---"
	@ls -lh *_dfu.zip

check-host:
	cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840

check-exclusive:
	@set -e; \
	for ex in $(EXCLUSIVE_DEFFMT_EXAMPLES); do \
		cargo check --example $$ex --features nrf52840,defmt,_cs-cortex; \
	done; \
	for ex in $(EXCLUSIVE_USB_EXAMPLES); do \
		cargo check --example $$ex --features nrf52840,_cs-cortex; \
	done

check-mpsl:
	@set -e; \
	for ex in $(MPSL_EXAMPLES); do \
		cargo check --example $$ex --features nrf52840,defmt,mpsl; \
	done

check-feature-conflict:
	@cargo check --features nrf52840,mpsl,_cs-cortex >/tmp/embassy-nrf-esb-feature-conflict.log 2>&1; \
	status=$$?; \
	if [ $$status -eq 0 ]; then \
		cat /tmp/embassy-nrf-esb-feature-conflict.log; \
		echo "error: nrf52840,mpsl,_cs-cortex unexpectedly compiled"; \
		exit 1; \
	fi; \
	if ! grep -q 'features `mpsl` and `_cs-cortex` are mutually exclusive' /tmp/embassy-nrf-esb-feature-conflict.log; then \
		cat /tmp/embassy-nrf-esb-feature-conflict.log; \
		echo "error: feature conflict failed for an unexpected reason"; \
		exit 1; \
	fi; \
	echo "feature conflict check passed"

check-fmt:
	rustfmt --edition 2024 --check $(RUSTFMT_CHECK_FILES)
	git diff --check

check-no-hw: check-host check-exclusive check-mpsl check-feature-conflict check-fmt

# Test runners (delegate to the xtask crate). HIL tests need a probe on the
# nRF52833; host tests use the ELF triple baked into xtask (override with
# `cargo xtask test-host <triple>`).
test: ; cargo xtask test
test-host: ; cargo xtask test-host
test-hw: ; cargo xtask test-hw

# Build single example: make build-ptx_silent
build-%:
	@command -v cargo >/dev/null || { echo "error: cargo not found in PATH"; exit 1; }
	cargo build --example $* --features $(FEATURES) --release

# Convert to hex: make ptx_silent.hex
%.hex: build-%
	@command -v $(OBJCOPY) >/dev/null || { echo "error: $(OBJCOPY) not found in PATH. Install arm-none-eabi-binutils or run make OBJCOPY=<objcopy>"; exit 1; }
	$(OBJCOPY) -O ihex $(RELEASE_DIR)/$* $@

# Create DFU package: make ptx_silent_dfu.zip
%_dfu.zip: %.hex
	@command -v python3 >/dev/null || { echo "error: python3 not found in PATH"; exit 1; }
	@python3 -c 'import nordicsemi' >/dev/null 2>&1 || { echo "error: nrfutil Python package not found. Install it with: python3 -m pip install nrfutil"; exit 1; }
	python3 -m nordicsemi pkg generate \
		--application $< \
		--hw-version $(HW_VERSION) \
		--sd-req $(SD_REQ) \
		--application-version $(APP_VERSION) \
		$@

# Flash via serial DFU: make flash-ptx_silent PORT=/dev/tty.usbmodemXXXX
flash-%: %_dfu.zip
ifndef PORT
	$(error PORT is not set. Usage: make flash-ptx_silent PORT=/dev/tty.usbmodemXXXX)
endif
	@command -v python3 >/dev/null || { echo "error: python3 not found in PATH"; exit 1; }
	@python3 -c 'import nordicsemi' >/dev/null 2>&1 || { echo "error: nrfutil Python package not found. Install it with: python3 -m pip install nrfutil"; exit 1; }
	python3 -m nordicsemi dfu serial -pkg $< -p $(PORT) -b $(DFU_BAUD)

# List available serial ports
ports:
	@echo "Available serial ports:"
	@ls /dev/tty.usbmodem* /dev/cu.usbmodem* 2>/dev/null || echo "  (none found)"

# Clean build artifacts (keeps source)
clean:
	rm -f *.hex *.zip *.uf2
	cargo clean

# Quick reference
help:
	@echo "Usage:"
	@echo "  make                    Build all DFU packages"
	@echo "  make check-no-hw        Run host tests and compile checks that need no dongle"
	@echo "  make check-host         Run host-side unit tests"
	@echo "  make check-exclusive    Check exclusive ESB examples"
	@echo "  make check-mpsl         Check MPSL examples"
	@echo "  make ptx_silent_dfu.zip Build single DFU package"
	@echo "  make flash-ptx_silent PORT=/dev/tty.usbmodemXXXX"
	@echo "  make flash-prx_usb PORT=/dev/tty.usbmodemXXXX"
	@echo "  make flash-usb_minimal PORT=/dev/tty.usbmodemXXXX"
	@echo "  make ports              List connected serial ports"
	@echo "  make clean              Remove all build artifacts"

.PHONY: all check-host check-exclusive check-mpsl check-feature-conflict check-fmt check-no-hw test test-host test-hw clean help ports build-% flash-%
