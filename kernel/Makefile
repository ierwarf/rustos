ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: build-kernel

build-kernel:
	$(CARGO) rustc $(KERNEL_CARGO_ZFLAGS) -p $(KERNEL_PACKAGE) --target $(KERNEL_TARGET) --release -- $(KERNEL_RUSTC_ARGS)
	mkdir -p "$$(dirname "$(ARTIFACT_KERNEL_ELF)")"
	cp "$(KERNEL_SOURCE)" "$(ARTIFACT_KERNEL_ELF)"
