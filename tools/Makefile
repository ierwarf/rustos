ROOT_DIR ?= $(abspath ..)
include $(ROOT_DIR)/mk/config.mk

.PHONY: stage clean

stage:
	$(CARGO) xtask stage

clean:
	$(CARGO) clean
	rm -rf "$(BUILD_DIR)"
