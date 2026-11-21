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
    assert!(drive.contains("cache=none"));
    // Exactly one drive and one frontend: the storage DVM admits one
    // controller, and the immutable backing must not gain a second path.
    assert_eq!(args.iter().filter(|arg| arg.as_str() == "-drive").count(), 1);
    assert_eq!(args.iter().filter(|arg| arg.as_str() == "-device").count(), 1);
    let device = args
        .windows(2)
        .find_map(|args| (args[0] == "-device").then_some(args[1].as_str()))
        .expect("DVM virtual-storage device argument");
    // The paravirtual queued frontend; never the emulated ATAPI/IDE/AHCI ones.
    assert!(device.starts_with("virtio-blk-pci,drive=dvm-storage-disk,"));
    assert!(!device.contains("ide-"));
    assert!(!device.contains("bus=ide"));
    // The device must present exactly the geometry the signed header declares.
    assert!(device.contains(&format!(
        "logical_block_size={DVM_STORAGE_LOGICAL_BLOCK_BYTES}"
    )));
    assert!(device.contains(&format!(
        "physical_block_size={DVM_STORAGE_PHYSICAL_BLOCK_BYTES}"
    )));
}

/// The writable (`--writable-storage`) frontend is the same virtio-blk device
/// and geometry over a writable backing; only the drive's `readonly=on` differs.
pub(super) fn dvm_writable_storage_uses_the_same_virtio_frontend() {
    let mut read_only = Command::new("qemu-system-x86_64");
    append_dvm_virtual_storage(&mut read_only, Path::new("/tmp/rustos-dvm-block.img"));
    let mut writable = Command::new("qemu-system-x86_64");
    append_dvm_writable_virtual_storage(&mut writable, Path::new("/tmp/rustos-dvm-block.img"));
    let collect = |command: &Command| {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    let (read_only, writable) = (collect(&read_only), collect(&writable));
    assert_eq!(read_only.len(), 4);
    assert_eq!(writable.len(), 4);
    assert_eq!(read_only[3], writable[3], "frontend and geometry are shared");
    assert!(!writable[1].contains("readonly=on"));
    assert!(writable[1].contains("id=dvm-storage-disk"));
    assert!(writable[1].contains("cache=none"));
    assert!(writable[3].starts_with("virtio-blk-pci,drive=dvm-storage-disk,"));
}

/// The DVM machine must not carry q35's implicit ICH9 AHCI function: the
/// storage DVM admits exactly one controller and that is the virtio-blk disk.
pub(super) fn dvm_machine_has_no_implicit_storage_controller() {
    assert!(dvm_machine().split(',').any(|property| property == "sata=off"));
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

pub(super) fn dvm_block_read_only_media_geometry_is_the_declared_storage_geometry() {
    // The declared harness contract: 512-byte logical and physical blocks,
    // matching every staged volume image, with capacity in 512-byte protocol
    // sectors and a FLUSH-only feature word (never FUA on read-only media).
    assert_eq!(DVM_STORAGE_LOGICAL_BLOCK_BYTES, 512);
    assert_eq!(DVM_STORAGE_PHYSICAL_BLOCK_BYTES, 512);
    assert_eq!(DVM_BLOCK_MEDIA_FEATURES, DVM_BLOCK_FEATURE_FLUSH);
    let header = dvm_read_only_block_header(
        1,
        8192,
        DVM_STORAGE_LOGICAL_BLOCK_BYTES,
        DVM_STORAGE_PHYSICAL_BLOCK_BYTES,
        DVM_BLOCK_MEDIA_FEATURES,
    );
    assert_eq!(header.logical_block_size, 512);
    assert_eq!(header.physical_block_size, 512);
    assert_eq!(header.capacity_sectors, 8192);
    assert_eq!(header.features, DVM_BLOCK_FEATURE_FLUSH);
    assert_eq!(header.flags, DVM_BLOCK_FLAG_READ_ONLY);

    // Capacity is derived from the backing size in 512-byte protocol sectors
    // and must be a whole number of logical blocks; there is no CD-media
    // (2048-byte) alignment requirement any more.
    assert_eq!(dvm_storage_capacity_sectors(512).unwrap(), 1);
    assert_eq!(dvm_storage_capacity_sectors(4096).unwrap(), 8);
    assert_eq!(dvm_storage_capacity_sectors(512 * 5).unwrap(), 5);
    assert!(dvm_storage_capacity_sectors(0).is_err());
    assert!(dvm_storage_capacity_sectors(513).is_err());

    // A live header is accepted only for exactly the declared geometry over
    // exactly the backing disk's capacity.
    assert!(dvm_storage_header_geometry_matches(&header, 8192 * 512));
    assert!(!dvm_storage_header_geometry_matches(&header, 8191 * 512));
    let mut wrong = header;
    wrong.logical_block_size = 2048;
    wrong.physical_block_size = 2048;
    assert!(!dvm_storage_header_geometry_matches(&wrong, 8192 * 512));
    let mut wrong = header;
    wrong.physical_block_size = 4096;
    assert!(!dvm_storage_header_geometry_matches(&wrong, 8192 * 512));
}

/// The signed header geometry of a created read-only aperture comes from the
/// declared geometry, and a non-512-multiple private disk is refused.
pub(super) fn create_read_only_dvm_block_aperture_signs_declared_geometry() {
    use driver_domain_protocol::{DVM_BLOCK_HEADER_RECORD_BYTES, DvmBlockHeader};

    let temp = tempfile::tempdir().unwrap();
    let key_path = temp.path().join("epoch.key");
    crate::storage_epoch::load_or_create_signing_key(&key_path).unwrap();
    let aperture = temp.path().join("aperture.bin");

    let unaligned = temp.path().join("unaligned.img");
    fs::write(&unaligned, [0_u8; 513]).unwrap();
    assert!(super::super::create_dvm_block_aperture(&aperture, &unaligned, &key_path).is_err());

    // Not a multiple of the former 2048-byte ATAPI block, but a valid 512-byte
    // block image: the harness no longer imposes media-specific alignment.
    let disk = temp.path().join("ro-disk.img");
    fs::write(&disk, vec![0_u8; 512 * 13]).unwrap();
    super::super::create_dvm_block_aperture(&aperture, &disk, &key_path).unwrap();
    let bytes = fs::read(&aperture).unwrap();
    let header_bytes: &[u8; DVM_BLOCK_HEADER_RECORD_BYTES] =
        bytes[..DVM_BLOCK_HEADER_RECORD_BYTES].try_into().unwrap();
    let header = DvmBlockHeader::decode(header_bytes).unwrap();
    assert!(header.is_valid());
    assert_eq!(header.generation, 1);
    assert_eq!(header.capacity_sectors, 13);
    assert_eq!(header.logical_block_size, DVM_STORAGE_LOGICAL_BLOCK_BYTES);
    assert_eq!(header.physical_block_size, DVM_STORAGE_PHYSICAL_BLOCK_BYTES);
    assert_eq!(header.flags, DVM_BLOCK_FLAG_READ_ONLY);
    assert_eq!(header.features, DVM_BLOCK_MEDIA_FEATURES);

    // Epoch rotation keeps the declared geometry and refuses a header whose
    // geometry diverged from the backing disk.
    let successor = super::super::rotate_dvm_block_epoch(&aperture, &disk, &key_path).unwrap();
    assert_eq!(successor, 2);
    fs::write(&disk, vec![0_u8; 512 * 14]).unwrap();
    assert!(super::super::rotate_dvm_block_epoch(&aperture, &disk, &key_path).is_err());
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
    let relay = fs::read_to_string(
        root.join("driver-domains/linux/package/rustos-dvm-block/src/rustos-dvm-block.c"),
    )
    .expect("read Linux DVM block relay");
    let uio = fs::read_to_string(
        root.join("driver-domains/linux/package/rustos-dvm-block/src/rustos_dvm_block_uio.c"),
    )
    .expect("read Linux DVM block UIO cache contract");

    // virtio-pci and virtio_blk are built in, so S12 loads no module for the
    // harness disk; the verifier pins that and forbids optical/ATAPI media.
    assert!(fragment.lines().any(|line| line == "CONFIG_VIRTIO_PCI=y"));
    assert!(fragment.lines().any(|line| line == "CONFIG_VIRTIO_BLK=y"));
    assert!(fragment.lines().any(|line| line == "# CONFIG_BLK_DEV_SR is not set"));
    assert!(!fragment.lines().any(|line| line == "CONFIG_BLK_DEV_SR=y"));
    assert!(
        verifier
            .lines()
            .any(|line| line == "    grep -qx \"${1}=y\" \"$config\" || {")
    );
    assert!(
        verifier
            .lines()
            .any(|line| line == "require_builtin CONFIG_VIRTIO_BLK")
    );
    assert!(
        verifier
            .lines()
            .any(|line| line == "require_builtin CONFIG_VIRTIO_PCI")
    );
    assert!(
        verifier
            .lines()
            .any(|line| line == "require_disabled CONFIG_BLK_DEV_SR")
    );
    // S12 counts and admits the same closed controller class set as the relay.
    assert!(startup.contains("0x0106*) echo ahci ;;"));
    assert!(startup.contains("0x0108*) echo nvme ;;"));
    assert!(startup.contains("0x1001|0x1042) echo virtio-blk ;;"));
    assert!(startup.contains("virtio-blk:virtio-pci)"));
    assert!(startup.contains("modprobe sd_mod"));
    assert!(!startup.contains("modprobe sr_mod"));
    for class in ["PCI_CLASS_STORAGE_SATA", "PCI_CLASS_STORAGE_NVM", "PCI_CLASS_STORAGE_SCSI"] {
        assert!(relay.contains(class), "relay must classify {class}");
    }
    assert!(relay.contains("\"virtio_blk\""));
    assert!(relay.contains("VIRTIO_ID_BLOCK"));
    // The relay reads the geometry from the kernel and never hardcodes media.
    assert!(relay.contains("ioctl(device->fd, BLKSSZGET, &logical)"));
    assert!(relay.contains("ioctl(device->fd, BLKPBSZGET, &physical)"));
    assert!(relay.contains("ioctl(device->fd, BLKROGET, &read_only)"));
    assert!(relay.contains("(header_read_only ? O_RDONLY : O_RDWR)"));
    assert!(uio.contains("!(shared->flags & IORESOURCE_PREFETCH)"));
    assert!(uio.contains("mapped = ioremap_wc(shared->start, sizeof(bytes));"));
    assert!(uio.contains("pgprot_writecombine(vma->vm_page_prot)"));
    assert!(relay.contains("wc_store_fence();"));
}

pub(super) fn dvm_block_recovery_readiness_tracks_the_exact_successor_generation() {
    let mut header = dvm_read_only_block_header(2, 8192, 512, 4096, DVM_BLOCK_FEATURE_FLUSH);
    header.flags |= DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;
    assert!(dvm_block_header_matches_ready_generation(header, 2));
    assert!(!dvm_block_header_matches_ready_generation(header, 1));

    header.flags &= !DVM_BLOCK_FLAG_READ_ONLY;
    assert!(!dvm_block_header_matches_ready_generation(header, 2));
}

#[test]
pub(super) fn dvm_writable_block_header_omits_read_only_and_sets_flush_fua() {
    use driver_domain_protocol::DVM_BLOCK_FEATURE_FUA;

    let header = super::super::dvm_writable_block_header(1, 8192, 512, 4096, 0);
    assert_eq!(header.generation, 1);
    assert_eq!(header.capacity_sectors, 8192);
    assert_eq!(header.logical_block_size, 512);
    assert_eq!(header.physical_block_size, 4096);
    assert_eq!(header.flags & DVM_BLOCK_FLAG_READ_ONLY, 0);
    assert_eq!(
        header.flags & (DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY),
        0
    );
    assert_eq!(
        header.features & (DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA),
        DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA
    );
    assert!(header.with_epoch_signature([1; 64]).is_valid());
}

#[test]
pub(super) fn create_writable_dvm_block_aperture_signs_512_writable_header() {
    use driver_domain_protocol::{
        DVM_BLOCK_FEATURE_FUA, DVM_BLOCK_HEADER_RECORD_BYTES, DvmBlockHeader,
    };

    let temp = tempfile::tempdir().unwrap();
    let key_path = temp.path().join("epoch.key");
    crate::storage_epoch::load_or_create_signing_key(&key_path).unwrap();

    let unaligned = temp.path().join("unaligned.img");
    fs::write(&unaligned, [0_u8; 513]).unwrap();
    let aperture = temp.path().join("aperture.bin");
    assert!(super::super::create_writable_dvm_block_aperture(
        &aperture,
        &unaligned,
        &key_path
    )
    .is_err());

    let disk = temp.path().join("rw-disk.img");
    fs::write(&disk, vec![0_u8; 4096]).unwrap();
    super::super::create_writable_dvm_block_aperture(&aperture, &disk, &key_path).unwrap();

    let bytes = fs::read(&aperture).unwrap();
    let header_bytes: &[u8; DVM_BLOCK_HEADER_RECORD_BYTES] =
        bytes[..DVM_BLOCK_HEADER_RECORD_BYTES].try_into().unwrap();
    let header = DvmBlockHeader::decode(header_bytes).unwrap();
    assert!(header.is_valid());
    assert_eq!(header.generation, 1);
    assert_eq!(header.capacity_sectors, 8);
    assert_eq!(header.logical_block_size, 512);
    assert_eq!(header.physical_block_size, 512);
    assert_eq!(header.flags & DVM_BLOCK_FLAG_READ_ONLY, 0);
    assert_eq!(
        header.features & (DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA),
        DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA
    );
}
