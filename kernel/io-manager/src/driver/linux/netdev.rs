use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::ptr;

struct CompatData(UnsafeCell<[usize; 128]>);

unsafe impl Sync for CompatData {}

static COMPAT_DATA: CompatData = CompatData(UnsafeCell::new([0; 128]));
const COMPAT_CPUMASK_BYTES: usize = core::mem::size_of::<usize>() * 16;

unsafe extern "C" fn compat_printk(_fmt: *const c_char) -> i32 {
    0
}

unsafe extern "C" fn register_netdevice(dev: *mut c_void) -> i32 {
    if dev.is_null() {
        return -22;
    }
    crate::network::register_linux_netdev(
        dev as usize,
        crate::network::current_linux_netdev_transport(),
    )
}

unsafe extern "C" fn unregister_netdev(dev: *mut c_void) {
    if !dev.is_null() {
        crate::network::unregister_linux_netdev(dev as usize);
    }
}

unsafe extern "C" fn alloc_etherdev_mqs(sizeof_priv: i32, txqs: u32, rxqs: u32) -> *mut c_void {
    let Some(queue_count) = (txqs as usize).checked_add(rxqs as usize) else {
        return ptr::null_mut();
    };
    let Some(size) = core::mem::size_of::<usize>()
        .checked_mul(512)
        .and_then(|base| base.checked_add(sizeof_priv.max(0) as usize))
        .and_then(|base| base.checked_add(queue_count.checked_mul(64)?))
    else {
        return ptr::null_mut();
    };
    let dev = unsafe { super::base::__kmalloc_noprof(size.max(4096), 0) };
    if dev.is_null() {
        return ptr::null_mut();
    }
    crate::network::allocate_linux_netdev(dev as usize, sizeof_priv.max(0) as usize, txqs, rxqs);
    dev
}

unsafe extern "C" fn free_netdev(dev: *mut c_void) {
    if dev.is_null() {
        return;
    }
    crate::network::free_linux_netdev(dev as usize);
    unsafe { super::base::kfree(dev) };
}

unsafe extern "C" fn netif_carrier_on(dev: *mut c_void) {
    if !dev.is_null() {
        crate::network::set_linux_netdev_carrier(dev as usize, true);
    }
}

unsafe extern "C" fn netif_carrier_off(dev: *mut c_void) {
    if !dev.is_null() {
        crate::network::set_linux_netdev_carrier(dev as usize, false);
    }
}

unsafe extern "C" fn compat_noop() {}

unsafe extern "C" fn compat_return_zero() -> i32 {
    0
}

unsafe extern "C" fn compat_return_one() -> i32 {
    1
}

unsafe extern "C" fn compat_return_null() -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn alloc_cpumask_var_node(mask: *mut *mut c_void, gfp: u32, _node: i32) -> bool {
    if mask.is_null() {
        return false;
    }
    let ptr = unsafe { super::base::__kmalloc_noprof(COMPAT_CPUMASK_BYTES, gfp) };
    if ptr.is_null() {
        return false;
    }
    unsafe {
        *mask = ptr;
    }
    true
}

unsafe extern "C" fn free_cpumask_var(mask: *mut c_void) {
    unsafe { super::base::kfree(mask) };
}

unsafe extern "C" fn napi_alloc_frag_align(size: usize, _align_mask: usize) -> *mut c_void {
    unsafe { super::base::__kmalloc_noprof(size, 0) }
}

unsafe extern "C" fn netdev_rss_key_fill(buf: *mut c_void, len: usize) {
    unsafe { super::runtime::get_random_bytes(buf, len) };
}

unsafe extern "C" fn passthru_features_check(
    _skb: *mut c_void,
    _dev: *mut c_void,
    features: u64,
) -> u64 {
    features
}

unsafe extern "C" fn bpf_prog_add(prog: *mut c_void, _increment: i32) -> *mut c_void {
    prog
}

unsafe extern "C" fn bpf_prog_sub(prog: *mut c_void, _decrement: i32) -> *mut c_void {
    prog
}

unsafe extern "C" fn bpf_prog_put(_prog: *mut c_void) {}

