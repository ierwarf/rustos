use super::*;

#[test]
fn byte_len_to_page_count_rounds_up() {
    assert_eq!(byte_len_to_page_count(1).unwrap(), 1);
    assert_eq!(byte_len_to_page_count(PAGE_4KIB).unwrap(), 1);
    assert_eq!(byte_len_to_page_count(PAGE_4KIB + 1).unwrap(), 2);
}

#[test]
fn validate_user_page_range_rejects_unaligned_or_oob() {
    assert_eq!(
        validate_user_page_range(VirtAddr::new(USER_SPACE_BASE + 1), 1),
        Err(AddressSpaceError::AddressNotPageAligned)
    );
    assert_eq!(
        validate_user_page_range(VirtAddr::new(USER_SPACE_END_EXCLUSIVE), 1),
        Err(AddressSpaceError::AddressOutOfRange)
    );
    assert!(validate_user_page_range(VirtAddr::new(USER_SPACE_BASE), 1).is_ok());
}

#[test]
fn user_page_flags_enforce_wx_and_reject_huge_pages() {
    assert_eq!(
        normalize_user_page_flags(PageTableFlags::WRITABLE),
        Err(AddressSpaceError::ProtectionViolation)
    );
    assert_eq!(
        normalize_user_page_flags(PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE),
        Ok(PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE)
    );
    assert_eq!(
        normalize_user_page_flags(PageTableFlags::HUGE_PAGE | PageTableFlags::NO_EXECUTE),
        Err(AddressSpaceError::HugePageConflict)
    );
}

#[test]
fn no_access_keeps_owned_bytes_but_denies_hardware_and_checked_user_access() {
    let none = normalize_user_page_flags(USER_INACCESSIBLE_LEAF | PageTableFlags::NO_EXECUTE).unwrap();
    assert!(none.contains(PageTableFlags::PRESENT | USER_INACCESSIBLE_LEAF));
    assert!(!none.intersects(PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE));
    assert_eq!(validate_user_page_access(none, UserBufferAccess::Read), Err(AddressSpaceError::ProtectionViolation));
    assert_eq!(validate_user_page_access(none, UserBufferAccess::Write), Err(AddressSpaceError::ProtectionViolation));
    assert!(normalize_user_page_flags(none | PageTableFlags::WRITABLE).is_err());
    let hidden = preserve_4k_leaf_pat(none | ROOT_OWNED_USER_LEAF, none);
    assert!(hidden.contains(ROOT_OWNED_USER_LEAF | USER_INACCESSIBLE_LEAF));
    let read = normalize_user_page_flags(PageTableFlags::NO_EXECUTE).unwrap();
    let restored = preserve_4k_leaf_pat(hidden, read);
    assert!(restored.contains(ROOT_OWNED_USER_LEAF | PageTableFlags::USER_ACCESSIBLE));
    assert!(!restored.contains(USER_INACCESSIBLE_LEAF));
}

#[test]
fn mprotect_preserves_write_combine_pat_on_4k_leaf() {
    let existing = PageTableFlags::PRESENT
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE
        | PageTableFlags::HUGE_PAGE
        | ROOT_OWNED_USER_LEAF;
    let requested =
        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::NO_EXECUTE;
    let preserved = preserve_4k_leaf_pat(existing, requested);
    assert!(preserved.contains(PageTableFlags::HUGE_PAGE));
    assert!(preserved.contains(ROOT_OWNED_USER_LEAF));
    assert!(!preserved.contains(PageTableFlags::WRITABLE));
}

#[test]
fn mprotect_cannot_bypass_a_cow_write_fault() {
    let existing = PageTableFlags::PRESENT
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE
        | ROOT_OWNED_USER_LEAF
        | COW_USER_LEAF;
    let requested = PageTableFlags::PRESENT
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE;
    let preserved = preserve_4k_leaf_pat(existing, requested);
    assert!(preserved.contains(ROOT_OWNED_USER_LEAF | COW_USER_LEAF));
    assert!(!preserved.contains(PageTableFlags::WRITABLE));
}

#[test]
fn protection_span_preflight_rejects_a_hole_before_commit() {
    let mut visited = 0;
    let accepted = validate_protection_span(4, |page_index| {
        assert_eq!(page_index, visited);
        visited += 1;
        page_index != 2
    });
    assert!(!accepted);
    assert_eq!(visited, 3);

    assert!(validate_protection_span(4, |_| true));
}

#[test]
fn unmap_descriptor_plan_is_complete_before_first_pte_remove() {
    let start = VirtAddr::new(USER_SPACE_BASE);
    let mut visited = 0_usize;
    let rejected = plan_user_page_unmap(start, 3, |virt| {
        let page = usize::try_from((virt.as_u64() - start.as_u64()) / PAGE_4KIB_U64).unwrap();
        assert_eq!(page, visited);
        visited += 1;
        if page == 1 {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        Ok(PhysAddr::new((page as u64 + 1) * PAGE_4KIB_U64))
    });
    assert_eq!(rejected, Err(AddressSpaceError::InvalidFrameOwnership));
    assert_eq!(visited, 2);

    let planned = plan_user_page_unmap(start, 3, |virt| {
        let page = (virt.as_u64() - start.as_u64()) / PAGE_4KIB_U64;
        Ok(PhysAddr::new((page + 1) * PAGE_4KIB_U64))
    })
    .unwrap();
    assert_eq!(planned.len(), 3);
    assert_eq!(planned[2], 3 * PAGE_4KIB_U64);
}

#[test]
fn external_frames_must_be_aligned_and_below_the_physical_ceiling() {
    let limit = 1_u64 << 40;
    assert!(external_frame_is_encodable(0x1000, limit));
    assert!(external_frame_is_encodable(limit - PAGE_4KIB_U64, limit));
    assert!(!external_frame_is_encodable(0x1001, limit));
    assert!(!external_frame_is_encodable(limit, limit));
    assert!(!external_frame_is_encodable(u64::MAX & !(PAGE_4KIB_U64 - 1), limit));
}
