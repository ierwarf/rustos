ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: build-efi check

build-efi:
	$(CARGO) build -p $(BOOTLOADER_PACKAGE) --target $(TARGET) --release
	mkdir -p "$$(dirname "$(ARTIFACT_BOOT_EFI)")"
	cp "$(SOURCE_EFI)" "$(ARTIFACT_BOOT_EFI)"

check:
	$(CARGO) check -p $(BOOTLOADER_PACKAGE) --target $(TARGET)
