# SPDX-License-Identifier: MIT

RUSTOS_DVM_DISPLAY_VERSION = 18
RUSTOS_DVM_DISPLAY_SITE = $(BR2_EXTERNAL_RUSTOS_LINUX_DVM_PATH)/package/rustos-dvm-display/src
RUSTOS_DVM_DISPLAY_SITE_METHOD = local
RUSTOS_DVM_DISPLAY_LICENSE = MIT
RUSTOS_DVM_DISPLAY_DEPENDENCIES = host-pkgconf libdrm libegl libgbm libgles

define RUSTOS_DVM_DISPLAY_BUILD_CMDS
	$(TARGET_CC) $(TARGET_CFLAGS) $(TARGET_LDFLAGS) -I$(STAGING_DIR)/usr/include/libdrm -std=c11 -Wall -Wextra -Werror \
		$(@D)/rustos-dvm-display.c $(@D)/rustos-dvm-gpu-runtime.c \
		-o $(@D)/rustos-dvm-display -lEGL -lGLESv2 -lgbm -ldrm
	$(TARGET_CC) $(TARGET_CFLAGS) $(TARGET_LDFLAGS) -I$(STAGING_DIR)/usr/include/libdrm -std=c11 -Wall -Wextra -Werror \
		$(@D)/rustos-dvm-gpu-probe.c -o $(@D)/rustos-dvm-gpu-probe \
		-lEGL -lGLESv2 -lgbm -ldrm
endef

define RUSTOS_DVM_DISPLAY_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/rustos-dvm-display \
		$(TARGET_DIR)/usr/libexec/rustos-dvm-display
	$(INSTALL) -D -m 0755 $(@D)/rustos-dvm-gpu-probe \
		$(TARGET_DIR)/usr/libexec/rustos-dvm-gpu-probe
endef

$(eval $(kernel-module))
$(eval $(generic-package))
