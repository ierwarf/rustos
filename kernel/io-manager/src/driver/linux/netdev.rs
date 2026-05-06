use core::ffi::{c_char, c_void};

static COMPAT_DATA: [usize; 128] = [0; 128];

unsafe extern "C" fn compat_zero() -> usize {
    0
}

unsafe extern "C" fn compat_null() -> *mut c_void {
    core::ptr::null_mut()
}

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
    if dev.is_null() {
        return;
    }
    crate::network::unregister_linux_netdev(dev as usize);
}

unsafe extern "C" fn alloc_etherdev_mqs(sizeof_priv: i32, txqs: u32, rxqs: u32) -> *mut c_void {
    let Some(queue_count) = (txqs as usize).checked_add(rxqs as usize) else {
        return core::ptr::null_mut();
    };
    let Some(size) = core::mem::size_of::<usize>()
        .checked_mul(512)
        .and_then(|base| base.checked_add(sizeof_priv.max(0) as usize))
        .and_then(|base| base.checked_add(queue_count.checked_mul(64)?))
    else {
        return core::ptr::null_mut();
    };
    let dev = unsafe { super::base::__kmalloc_noprof(size.max(4096), 0) };
    if dev.is_null() {
        return core::ptr::null_mut();
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
    if dev.is_null() {
        return;
    }
    crate::network::set_linux_netdev_carrier(dev as usize, true);
}

unsafe extern "C" fn netif_carrier_off(dev: *mut c_void) {
    if dev.is_null() {
        return;
    }
    crate::network::set_linux_netdev_carrier(dev as usize, false);
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
        _ if is_netdev_data_symbol(name) => Some(COMPAT_DATA.as_ptr() as usize),
        _ if is_stubbed_netdev_pointer_symbol(name) => Some(compat_null as *const () as usize),
        _ if is_stubbed_netdev_symbol(name) => Some(compat_zero as *const () as usize),
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

fn is_stubbed_netdev_symbol(name: &str) -> bool {
    name.starts_with("__cpuhp_")
        || name.starts_with("__netif_")
        || name.starts_with("__napi_")
        || name.starts_with("__xdp_")
        || name.starts_with("bpf_")
        || name.starts_with("dev_addr_")
        || name.starts_with("dql_")
        || name.starts_with("eth_")
        || name.starts_with("ethtool_")
        || name.starts_with("net_dim")
        || name.starts_with("net_failover_")
        || name.starts_with("netif_")
        || name.starts_with("napi_")
        || name.starts_with("netdev_")
        || name.starts_with("rtnl_")
        || name.starts_with("synchronize_net")
        || name.starts_with("xdp_")
        || name.starts_with("xp_")
        || name.starts_with("xsk_")
        || matches!(
            name,
            "alloc_cpumask_var_node"
                | "free_cpumask_var"
                | "cpus_read_lock"
                | "cpus_read_unlock"
                | "do_trace_netlink_extack"
                | "gro_receive_skb"
                | "jiffies_to_usecs"
                | "net_ratelimit"
                | "nf_conntrack_destroy"
                | "passthru_features_check"
        )
}

fn is_stubbed_netdev_pointer_symbol(name: &str) -> bool {
    matches!(name, "netdev_rss_key_fill")
}
