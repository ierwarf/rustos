TARGET ?= x86_64-unknown-uefi
PACKAGE ?= bootloader
BOOTLOADER_PACKAGE ?= $(PACKAGE)
CARGO ?= cargo
RUSTUP ?= rustup

BUILD_DIR ?= build
EFI_BOOT_DIR ?= $(BUILD_DIR)/EFI/BOOT
BOOT_EFI ?= $(EFI_BOOT_DIR)/BOOTX64.EFI
SOURCE_EFI ?= target/$(TARGET)/release/$(BOOTLOADER_PACKAGE).efi
STARTUP_NSH ?= $(BUILD_DIR)/startup.nsh

.PHONY: all target build build-efi stage check clean

all: build

target:
	$(RUSTUP) target add $(TARGET)

build: target build-efi stage
	@echo "UEFI image ready: $(BOOT_EFI)"
	@echo "UEFI startup script ready: $(STARTUP_NSH)"

build-efi:
	$(CARGO) build -p $(BOOTLOADER_PACKAGE) --target $(TARGET) --release

stage:
	mkdir -p $(EFI_BOOT_DIR)
	cp $(SOURCE_EFI) $(BOOT_EFI)
	printf '\\EFI\\BOOT\\BOOTX64.EFI\r\n' > $(STARTUP_NSH)

check: target
	$(CARGO) check -p $(BOOTLOADER_PACKAGE) --target $(TARGET)

clean:
	$(CARGO) clean
	rm -rf $(BUILD_DIR)
