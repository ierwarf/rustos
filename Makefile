ROOT_DIR := $(CURDIR)
include $(ROOT_DIR)/mk/config.mk

.PHONY: all target build build-efi build-prekernel build-kernel build-user build-console-demo build-driver-modules stage check clean

all: build

target:
	$(RUSTUP) target add $(TARGET)
	$(RUSTUP) target add $(KERNEL_TARGET)

build: target build-efi build-prekernel build-kernel build-user build-console-demo build-driver-modules stage
	@echo "UEFI image ready: $(BOOT_EFI)"
	@echo "Prekernel ELF ready: $(PREKERNEL_ELF)"
	@echo "Kernel ELF ready: $(KERNEL_ELF)"
	@echo "User ELF ready: $(IMAGE_USER_ELF)"
	@echo "User EXE ready: $(IMAGE_WIN_USER_EXE)"
	@echo "Console demo ELF ready: $(IMAGE_PRINTF_DEMO_ELF)"
	@echo "PS/2 mouse driver module ready: $(IMAGE_PSMOUSE_KO)"
	@echo "UEFI startup script ready: $(STARTUP_NSH)"
	@if [ -f "$(BOOT_FILE_LIST)" ]; then echo "Boot file manifest ready: $(BOOT_FILE_LIST)"; fi

build-efi:
	$(MAKE) -C $(ROOT_DIR)/bootloader ROOT_DIR=$(ROOT_DIR) build-efi

build-prekernel:
	$(MAKE) -C $(ROOT_DIR)/prekernel ROOT_DIR=$(ROOT_DIR) build-prekernel

build-kernel:
	$(MAKE) -C $(ROOT_DIR)/kernel ROOT_DIR=$(ROOT_DIR) build-kernel

build-user:
	$(MAKE) -C $(ROOT_DIR)/uiserver ROOT_DIR=$(ROOT_DIR) build-user

build-console-demo:
	$(MAKE) -C $(ROOT_DIR)/userdemo ROOT_DIR=$(ROOT_DIR) build-console-demo

build-driver-modules:
	$(MAKE) -C $(ROOT_DIR)/drivers ROOT_DIR=$(ROOT_DIR) build-driver-modules

stage:
	$(MAKE) -C $(ROOT_DIR)/tools ROOT_DIR=$(ROOT_DIR) stage

check: target
	$(MAKE) -C $(ROOT_DIR)/bootloader ROOT_DIR=$(ROOT_DIR) check

clean:
	$(MAKE) -C $(ROOT_DIR)/tools ROOT_DIR=$(ROOT_DIR) clean
