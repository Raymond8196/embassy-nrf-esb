EXAMPLES    := usb_minimal ptx_silent prx_usb
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

# Build single example: make build-ptx_silent
build-%:
	cargo build --example $* --features $(FEATURES) --release

# Convert to hex: make ptx_silent.hex
%.hex: build-%
	$(OBJCOPY) -O ihex $(RELEASE_DIR)/$* $@

# Create DFU package: make ptx_silent_dfu.zip
%_dfu.zip: %.hex
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
	@echo "  make ptx_silent_dfu.zip Build single DFU package"
	@echo "  make flash-ptx_silent PORT=/dev/tty.usbmodemXXXX"
	@echo "  make flash-prx_usb PORT=/dev/tty.usbmodemXXXX"
	@echo "  make flash-usb_minimal PORT=/dev/tty.usbmodemXXXX"
	@echo "  make ports              List connected serial ports"
	@echo "  make clean              Remove all build artifacts"

.PHONY: all clean help ports build-% flash-%
