set pagination off
set confirm off
set architecture i386:x86-64
file /home/hongii/rustos/build/artifacts/nucleus.elf
target remote :1234

# Prekernel is linked at a fixed image base by xtask.
add-symbol-file /home/hongii/rustos/build/artifacts/prekernel.elf 0x100000

echo Connected to RustOS debug target.

echo Nucleus symbols: /home/hongii/rustos/build/artifacts/nucleus.elf

echo Prekernel symbols: /home/hongii/rustos/build/artifacts/prekernel.elf

echo Module symbols can be loaded from /dev/debug0 snapshots once the guest is up.

