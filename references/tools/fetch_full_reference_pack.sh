#!/usr/bin/env bash
# SPDX-License-Identifier: CC0-1.0
set -euo pipefail

DEST="${1:-./upstream-sources}"
PROFILE="${PROFILE:-core}"   # core or extended
mkdir -p "$DEST"

clone_sparse() {
  local url="$1" name="$2"; shift 2
  local dir="$DEST/$name"
  if [[ ! -d "$dir/.git" ]]; then
    git clone --depth=1 --filter=blob:none --sparse "$url" "$dir"
  else
    git -C "$dir" fetch --depth=1 origin
    git -C "$dir" reset --hard FETCH_HEAD
  fi
  git -C "$dir" sparse-checkout set --no-cone "$@"
  git -C "$dir" rev-parse HEAD > "$dir/.REFERENCE_PACK_COMMIT"
}

clone_shallow() {
  local url="$1" name="$2"
  local dir="$DEST/$name"
  if [[ ! -d "$dir/.git" ]]; then
    git clone --depth=1 --filter=blob:none "$url" "$dir"
  else
    git -C "$dir" fetch --depth=1 origin
    git -C "$dir" reset --hard FETCH_HEAD
  fi
  git -C "$dir" rev-parse HEAD > "$dir/.REFERENCE_PACK_COMMIT"
}

# Core profile: highest-value source paths for a ~150k LOC OS.
clone_sparse https://github.com/torvalds/linux.git linux \
  /README /COPYING /MAINTAINERS /Documentation/process/ /Documentation/locking/ \
  /Documentation/RCU/ /Documentation/dev-tools/ /Documentation/memory-barriers.txt \
  /Documentation/atomic_t.txt /Documentation/ABI/ /Documentation/userspace-api/ \
  /tools/memory-model/ /kernel/locking/ /kernel/sched/ /kernel/rcu/ /kernel/fork.c \
  /fs/exec.c /fs/binfmt_elf.c /fs/namei.c /mm/memory.c /mm/mmap.c /ipc/ \
  /include/uapi/ /include/linux/compat.h /arch/x86/entry/

clone_sparse https://github.com/QubesOS/qubes-doc.git qubes-doc \
  /developer/system/ /developer/services/ /developer/building/ /README.md /LICENSE*
clone_sparse https://github.com/QubesOS/qubes-core-admin.git qubes-core-admin \
  /qubes/ /doc/ /tests/ /README* /LICENSE*
clone_sparse https://github.com/QubesOS/qubes-core-qrexec.git qubes-qrexec \
  /daemon/ /agent/ /libqrexec/ /README* /LICENSE*
clone_sparse https://github.com/xen-project/xen.git xen \
  /xen/include/public/ /xen/common/event_channel.c /xen/common/grant_table.c \
  /xen/common/domain.c /xen/common/schedule.c /xen/arch/x86/hvm/ /xen/arch/x86/pv/ /docs/
clone_sparse https://github.com/seL4/seL4.git sel4 \
  /src/ /include/ /libsel4/ /manual/ /configs/ /README.md /CAVEATS.md /LICENSE.md
clone_sparse https://github.com/seL4/l4v.git sel4-l4v /spec/ /proof/ /README.md /LICENSE*
clone_shallow https://github.com/seL4/rfcs.git sel4-rfcs
clone_sparse https://fuchsia.googlesource.com/fuchsia fuchsia \
  /src/starnix/ /zircon/kernel/ /zircon/system/public/zircon/ /docs/concepts/kernel/ /docs/contribute/
clone_sparse https://github.com/freebsd/freebsd-src.git freebsd-src \
  /sys/compat/linux/ /sys/amd64/linux/ /sys/arm64/linux/ /share/man/man4/linux.4 /COPYRIGHT
clone_sparse https://github.com/google/gvisor.git gvisor /pkg/sentry/ /pkg/abi/linux/ /runsc/ /docs/ /LICENSE
clone_shallow https://github.com/tlaplus/Examples.git tla-examples
clone_sparse https://github.com/google/syzkaller.git syzkaller /docs/ /sys/linux/ /pkg/report/ /LICENSE
clone_sparse https://github.com/herd/herdtools7.git herdtools7 /herd/ /litmus/ /catalogue/ /README.md /LICENSE*

if [[ "$PROFILE" == "extended" ]]; then
  clone_sparse https://github.com/NetBSD/src.git netbsd-src /sys/compat/linux/ /sys/kern/ /share/man/
  clone_sparse https://github.com/illumos/illumos-gate.git illumos-gate /usr/src/uts/common/brand/lx/ /usr/src/lib/brand/lx/ /usr/src/uts/common/os/
  clone_sparse https://github.com/wine-mirror/wine.git wine /server/ /dlls/ntdll/unix/ /dlls/wow64/ /include/ /docs/ /LICENSE*
  clone_sparse https://github.com/reactos/reactos.git reactos /ntoskrnl/ /dll/ntdll/ /sdk/include/ndk/ /subsystems/ /README.md /COPYING
  clone_sparse https://github.com/Stichting-MINIX-Research-Foundation/minix.git minix /minix/kernel/ /minix/servers/ /minix/include/ /docs/ /LICENSE
  clone_sparse https://github.com/HelenOS/helenos.git helenos /kernel/ /uspace/srv/ /uspace/lib/c/ /README.md /LICENSE*
  clone_shallow https://github.com/redox-os/kernel.git redox-kernel
  clone_sparse https://github.com/genodelabs/genode.git genode /repos/base/ /repos/base-hw/ /doc/ /LICENSE*
  clone_shallow https://github.com/kernkonzept/fiasco.git fiasco
  clone_shallow https://github.com/kernkonzept/l4re-core.git l4re-core
  clone_shallow https://github.com/apalache-mc/apalache.git apalache
  clone_shallow https://github.com/model-checking/kani.git kani
  clone_shallow https://github.com/tokio-rs/loom.git loom
  clone_sparse https://github.com/diffblue/cbmc.git cbmc /doc/ /src/ /regression/ /LICENSE
  clone_shallow https://github.com/riscv/riscv-isa-manual.git riscv-isa-manual
  clone_shallow https://github.com/oasis-tcs/virtio-spec.git virtio-spec
  clone_shallow https://github.com/devicetree-org/devicetree-specification.git devicetree-spec
  clone_shallow https://github.com/ARM-software/abi-aa.git arm-abi-aa
fi

python3 "$(dirname "$0")/snapshot_commits.py" "$DEST"
echo "Fetched profile=$PROFILE into $DEST"
