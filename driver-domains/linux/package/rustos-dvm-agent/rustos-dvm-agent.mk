# SPDX-License-Identifier: MIT

RUSTOS_DVM_AGENT_VERSION = 5
RUSTOS_DVM_AGENT_SITE = $(BR2_EXTERNAL_RUSTOS_LINUX_DVM_PATH)/package/rustos-dvm-agent/src
RUSTOS_DVM_AGENT_SITE_METHOD = local
RUSTOS_DVM_AGENT_LICENSE = MIT

define RUSTOS_DVM_AGENT_BUILD_CMDS
	$(TARGET_CC) $(TARGET_CFLAGS) $(TARGET_LDFLAGS) -std=c11 -Wall -Wextra -Werror \
		$(@D)/rustos-dvm-agent.c -o $(@D)/rustos-dvm-agent
endef

define RUSTOS_DVM_AGENT_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/rustos-dvm-agent \
		$(TARGET_DIR)/usr/libexec/rustos-dvm-agent
endef

$(eval $(generic-package))
