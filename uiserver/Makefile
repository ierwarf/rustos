ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: build-user

build-user:
	mkdir -p "$(USER_BUILD_DIR)"
	@if [ "$(USER_ELF_LINKAGE)" != "dynamic" ]; then \
		echo "Rust std UISERVER currently supports only USER_ELF_LINKAGE=dynamic" >&2; \
		exit 1; \
	fi
	$(CARGO) build -p $(USER_ELF_PACKAGE) --target $(KERNEL_TARGET) --release
	cp "$(ROOT_DIR)/target/$(KERNEL_TARGET)/release/$(USER_ELF_PACKAGE)" "$(USER_SOURCE)"
	nasm -f win64 -o "$(WIN_USER_OBJECT)" "$(WIN_USER_ASM_SOURCE)"
	$(LD) -mi386pep --subsystem console --image-base 0x8000400000 -e start -o "$(WIN_USER_SOURCE)" "$(WIN_USER_OBJECT)"
	mkdir -p "$$(dirname "$(ARTIFACT_USER_ELF)")"
	mkdir -p "$$(dirname "$(ARTIFACT_WIN_USER_EXE)")"
	cp "$(USER_SOURCE)" "$(ARTIFACT_USER_ELF)"
	cp "$(WIN_USER_SOURCE)" "$(ARTIFACT_WIN_USER_EXE)"
