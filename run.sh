set -e

make build

echo "

====================================
Build completed. Starting QEMU...
====================================

"

  qemu-system-x86_64 \
    -bios OVMF.fd \
    -drive if=pflash,format=raw,readonly=on,file=OVMF.fd \
    -drive file=fat:rw:build,format=raw \
    -net none \
    -m 1G \
    -monitor none \
    -debugcon stdio \
    -global isa-debugcon.iobase=0xe9 \
    -d int -D qemu_interrupt.log

set +e

echo "

====================================
QEMU exited with code $?
====================================

"