fn compat_data_addr() -> usize {
    COMPAT_DATA.0.get() as usize
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    if let Some(symbol) = resolve_symbol_meta(name) {
        return Some(symbol.addr);
    }
    match name {
        "__dynamic_netdev_dbg"
        | "netdev_printk"
        | "netdev_err"
        | "netdev_warn"
        | "_dev_err"
        | "_dev_info"
        | "_dev_warn"
        | "__warn_printk" => Some(compat_printk as *const () as usize),
        _ if is_netdev_data_symbol(name) => Some(compat_data_addr()),
        "__cpuhp_setup_state" | "__cpuhp_state_add_instance" => {
            Some(compat_return_zero as *const () as usize)
        }
        "__cpuhp_remove_state"
        | "__cpuhp_state_remove_instance"
        | "__napi_schedule"
        | "__netif_napi_del_locked"
        | "__netif_set_xps_queue"
        | "bpf_warn_invalid_xdp_action"
        | "cpus_read_lock"
        | "cpus_read_unlock"
        | "do_trace_netlink_extack"
        | "dql_completed"
        | "dql_reset"
        | "eth_commit_mac_addr_change"
        | "ethtool_sprintf"
        | "napi_disable"
        | "napi_enable"
        | "net_dim"
        | "net_dim_free_irq_moder"
        | "net_dim_init_irq_moder"
        | "net_dim_work_cancel"
        | "net_failover_destroy"
        | "netdev_notify_peers"
        | "netif_device_attach"
        | "netif_device_detach"
        | "netif_napi_add_weight_locked"
        | "netif_queue_set_napi"
        | "netif_schedule_queue"
        | "netif_tx_lock"
        | "netif_tx_stop_all_queues"
        | "netif_tx_unlock"
        | "netif_tx_wake_queue"
        | "nf_conntrack_destroy"
        | "rtnl_lock"
        | "rtnl_unlock"
        | "synchronize_net"
        | "xdp_do_flush"
        | "xdp_features_clear_redirect_target"
        | "xdp_features_set_redirect_target"
        | "xdp_return_frame"
        | "xdp_return_frame_rx_napi"
        | "xdp_rxq_info_unreg"
        | "xdp_warn"
        | "xp_dma_unmap"
        | "xp_free"
        | "xsk_set_tx_need_wakeup"
        | "xsk_tx_completed" => Some(compat_noop as *const () as usize),
        "__xdp_rxq_info_reg"
        | "dev_addr_mod"
        | "eth_prepare_mac_addr_change"
        | "eth_validate_addr"
        | "ethtool_op_get_link"
        | "ethtool_op_get_ts_info"
        | "ethtool_virtdev_set_link_ksettings"
        | "gro_receive_skb"
        | "napi_complete_done"
        | "napi_schedule_prep"
        | "net_dim_get_rx_irq_moder"
        | "netif_set_real_num_rx_queues"
        | "netif_set_real_num_tx_queues"
        | "skb_coalesce_rx_frag"
        | "xdp_do_redirect"
        | "xdp_master_redirect"
        | "xdp_rxq_info_reg_mem_model"
        | "xp_alloc_batch"
        | "xp_dma_map"
        | "xp_set_rxq_info"
        | "xsk_tx_peek_release_desc_batch"
        | "xsk_uses_need_wakeup" => Some(compat_return_zero as *const () as usize),
        "__napi_alloc_frag_align" => Some(napi_alloc_frag_align as *const () as usize),
        "alloc_cpumask_var_node" => Some(alloc_cpumask_var_node as *const () as usize),
        "bpf_dispatcher_xdp_func" => Some(compat_return_zero as *const () as usize),
        "bpf_prog_add" => Some(bpf_prog_add as *const () as usize),
        "bpf_prog_put" => Some(bpf_prog_put as *const () as usize),
        "bpf_prog_sub" => Some(bpf_prog_sub as *const () as usize),
        "eth_type_trans" => Some(compat_return_zero as *const () as usize),
        "free_cpumask_var" => Some(free_cpumask_var as *const () as usize),
        "net_failover_create" => Some(compat_return_null as *const () as usize),
        "net_ratelimit" => Some(compat_return_one as *const () as usize),
        "netdev_rss_key_fill" => Some(netdev_rss_key_fill as *const () as usize),
        "netdev_stat_queue_sum" => Some(compat_return_zero as *const () as usize),
        "passthru_features_check" => Some(passthru_features_check as *const () as usize),
        "xdp_convert_zc_to_xdp_frame" => Some(compat_return_null as *const () as usize),
        "xp_raw_get_dma" => Some(compat_return_zero as *const () as usize),
        _ => None,
    }
}

pub(crate) fn resolve_symbol_meta(name: &str) -> Option<super::LinuxCompatSymbol> {
    super::linux_compat_symbols!(name, {
        "alloc_etherdev_mqs" => alloc_etherdev_mqs;
        "free_netdev" => free_netdev;
        "register_netdev" => register_netdevice;
        "register_netdevice" => register_netdevice;
        "unregister_netdev" => unregister_netdev;
        "unregister_netdevice_queue" => unregister_netdev;
        "netif_carrier_on" => netif_carrier_on;
        "netif_carrier_off" => netif_carrier_off;
        "__dynamic_netdev_dbg" => compat_printk, preserve_stack_tail;
        "netdev_printk" => compat_printk, preserve_stack_tail;
        "netdev_err" => compat_printk, preserve_stack_tail;
        "netdev_warn" => compat_printk, preserve_stack_tail;
        "_dev_err" => compat_printk, preserve_stack_tail;
        "_dev_info" => compat_printk, preserve_stack_tail;
        "_dev_warn" => compat_printk, preserve_stack_tail;
        "__warn_printk" => compat_printk, preserve_stack_tail;
    })
}

pub(crate) fn symbol_abi(name: &str) -> Option<super::LinuxCompatExportAbi> {
    resolve_symbol_meta(name).map(|symbol| symbol.abi)
}

fn is_netdev_data_symbol(name: &str) -> bool {
    name.starts_with("__SCK__tp_func_")
        || name.starts_with("__SCT__tp_func_")
        || name.starts_with("__tracepoint_")
        || matches!(
            name,
            "__cpu_online_mask"
                | "__num_online_cpus"
                | "__preempt_count"
                | "bpf_master_redirect_enabled_key"
                | "bpf_stats_enabled_key"
                | "flow_keys_basic_dissector"
                | "hugetlb_optimize_vmemmap_key"
                | "net_dim_setting"
                | "nr_cpu_ids"
                | "page_offset_base"
                | "phys_base"
                | "softnet_data"
                | "vmemmap_base"
        )
}
