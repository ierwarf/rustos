# SPDX-License-Identifier: MIT

RUSTOS_DVM_NET_VERSION = 1
RUSTOS_DVM_NET_SITE = $(BR2_EXTERNAL_RUSTOS_LINUX_DVM_PATH)/package/rustos-dvm-net/src
RUSTOS_DVM_NET_SITE_METHOD = local
RUSTOS_DVM_NET_LICENSE = MIT

define RUSTOS_DVM_NET_BUILD_CMDS
	$(TARGET_CC) $(TARGET_CFLAGS) $(TARGET_LDFLAGS) -std=c11 -Wall -Wextra -Werror \
		$(@D)/rustos-dvm-net.c -o $(@D)/rustos-dvm-net
endef

define RUSTOS_DVM_NET_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/rustos-dvm-net \
		$(TARGET_DIR)/usr/libexec/rustos-dvm-net
endef

$(eval $(generic-package))
