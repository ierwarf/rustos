# SPDX-License-Identifier: MIT

RUSTOS_DVM_NVIDIA_OPEN_VERSION = $(NVIDIA_OPEN_VERSION)
RUSTOS_DVM_NVIDIA_OPEN_SITE = $(patsubst %/,%,$(dir $(NVIDIA_OPEN_URL)))
RUSTOS_DVM_NVIDIA_OPEN_SOURCE = $(notdir $(NVIDIA_OPEN_URL))
RUSTOS_DVM_NVIDIA_OPEN_LICENSE = MIT OR GPL-2.0-only (open modules), NVIDIA Software License (firmware)
RUSTOS_DVM_NVIDIA_OPEN_LICENSE_FILES = LICENSE
RUSTOS_DVM_NVIDIA_OPEN_REDISTRIBUTE = NO

# The official .run is a self-extracting archive and refuses an existing
# target.  Its kernel-open directory is the source-published module flavor;
# the similarly named kernel directory is intentionally never built.
define RUSTOS_DVM_NVIDIA_OPEN_EXTRACT_CMDS
	$(SHELL) $(RUSTOS_DVM_NVIDIA_OPEN_DL_DIR)/$(RUSTOS_DVM_NVIDIA_OPEN_SOURCE) \
		--extract-only --target $(@D)/tmp-extract
	chmod u+w -R $(@D)
	mv $(@D)/tmp-extract/* $(@D)/tmp-extract/.manifest $(@D)
	rm -rf $(@D)/tmp-extract
endef

RUSTOS_DVM_NVIDIA_OPEN_MODULE_SUBDIRS = kernel-open
RUSTOS_DVM_NVIDIA_OPEN_MODULE_MAKE_OPTS = \
	NV_KERNEL_SOURCES="$(LINUX_DIR)" \
	NV_KERNEL_OUTPUT="$(LINUX_DIR)" \
	NV_KERNEL_MODULES="nvidia nvidia-modeset nvidia-drm"

define RUSTOS_DVM_NVIDIA_OPEN_INSTALL_GSP_FIRMWARE
	$(INSTALL) -D -m 0644 $(@D)/firmware/gsp_ga10x.bin \
		$(TARGET_DIR)/lib/firmware/nvidia/$(RUSTOS_DVM_NVIDIA_OPEN_VERSION)/gsp_ga10x.bin
	$(INSTALL) -D -m 0644 $(@D)/firmware/gsp_tu10x.bin \
		$(TARGET_DIR)/lib/firmware/nvidia/$(RUSTOS_DVM_NVIDIA_OPEN_VERSION)/gsp_tu10x.bin
endef

$(eval $(kernel-module))
RUSTOS_DVM_NVIDIA_OPEN_POST_INSTALL_TARGET_HOOKS += RUSTOS_DVM_NVIDIA_OPEN_INSTALL_GSP_FIRMWARE
$(eval $(generic-package))
