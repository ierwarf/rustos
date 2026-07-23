# SPDX-License-Identifier: MIT

RUSTOS_DVM_BLOCK_VERSION = 1
RUSTOS_DVM_BLOCK_SITE = $(BR2_EXTERNAL_RUSTOS_LINUX_DVM_PATH)/package/rustos-dvm-block/src
RUSTOS_DVM_BLOCK_SITE_METHOD = local
RUSTOS_DVM_BLOCK_LICENSE = MIT, GPL-2.0-only
RUSTOS_DVM_BLOCK_DEPENDENCIES = linux

define RUSTOS_DVM_BLOCK_BUILD_CMDS
	$(TARGET_CC) $(TARGET_CFLAGS) $(TARGET_LDFLAGS) -std=c11 -Wall -Wextra -Werror \
		$(@D)/rustos-dvm-block.c -o $(@D)/rustos-dvm-block
endef

define RUSTOS_DVM_BLOCK_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/rustos-dvm-block \
		$(TARGET_DIR)/usr/libexec/rustos-dvm-block
endef

$(eval $(kernel-module))
$(eval $(generic-package))
