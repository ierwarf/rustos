ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: build-driver-modules

build-driver-modules:
	mkdir -p "$$(dirname "$(ARTIFACT_PSMOUSE_KO)")"
	@test -f "$(ARTIFACT_PSMOUSE_KO)" || { \
		echo "missing prebuilt psmouse module: $(ARTIFACT_PSMOUSE_KO)"; \
		exit 1; \
	}
