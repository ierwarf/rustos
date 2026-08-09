//! DVM block transport test implementations.
//!
//! The parent retains the stable libtest witness names consumed by formal
//! implementation-mutation evidence. These helpers hold only the associated
//! QEMU read-only and successor-generation assertions.

use super::*;

pub(super) fn dvm_attached_block_disk_requires_qemu_read_only_backing() {
    let mut command = Command::new("qemu-system-x86_64");
    append_dvm_virtual_storage(&mut command, Path::new("/tmp/rustos-dvm-block.img"));
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let drive = args
        .windows(2)
        .find_map(|args| (args[0] == "-drive").then_some(args[1].as_str()))
        .expect("DVM virtual-storage drive argument");
    assert!(drive.contains("id=dvm-storage-disk"));
    assert!(drive.contains("readonly=on"));
    let device = args
        .windows(2)
        .find_map(|args| (args[0] == "-device").then_some(args[1].as_str()))
        .expect("DVM virtual-storage device argument");
    assert!(device.starts_with("ide-cd,"));
    assert!(device.contains("bus=ide.0,unit=0"));
    assert!(!device.contains("logical_block_size="));
    assert!(!device.contains("physical_block_size="));
}

pub(super) fn dvm_block_transport_header_matches_read_only_qemu_backing() {
    let features = DVM_BLOCK_FEATURE_FLUSH;
    let header = dvm_read_only_block_header(7, 8192, 512, 4096, features);
    assert_eq!(header.generation, 7);
    assert_eq!(header.capacity_sectors, 8192);
    assert_eq!(header.logical_block_size, 512);
    assert_eq!(header.physical_block_size, 4096);
    assert_eq!(header.features, features);
    assert_eq!(header.flags, DVM_BLOCK_FLAG_READ_ONLY);
    assert_eq!(
        header.flags & (DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY),
        0
    );
}

pub(super) fn dvm_block_read_only_media_geometry_matches_atapi_capacity() {
    assert_eq!(DVM_BLOCK_MEDIA_BLOCK_BYTES, 2048);
    assert_eq!(DVM_BLOCK_MEDIA_FEATURES, DVM_BLOCK_FEATURE_FLUSH);
    let header = dvm_read_only_block_header(
        1,
        8192,
        DVM_BLOCK_MEDIA_BLOCK_BYTES,
        DVM_BLOCK_MEDIA_BLOCK_BYTES,
        DVM_BLOCK_MEDIA_FEATURES,
    );
    assert_eq!(header.logical_block_size, 2048);
    assert_eq!(header.physical_block_size, 2048);
    assert_eq!(header.capacity_sectors, 8192);
    assert_eq!(header.features, DVM_BLOCK_FEATURE_FLUSH);
}

pub(super) fn dvm_block_read_only_media_driver_closure_is_explicit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fragment = fs::read_to_string(root.join("driver-domains/linux/board/linux.fragment"))
        .expect("read Linux DVM kernel fragment");
    let verifier =
        fs::read_to_string(root.join("driver-domains/linux/scripts/verify-kernel-config.sh"))
            .expect("read Linux DVM kernel-config verifier");
    let startup = fs::read_to_string(
        root.join("driver-domains/linux/board/overlay/etc/init.d/S12rustos-dvm-block"),
    )
    .expect("read Linux DVM storage startup closure");
    let uio = fs::read_to_string(
        root.join("driver-domains/linux/package/rustos-dvm-block/src/rustos_dvm_block_uio.c"),
    )
    .expect("read Linux DVM block UIO cache contract");

    assert!(fragment.lines().any(|line| line == "CONFIG_BLK_DEV_SR=y"));
    assert!(
        verifier
            .lines()
            .any(|line| line == "    grep -qx \"${1}=y\" \"$config\" || {")
    );
    assert!(
        verifier
            .lines()
            .any(|line| line == "require_builtin CONFIG_BLK_DEV_SR")
    );
    assert!(startup.contains("built-in sr owns the immutable ATAPI"));
    assert!(!startup.contains("modprobe sr_mod"));
    assert!(uio.contains("!(shared->flags & IORESOURCE_PREFETCH)"));
    assert!(uio.contains("~_PAGE_CACHE_MASK"));
    assert!(uio.contains("coherent WB"));
}

pub(super) fn dvm_block_recovery_readiness_tracks_the_exact_successor_generation() {
    let mut header = dvm_read_only_block_header(2, 8192, 512, 4096, DVM_BLOCK_FEATURE_FLUSH);
    header.flags |= DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;
    assert!(dvm_block_header_matches_ready_generation(header, 2));
    assert!(!dvm_block_header_matches_ready_generation(header, 1));

    header.flags &= !DVM_BLOCK_FLAG_READ_ONLY;
    assert!(!dvm_block_header_matches_ready_generation(header, 2));
}
