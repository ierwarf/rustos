# SPDX-License-Identifier: MIT

RUSTOS_DVM_DISPLAY_VERSION = 14
RUSTOS_DVM_DISPLAY_SITE = $(BR2_EXTERNAL_RUSTOS_LINUX_DVM_PATH)/package/rustos-dvm-display/src
RUSTOS_DVM_DISPLAY_SITE_METHOD = local
RUSTOS_DVM_DISPLAY_LICENSE = MIT
RUSTOS_DVM_DISPLAY_DEPENDENCIES = libdrm

define RUSTOS_DVM_DISPLAY_BUILD_CMDS
	$(TARGET_CC) $(TARGET_CFLAGS) $(TARGET_LDFLAGS) -I$(STAGING_DIR)/usr/include/libdrm -std=c11 -Wall -Wextra -Werror \
		$(@D)/rustos-dvm-display.c -o $(@D)/rustos-dvm-display -ldrm
endef

define RUSTOS_DVM_DISPLAY_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/rustos-dvm-display \
		$(TARGET_DIR)/usr/libexec/rustos-dvm-display
endef

$(eval $(kernel-module))
$(eval $(generic-package))
