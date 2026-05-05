use core::ffi::c_void;

unsafe extern "C" fn compat_zero() -> usize {
    0
}

unsafe extern "C" fn compat_null() -> *mut c_void {
    core::ptr::null_mut()
}

unsafe extern "C" fn alloc_skb(size: u32, gfp: u32) -> *mut c_void {
    let total = (size as usize).saturating_add(512).max(2048);
    unsafe { super::base::__kmalloc_noprof(total, gfp) }
}

unsafe extern "C" fn free_skb(skb: *mut c_void) {
    unsafe { super::base::kfree(skb) };
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "__alloc_skb" | "napi_alloc_skb" | "build_skb" => Some(alloc_skb as *const () as usize),
        "consume_skb" | "dev_kfree_skb_any_reason" | "napi_consume_skb" => {
            Some(free_skb as *const () as usize)
        }
        _ if is_stubbed_skb_pointer_symbol(name) => Some(compat_null as *const () as usize),
        _ if is_stubbed_skb_symbol(name) => Some(compat_zero as *const () as usize),
        _ => None,
    }
}

fn is_stubbed_skb_symbol(name: &str) -> bool {
    name.starts_with("skb_")
        || name.starts_with("__skb_")
        || name.starts_with("__pskb_")
        || matches!(
            name,
            "skb_add_rx_frag_netmem"
                | "skb_clone_tx_timestamp"
                | "skb_coalesce_rx_frag"
                | "skb_page_frag_refill"
                | "skb_partial_csum_set"
                | "skb_put"
                | "skb_to_sgvec"
                | "skb_tstamp_tx"
        )
}

fn is_stubbed_skb_pointer_symbol(name: &str) -> bool {
    matches!(name, "skb_clone")
}
