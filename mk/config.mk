TARGET ?= x86_64-unknown-uefi
PACKAGE ?= bootloader
BOOTLOADER_PACKAGE ?= $(PACKAGE)
CARGO ?= cargo
RUSTUP ?= rustup
LD ?= ld
CC ?= gcc
AR ?= ar

KERNEL_PACKAGE ?= kernel
KERNEL_TARGET ?= x86_64-unknown-linux-gnu
KERNEL_CARGO_ZFLAGS ?= -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem
KERNEL_RUSTC_ARGS ?= -C no-redzone -C relocation-model=pic -C link-arg=-nostartfiles -C link-arg=-shared -C link-arg=-static -C link-arg=-Wl,-Bsymbolic -C link-arg=-Wl,-e,_start

PREKERNEL_PACKAGE ?= prekernel
PREKERNEL_RUSTC_ARGS ?= -C no-redzone -C link-arg=-nostartfiles -C link-arg=-no-pie -C link-arg=-static -C link-arg=-Wl,--image-base=0x100000

BUILD_DIR ?= $(ROOT_DIR)/build
ARTIFACT_DIR ?= $(BUILD_DIR)/artifacts
IMAGE_DIR ?= $(BUILD_DIR)/image

EFI_BOOT_DIR ?= $(IMAGE_DIR)/EFI/BOOT
BOOT_EFI ?= $(EFI_BOOT_DIR)/BOOTX64.EFI
SOURCE_EFI ?= $(ROOT_DIR)/target/$(TARGET)/release/$(BOOTLOADER_PACKAGE).efi
ARTIFACT_BOOT_EFI ?= $(ARTIFACT_DIR)/boot/BOOTX64.EFI

KERNEL_SOURCE ?= $(ROOT_DIR)/target/$(KERNEL_TARGET)/release/$(KERNEL_PACKAGE)
ARTIFACT_KERNEL_ELF ?= $(ARTIFACT_DIR)/kernel/kernel.elf
KERNEL_ELF ?= $(IMAGE_DIR)/kernel.elf

PREKERNEL_SOURCE ?= $(ROOT_DIR)/target/$(KERNEL_TARGET)/release/$(PREKERNEL_PACKAGE)
ARTIFACT_PREKERNEL_ELF ?= $(ARTIFACT_DIR)/boot/prekernel.elf
PREKERNEL_ELF ?= $(IMAGE_DIR)/prekernel.elf

USER_BUILD_DIR ?= $(ROOT_DIR)/target/uiserver
USER_ELF_PACKAGE ?= uiserver
USER_ELF_LINKAGE ?= dynamic
USER_SOURCE ?= $(USER_BUILD_DIR)/UISERVER.ELF
ARTIFACT_USER_ELF ?= $(ARTIFACT_DIR)/system/apps/uiserver/uiserver.elf
IMAGE_USER_ELF ?= $(IMAGE_DIR)/system/apps/uiserver/uiserver.elf
WIN_USER_OBJECT ?= $(USER_BUILD_DIR)/uiserver-win.obj
WIN_USER_SOURCE ?= $(USER_BUILD_DIR)/UISERVER.EXE
WIN_USER_ASM_SOURCE ?= $(ROOT_DIR)/uiserver/winmain.asm
ARTIFACT_WIN_USER_EXE ?= $(ARTIFACT_DIR)/system/apps/uiserver/uiserver.exe
IMAGE_WIN_USER_EXE ?= $(IMAGE_DIR)/system/apps/uiserver/uiserver.exe

PRINTF_DEMO_SOURCE ?= $(ROOT_DIR)/userdemo/printf_console.c
ARTIFACT_PRINTF_DEMO_ELF ?= $(ARTIFACT_DIR)/system/apps/printfdemo/printfdemo.elf
IMAGE_PRINTF_DEMO_ELF ?= $(IMAGE_DIR)/system/apps/printfdemo/printfdemo.elf

DRIVER_MODULE_TARGET_DIR ?= $(ROOT_DIR)/target/driver-modules
ARTIFACT_AMDGPU_KO ?= $(ARTIFACT_DIR)/system/drivers/display/amdgpu.ko
IMAGE_AMDGPU_KO ?= $(IMAGE_DIR)/system/drivers/display/amdgpu.ko
AMDGPU_FIRMWARE_DIR ?= /lib/firmware/amdgpu
AMDGPU_IMAGE_FIRMWARE_DIR ?= $(IMAGE_DIR)/system/firmware/amdgpu
AMDGPU_REQUIRED_FIRMWARE_BASENAMES ?= dcn_3_1_4_dmcub.bin psp_13_0_10_sos.bin psp_13_0_10_ta.bin smu_13_0_10.bin
ARTIFACT_PSMOUSE_KO ?= $(ARTIFACT_DIR)/system/drivers/input/psmouse.ko
IMAGE_PSMOUSE_KO ?= $(IMAGE_DIR)/system/drivers/input/psmouse.ko

STARTUP_NSH ?= $(IMAGE_DIR)/startup.nsh
BOOT_FILE_LIST ?= $(IMAGE_DIR)/BOOTFILES.TXT

GLIBC_INTERPRETER_SOURCE ?= $(shell $(CC) -print-file-name=ld-linux-x86-64.so.2 2>/dev/null)
GLIBC_LIBC_SOURCE ?= $(shell $(CC) -print-file-name=libc.so.6 2>/dev/null)
GLIBC_LIBGCC_SOURCE ?= $(shell $(CC) -print-file-name=libgcc_s.so.1 2>/dev/null)
GLIBC_INTERPRETER_DEST ?= $(IMAGE_DIR)/lib64/ld-linux-x86-64.so.2
GLIBC_LIBC_PRIMARY_DEST ?= $(IMAGE_DIR)/lib/x86_64-linux-gnu/libc.so.6
GLIBC_LIBC_FALLBACK_DEST ?= $(IMAGE_DIR)/lib64/libc.so.6
GLIBC_LIBGCC_PRIMARY_DEST ?= $(IMAGE_DIR)/lib/x86_64-linux-gnu/libgcc_s.so.1
GLIBC_LIBGCC_FALLBACK_DEST ?= $(IMAGE_DIR)/lib64/libgcc_s.so.1
GLIBC_LDSO_CACHE_DEST ?= $(IMAGE_DIR)/etc/ld.so.cache
GLIBC_LDSO_PRELOAD_DEST ?= $(IMAGE_DIR)/etc/ld.so.preload
LDCONFIG ?= $(shell command -v ldconfig 2>/dev/null)
