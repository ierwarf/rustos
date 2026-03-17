ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: build-prekernel

build-prekernel:
	$(CARGO) rustc $(KERNEL_CARGO_ZFLAGS) -p $(PREKERNEL_PACKAGE) --target $(KERNEL_TARGET) --release -- $(PREKERNEL_RUSTC_ARGS)
	mkdir -p "$$(dirname "$(ARTIFACT_PREKERNEL_ELF)")"
	cp "$(PREKERNEL_SOURCE)" "$(ARTIFACT_PREKERNEL_ELF)"
