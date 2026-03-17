ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: stage clean

stage:
	rm -rf "$(IMAGE_DIR)"
	rm -rf "$(BUILD_DIR)/EFI" "$(BUILD_DIR)/etc" "$(BUILD_DIR)/lib" "$(BUILD_DIR)/lib64" "$(BUILD_DIR)/linux"
	rm -f "$(BUILD_DIR)/BOOTFILES.TXT" "$(BUILD_DIR)/startup.nsh" "$(BUILD_DIR)/kernel.elf" "$(BUILD_DIR)/prekernel.elf"
	rm -f "$(BUILD_DIR)/UISERVER.ELF" "$(BUILD_DIR)/UISERVER.EXE" "$(BUILD_DIR)/PRINTFDEMO.ELF"
	rm -f "$(BUILD_DIR)/USERDEMO.ELF" "$(BUILD_DIR)/USERDEMO.EXE"
	mkdir -p "$(EFI_BOOT_DIR)"
	cp "$(ARTIFACT_BOOT_EFI)" "$(BOOT_EFI)"
	cp "$(ARTIFACT_PREKERNEL_ELF)" "$(PREKERNEL_ELF)"
	cp "$(ARTIFACT_KERNEL_ELF)" "$(KERNEL_ELF)"
	mkdir -p "$$(dirname "$(IMAGE_USER_ELF)")"
	mkdir -p "$$(dirname "$(IMAGE_WIN_USER_EXE)")"
	mkdir -p "$$(dirname "$(IMAGE_PRINTF_DEMO_ELF)")"
	mkdir -p "$$(dirname "$(IMAGE_PSMOUSE_KO)")"
	cp "$(ARTIFACT_USER_ELF)" "$(IMAGE_USER_ELF)"
	cp "$(ARTIFACT_WIN_USER_EXE)" "$(IMAGE_WIN_USER_EXE)"
	cp "$(ARTIFACT_PRINTF_DEMO_ELF)" "$(IMAGE_PRINTF_DEMO_ELF)"
	cp "$(ARTIFACT_PSMOUSE_KO)" "$(IMAGE_PSMOUSE_KO)"
	rm -f "$(BOOT_FILE_LIST)"
	@if [ -f "$(GLIBC_INTERPRETER_SOURCE)" ]; then \
		mkdir -p "$$(dirname "$(GLIBC_INTERPRETER_DEST)")"; \
		cp "$(GLIBC_INTERPRETER_SOURCE)" "$(GLIBC_INTERPRETER_DEST)"; \
		printf 'lib64/ld-linux-x86-64.so.2\r\n' >> "$(BOOT_FILE_LIST)"; \
	fi
	@if [ -f "$(GLIBC_LIBC_SOURCE)" ]; then \
		mkdir -p "$$(dirname "$(GLIBC_LIBC_PRIMARY_DEST)")"; \
		mkdir -p "$$(dirname "$(GLIBC_LIBC_FALLBACK_DEST)")"; \
		cp "$(GLIBC_LIBC_SOURCE)" "$(GLIBC_LIBC_PRIMARY_DEST)"; \
		cp "$(GLIBC_LIBC_SOURCE)" "$(GLIBC_LIBC_FALLBACK_DEST)"; \
		printf 'lib/x86_64-linux-gnu/libc.so.6\r\n' >> "$(BOOT_FILE_LIST)"; \
		printf 'lib64/libc.so.6\r\n' >> "$(BOOT_FILE_LIST)"; \
	fi
	@if [ -f "$(GLIBC_LIBGCC_SOURCE)" ]; then \
		mkdir -p "$$(dirname "$(GLIBC_LIBGCC_PRIMARY_DEST)")"; \
		mkdir -p "$$(dirname "$(GLIBC_LIBGCC_FALLBACK_DEST)")"; \
		cp "$(GLIBC_LIBGCC_SOURCE)" "$(GLIBC_LIBGCC_PRIMARY_DEST)"; \
		cp "$(GLIBC_LIBGCC_SOURCE)" "$(GLIBC_LIBGCC_FALLBACK_DEST)"; \
		printf 'lib/x86_64-linux-gnu/libgcc_s.so.1\r\n' >> "$(BOOT_FILE_LIST)"; \
		printf 'lib64/libgcc_s.so.1\r\n' >> "$(BOOT_FILE_LIST)"; \
	fi
	@if [ -n "$(LDCONFIG)" ] && [ -f "$(GLIBC_LIBC_PRIMARY_DEST)" ]; then \
		mkdir -p "$$(dirname "$(GLIBC_LDSO_CACHE_DEST)")"; \
		"$(LDCONFIG)" -r "$(IMAGE_DIR)" -C /etc/ld.so.cache; \
		printf 'etc/ld.so.cache\r\n' >> "$(BOOT_FILE_LIST)"; \
	fi
	@printf 'system/apps/uiserver/uiserver.elf\r\n' >> "$(BOOT_FILE_LIST)"
	@printf 'system/apps/uiserver/uiserver.exe\r\n' >> "$(BOOT_FILE_LIST)"
	@printf 'system/apps/printfdemo/printfdemo.elf\r\n' >> "$(BOOT_FILE_LIST)"
	@printf 'system/drivers/input/psmouse.ko\r\n' >> "$(BOOT_FILE_LIST)"
	@mkdir -p "$$(dirname "$(GLIBC_LDSO_PRELOAD_DEST)")"
	@: > "$(GLIBC_LDSO_PRELOAD_DEST)"
	@printf 'etc/ld.so.preload\r\n' >> "$(BOOT_FILE_LIST)"
	@if [ ! -s "$(BOOT_FILE_LIST)" ]; then rm -f "$(BOOT_FILE_LIST)"; fi
	printf '\\EFI\\BOOT\\BOOTX64.EFI\r\n' > "$(STARTUP_NSH)"

clean:
	$(CARGO) clean
	rm -rf "$(BUILD_DIR)"
