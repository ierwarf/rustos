ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: build-console-demo

build-console-demo:
	mkdir -p "$$(dirname "$(ARTIFACT_PRINTF_DEMO_ELF)")"
	$(CC) "$(PRINTF_DEMO_SOURCE)" -o "$(ARTIFACT_PRINTF_DEMO_ELF)"
