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
    crate::debug::info!(
        driver,
        "linux compat: netdev registered dev={:#x}",
        dev as usize
    );
    crate::network::note_netdev_registered();
    0
}

unsafe extern "C" fn unregister_netdev(dev: *mut c_void) {
    crate::debug::info!(
        driver,
        "linux compat: netdev unregistered dev={:#x}",
        dev as usize
    );
}

unsafe extern "C" fn alloc_etherdev_mqs(sizeof_priv: i32, txqs: u32, rxqs: u32) -> *mut c_void {
    let size = core::mem::size_of::<usize>()
        .saturating_mul(512)
        .saturating_add(sizeof_priv.max(0) as usize)
        .saturating_add((txqs as usize + rxqs as usize).saturating_mul(64));
    unsafe { super::base::__kmalloc_noprof(size.max(4096), 0) }
}

unsafe extern "C" fn free_netdev(dev: *mut c_void) {
    unsafe { super::base::kfree(dev) };
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "alloc_etherdev_mqs" => Some(alloc_etherdev_mqs as *const () as usize),
        "free_netdev" => Some(free_netdev as *const () as usize),
        "register_netdevice" => Some(register_netdevice as *const () as usize),
        "unregister_netdev" => Some(unregister_netdev as *const () as usize),
        "unregister_netdevice_queue" => Some(unregister_netdev as *const () as usize),
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
