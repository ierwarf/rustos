TARGET ?= x86_64-unknown-uefi
PACKAGE ?= bootloader
BOOTLOADER_PACKAGE ?= $(PACKAGE)
CARGO ?= cargo
RUSTUP ?= rustup

KERNEL_PACKAGE ?= kernel
KERNEL_TARGET ?= x86_64-unknown-linux-gnu
KERNEL_CARGO_ZFLAGS ?= -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem
KERNEL_RUSTC_ARGS ?= -C no-redzone -C link-arg=-nostartfiles -C link-arg=-no-pie -C link-arg=-static

BUILD_DIR ?= build
EFI_BOOT_DIR ?= $(BUILD_DIR)/EFI/BOOT
BOOT_EFI ?= $(EFI_BOOT_DIR)/BOOTX64.EFI
SOURCE_EFI ?= target/$(TARGET)/release/$(BOOTLOADER_PACKAGE).efi
KERNEL_SOURCE ?= target/$(KERNEL_TARGET)/release/$(KERNEL_PACKAGE)
KERNEL_ELF ?= $(BUILD_DIR)/kernel.elf
STARTUP_NSH ?= $(BUILD_DIR)/startup.nsh

.PHONY: all target build build-efi build-kernel stage check clean

all: build

target:
	$(RUSTUP) target add $(TARGET)
	$(RUSTUP) target add $(KERNEL_TARGET)

build: target build-efi build-kernel stage
	@echo "UEFI image ready: $(BOOT_EFI)"
	@echo "Kernel ELF ready: $(KERNEL_ELF)"
	@echo "UEFI startup script ready: $(STARTUP_NSH)"

build-efi:
	$(CARGO) build -p $(BOOTLOADER_PACKAGE) --target $(TARGET) --release

build-kernel:
	$(CARGO) rustc $(KERNEL_CARGO_ZFLAGS) -p $(KERNEL_PACKAGE) --target $(KERNEL_TARGET) --release -- $(KERNEL_RUSTC_ARGS)

stage:
	mkdir -p $(EFI_BOOT_DIR)
	cp $(SOURCE_EFI) $(BOOT_EFI)
	cp $(KERNEL_SOURCE) $(KERNEL_ELF)
	printf '\\EFI\\BOOT\\BOOTX64.EFI\r\n' > $(STARTUP_NSH)

check: target
	$(CARGO) check -p $(BOOTLOADER_PACKAGE) --target $(TARGET)

clean:
	$(CARGO) clean
	rm -rf $(BUILD_DIR)
