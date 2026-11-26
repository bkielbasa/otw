ROOT := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Use the wrapper because Unicorn's build needs the configured libclang/ninja tools.
CARGO ?= $(ROOT)scripts/cargo.sh

# Default to the small guest that exits cleanly. TARGET may be a path to any
# PE32 guest, or another main.rs selector such as --hello, --boot or --kernel.
TARGET ?= --hello
ARGS ?= --run
FEATURES ?=

QEMU ?= qemu-system-i386
QEMU_DISPLAY ?= vnc=:1
# Empty: the desktop runs on drive C: (OTW_DRIVE_C, default ~/.otw/C).
# Set to a FAT32 image to browse that image read-only instead.
DESKTOP_FAT32 ?=

.PHONY: all build run desktop qemu test fmt package clean

all: build

build:
	$(CARGO) build $(if $(strip $(FEATURES)),--features "$(strip $(FEATURES))")

run:
	$(CARGO) run $(if $(strip $(FEATURES)),--features "$(strip $(FEATURES))") -- $(if $(strip $(TARGET)),"$(strip $(TARGET))") $(ARGS)

# Firefox: no crash reporter process; stubs for the few functions it
# imports that OTW lacks.
desktop: export OTW_ENV_MOZ_CRASHREPORTER_DISABLE ?= 1
desktop: export OTW_STUB_MISSING ?= 1
desktop:
	$(CARGO) run --features display -- --desktop $(if $(strip $(DESKTOP_FAT32)),--fat32 "$(strip $(DESKTOP_FAT32))")

qemu:
	QEMU='$(QEMU)' QEMU_DISPLAY='$(QEMU_DISPLAY)' $(ROOT)scripts/run-qemu.sh

test:
	$(CARGO) test $(if $(strip $(FEATURES)),--features "$(strip $(FEATURES))")

fmt:
	$(CARGO) fmt

package:
	$(ROOT)scripts/package-vm.sh

clean:
	$(CARGO) clean
