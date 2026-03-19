ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: build-driver-modules

build-driver-modules:
	mkdir -p "$$(dirname "$(ARTIFACT_AMDGPU_KO)")"
	mkdir -p "$$(dirname "$(ARTIFACT_PSMOUSE_KO)")"
	rm -f "$(ARTIFACT_AMDGPU_KO)"
	$(CARGO) build -p driver-abi --release --target $(KERNEL_TARGET)
	rustc "$(ROOT_DIR)/drivers/display/amdgpu/src/lib.rs" \
		--crate-name rustos_amdgpu_driver \
		--crate-type lib \
		--edition=2024 \
		--target $(KERNEL_TARGET) \
		--emit=obj \
		-o "$(ARTIFACT_AMDGPU_KO)" \
		-L dependency="$(ROOT_DIR)/target/$(KERNEL_TARGET)/release" \
		-L dependency="$(ROOT_DIR)/target/$(KERNEL_TARGET)/release/deps" \
		--extern driver_abi="$(ROOT_DIR)/target/$(KERNEL_TARGET)/release/libdriver_abi.rlib" \
		--cfg 'feature="module-image"' \
		-C opt-level=3 \
		-C panic=abort \
		-C no-redzone \
		-C relocation-model=pic
	@test -f "$(ARTIFACT_PSMOUSE_KO)" || { \
		echo "missing prebuilt psmouse module: $(ARTIFACT_PSMOUSE_KO)"; \
		exit 1; \
	}
