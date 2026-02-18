make build

qemu-system-x86_64 \
    -bios OVMF.fd \
    -drive if=pflash,format=raw,readonly=on,file=OVMF.fd \
    -drive file=fat:rw:build,format=raw \
    -net none \
    -m 1G \
    -monitor stdio \
    -d int -D qemu_interrupt.log