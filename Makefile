TARGET ?= x86_64-unknown-uefi
PACKAGE ?= bootloader
CARGO ?= cargo
RUSTUP ?= rustup

BUILD_DIR ?= build
EFI_BOOT_DIR ?= $(BUILD_DIR)/EFI/BOOT
BOOT_EFI ?= $(EFI_BOOT_DIR)/BOOTX64.EFI
SOURCE_EFI ?= target/$(TARGET)/release/$(PACKAGE).efi

.PHONY: all target build check clean

all: build

target:
	$(RUSTUP) target add $(TARGET)

build: target
	$(CARGO) build -p $(PACKAGE) --target $(TARGET) --release
	mkdir -p $(EFI_BOOT_DIR)
	cp $(SOURCE_EFI) $(BOOT_EFI)
	@echo "UEFI image ready: $(BOOT_EFI)"

check: target
	$(CARGO) check -p $(PACKAGE) --target $(TARGET)

clean:
	$(CARGO) clean
	rm -rf $(BUILD_DIR)
