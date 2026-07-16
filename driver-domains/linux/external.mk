# SPDX-License-Identifier: MIT

# This lock uses make-compatible assignments as well as shell-compatible
# assignments.  Include it before package makefiles so package metadata can
# share the exact fetch identity without changing Buildroot's current-package
# detection while a package infrastructure macro is evaluated.
include $(BR2_EXTERNAL_RUSTOS_LINUX_DVM_PATH)/sources.lock
include $(sort $(wildcard $(BR2_EXTERNAL_RUSTOS_LINUX_DVM_PATH)/package/*/*.mk))
