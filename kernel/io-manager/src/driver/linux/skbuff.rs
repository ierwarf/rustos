// RING3-MIGRATION-REFERENCE START: Linux .ko skbuff shims are explicit ring0
// compatibility substrate. Network policy belongs in netd.
use core::ffi::c_void;
use core::ptr;

unsafe extern "C" fn alloc_skb(size: u32, gfp: u32) -> *mut c_void {
    let total = (size as usize).saturating_add(512).max(2048);
    unsafe { super::base::__kmalloc_noprof(total, gfp) }
}

unsafe extern "C" fn free_skb(skb: *mut c_void) {
    unsafe { super::base::kfree(skb) };
}

unsafe extern "C" fn skb_put(_skb: *mut c_void, _len: u32) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn skb_to_sgvec(
    _skb: *mut c_void,
    _sg: *mut c_void,
    _offset: i32,
    _len: i32,
) -> i32 {
    0
}

unsafe extern "C" fn skb_partial_csum_set(_skb: *mut c_void, _start: u16, _off: u16) -> bool {
    false
}

unsafe extern "C" fn skb_page_frag_refill(_sz: u32, _pfrag: *mut c_void, _gfp: u32) -> bool {
    false
}

unsafe extern "C" fn skb_noop() {}

unsafe extern "C" fn skb_return_null() -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn skb_return_zero() -> i32 {
    0
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "__alloc_skb" | "napi_alloc_skb" | "build_skb" => Some(alloc_skb as *const () as usize),
        "consume_skb" | "dev_kfree_skb_any_reason" | "napi_consume_skb" => {
            Some(free_skb as *const () as usize)
        }
        "__pskb_pull_tail" => Some(skb_return_null as *const () as usize),
        "__skb_flow_dissect" => Some(skb_return_zero as *const () as usize),
        "skb_add_rx_frag_netmem" | "skb_clone_tx_timestamp" | "skb_tstamp_tx" => {
            Some(skb_noop as *const () as usize)
        }
        "skb_coalesce_rx_frag" => Some(skb_return_zero as *const () as usize),
        "skb_page_frag_refill" => Some(skb_page_frag_refill as *const () as usize),
        "skb_partial_csum_set" => Some(skb_partial_csum_set as *const () as usize),
        "skb_put" => Some(skb_put as *const () as usize),
        "skb_to_sgvec" => Some(skb_to_sgvec as *const () as usize),
        _ => None,
    }
}
// RING3-MIGRATION-REFERENCE END: Linux .ko skbuff compatibility substrate exception.
