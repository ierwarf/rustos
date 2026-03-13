TARGET ?= x86_64-unknown-uefi
PACKAGE ?= bootloader
BOOTLOADER_PACKAGE ?= $(PACKAGE)
CARGO ?= cargo
RUSTUP ?= rustup
LD ?= ld
CC = gcc

KERNEL_PACKAGE ?= kernel
KERNEL_TARGET ?= x86_64-unknown-linux-gnu
KERNEL_CARGO_ZFLAGS ?= -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem
KERNEL_RUSTC_ARGS ?= -C no-redzone -C link-arg=-nostartfiles -C link-arg=-no-pie -C link-arg=-static
PREKERNEL_PACKAGE ?= prekernel
PREKERNEL_SOURCE ?= target/$(KERNEL_TARGET)/release/$(PREKERNEL_PACKAGE)
PREKERNEL_RUSTC_ARGS ?= -C no-redzone -C link-arg=-nostartfiles -C link-arg=-no-pie -C link-arg=-static -C link-arg=-Wl,--image-base=0x100000

BUILD_DIR ?= build
EFI_BOOT_DIR ?= $(BUILD_DIR)/EFI/BOOT
BOOT_EFI ?= $(EFI_BOOT_DIR)/BOOTX64.EFI
SOURCE_EFI ?= target/$(TARGET)/release/$(BOOTLOADER_PACKAGE).efi
KERNEL_SOURCE ?= target/$(KERNEL_TARGET)/release/$(KERNEL_PACKAGE)
PREKERNEL_ELF ?= $(BUILD_DIR)/prekernel.elf
KERNEL_ELF ?= $(BUILD_DIR)/kernel.elf
USER_BUILD_DIR ?= target/userdemo
USER_SOURCE ?= $(USER_BUILD_DIR)/USERDEMO.ELF
USER_ELF_SOURCE ?= userdemo/main.c
USER_ELF_BUILD_SCRIPT ?= tools/build-userdemo-elf.sh
USER_ELF ?= $(BUILD_DIR)/USERDEMO.ELF
WIN_USER_OBJECT ?= $(USER_BUILD_DIR)/userdemo-win.obj
WIN_USER_SOURCE ?= $(USER_BUILD_DIR)/USERDEMO.EXE
WIN_USER_ASM_SOURCE ?= userdemo/winmain.asm
WIN_USER_EXE ?= $(BUILD_DIR)/USERDEMO.EXE
STARTUP_NSH ?= $(BUILD_DIR)/startup.nsh

.PHONY: all target build build-efi build-kernel stage check clean

all: build

target:
	$(RUSTUP) target add $(TARGET)
	$(RUSTUP) target add $(KERNEL_TARGET)

build: target build-efi build-prekernel build-kernel build-user stage
	@echo "UEFI image ready: $(BOOT_EFI)"
	@echo "Prekernel ELF ready: $(PREKERNEL_ELF)"
	@echo "Kernel ELF ready: $(KERNEL_ELF)"
	@echo "User ELF ready: $(USER_ELF)"
	@echo "User EXE ready: $(WIN_USER_EXE)"
	@echo "UEFI startup script ready: $(STARTUP_NSH)"

build-efi:
	$(CARGO) build -p $(BOOTLOADER_PACKAGE) --target $(TARGET) --release

build-kernel:
	$(CARGO) rustc $(KERNEL_CARGO_ZFLAGS) -p $(KERNEL_PACKAGE) --target $(KERNEL_TARGET) --release -- $(KERNEL_RUSTC_ARGS)

build-prekernel:
	$(CARGO) rustc $(KERNEL_CARGO_ZFLAGS) -p $(PREKERNEL_PACKAGE) --target $(KERNEL_TARGET) --release -- $(PREKERNEL_RUSTC_ARGS)

build-user:
	mkdir -p $(USER_BUILD_DIR)
	bash $(USER_ELF_BUILD_SCRIPT) $(USER_SOURCE) $(USER_ELF_SOURCE)
	nasm -f win64 -o $(WIN_USER_OBJECT) $(WIN_USER_ASM_SOURCE)
	$(LD) -mi386pep --subsystem console --image-base 0x8000400000 -e start -o $(WIN_USER_SOURCE) $(WIN_USER_OBJECT)

stage:
	mkdir -p $(EFI_BOOT_DIR)
	cp $(SOURCE_EFI) $(BOOT_EFI)
	cp $(PREKERNEL_SOURCE) $(PREKERNEL_ELF)
	cp $(KERNEL_SOURCE) $(KERNEL_ELF)
	cp $(USER_SOURCE) $(USER_ELF)
	cp $(WIN_USER_SOURCE) $(WIN_USER_EXE)
	printf '\\EFI\\BOOT\\BOOTX64.EFI\r\n' > $(STARTUP_NSH)

check: target
	$(CARGO) check -p $(BOOTLOADER_PACKAGE) --target $(TARGET)

clean:
	$(CARGO) clean
	rm -rf $(BUILD_DIR)
