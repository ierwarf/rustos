use core::mem::{offset_of, size_of};

use rustos_image_admission::{
    PE64_DOS_HEADER_SIZE, PE64_FILE_HEADER_SIZE, PE64_FILE_RELOCS_STRIPPED,
    PE64_IMPORT_DESCRIPTOR_BYTES, PE64_IMPORT_THUNK_BYTES, PE64_MACHINE_AMD64, PE64_OPTIONAL_MAGIC,
    PE64_RELOC_ABSOLUTE, PE64_RELOC_DIR64, PE64_SCN_EXECUTE, PE64_SCN_READ, PE64_SCN_WRITE,
    PE64_SECTION_HEADER_SIZE,
};
use rustos_user_abi::{linux, windows};

fn pair(name: &str, value: impl core::fmt::Display) {
    println!("{name}={value}");
}

fn linux_probe() {
    pair("af_unix", linux::AF_UNIX);
    pair("epoll_cloexec", linux::EPOLL_CLOEXEC);
    pair("epoll_ctl_add", linux::EPOLL_CTL_ADD);
    pair("epoll_ctl_del", linux::EPOLL_CTL_DEL);
    pair("epoll_ctl_mod", linux::EPOLL_CTL_MOD);
    pair("epollerr", linux::EPOLLERR);
    pair("epollet", 1_u32 << 31);
    pair("epollhup", linux::EPOLLHUP);
    pair("epollin", linux::EPOLLIN);
    pair("epollout", linux::EPOLLOUT);
    pair("f_dupfd_cloexec", linux::F_DUPFD_CLOEXEC);
    pair("map_anonymous", linux::MAP_ANONYMOUS);
    pair("map_fixed", linux::MAP_FIXED);
    pair("map_private", linux::MAP_PRIVATE);
    pair("msg_cmsg_cloexec", linux::MSG_CMSG_CLOEXEC);
    pair("msg_dontwait", linux::MSG_DONTWAIT);
    pair("o_cloexec", linux::O_CLOEXEC);
    pair("o_nonblock", linux::O_NONBLOCK);
    pair(
        "offset_epoll_event_data",
        offset_of!(linux::LinuxEpollEvent, data),
    );
    pair(
        "offset_msghdr_control",
        offset_of!(linux::LinuxMsghdr, msg_control),
    );
    pair("pollerr", linux::POLLERR);
    pair("pollhup", linux::POLLHUP);
    pair("pollin", linux::POLLIN);
    pair("pollout", linux::POLLOUT);
    pair("prot_exec", linux::PROT_EXEC);
    pair("prot_read", linux::PROT_READ);
    pair("prot_write", linux::PROT_WRITE);
    pair("scm_rights", linux::SCM_RIGHTS);
    pair("size_cmsghdr", size_of::<linux::LinuxCmsghdr>());
    pair("size_epoll_event", size_of::<linux::LinuxEpollEvent>());
    pair("size_iovec", size_of::<linux::LinuxIovec>());
    pair("size_msghdr", size_of::<linux::LinuxMsghdr>());
    pair("size_pollfd", size_of::<linux::LinuxPollFd>());
    pair("size_sockaddr_un", size_of::<linux::LinuxSockaddrUn>());
    pair("size_stat", size_of::<linux::LinuxStat>());
    pair("size_statx", size_of::<linux::LinuxStatx>());
    pair("size_timespec", size_of::<linux::LinuxTimespec>());
    pair("sock_cloexec", linux::SOCK_CLOEXEC);
    pair("sock_nonblock", linux::SOCK_NONBLOCK);
    pair("sol_socket", linux::SOL_SOCKET);
    pair("sys_epoll_create1", linux::SYS_EPOLL_CREATE1);
    pair("sys_epoll_ctl", linux::SYS_EPOLL_CTL);
    pair("sys_epoll_wait", linux::SYS_EPOLL_WAIT);
    pair("sys_mmap", linux::SYS_MMAP);
    pair("sys_recvmsg", linux::SYS_RECVMSG);
    pair("sys_sendmsg", linux::SYS_SENDMSG);
    pair("sys_socketpair", linux::SYS_SOCKETPAIR);
}

fn windows_probe() {
    pair("bool_false", windows::BOOL_FALSE);
    pair("error_invalid_function", windows::ERROR_INVALID_FUNCTION);
    pair("error_invalid_handle", windows::ERROR_INVALID_HANDLE);
    pair("error_invalid_parameter", windows::ERROR_INVALID_PARAMETER);
    pair("image_file_dll", 0x2000_u16);
    pair("image_file_machine_amd64", PE64_MACHINE_AMD64);
    pair("image_file_relocs_stripped", PE64_FILE_RELOCS_STRIPPED);
    pair("image_nt_optional_hdr64_magic", PE64_OPTIONAL_MAGIC);
    pair("image_rel_based_absolute", PE64_RELOC_ABSOLUTE);
    pair("image_rel_based_dir64", PE64_RELOC_DIR64);
    pair("image_scn_mem_execute", PE64_SCN_EXECUTE);
    pair("image_scn_mem_read", PE64_SCN_READ);
    pair("image_scn_mem_write", PE64_SCN_WRITE);
    pair("mem_commit", windows::MEM_COMMIT);
    pair("mem_release", windows::MEM_RELEASE);
    pair("mem_reserve", windows::MEM_RESERVE);
    pair("page_execute_read", windows::PAGE_EXECUTE_READ);
    pair("page_execute_readwrite", windows::PAGE_EXECUTE_READWRITE);
    pair("page_noaccess", windows::PAGE_NOACCESS);
    pair("page_readonly", windows::PAGE_READONLY);
    pair("page_readwrite", windows::PAGE_READWRITE);
    pair("page_size", windows::PAGE_SIZE);
    pair("size_image_base_relocation", 8);
    pair("size_image_dos_header", PE64_DOS_HEADER_SIZE);
    pair("size_image_import_descriptor", PE64_IMPORT_DESCRIPTOR_BYTES);
    pair("size_image_nt_headers64", PE64_FILE_HEADER_SIZE + 240);
    pair("size_image_optional_header64", 240);
    pair("size_image_section_header", PE64_SECTION_HEADER_SIZE);
    pair("size_image_thunk_data64", PE64_IMPORT_THUNK_BYTES);
    pair("status_invalid_handle", windows::STATUS_INVALID_HANDLE);
    pair(
        "status_invalid_parameter",
        windows::STATUS_INVALID_PARAMETER,
    );
    pair(
        "status_invalid_system_service",
        windows::STATUS_INVALID_SYSTEM_SERVICE,
    );
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("linux") => linux_probe(),
        Some("windows") => windows_probe(),
        _ => {
            eprintln!("usage: abi_contract_probe <linux|windows>");
            std::process::exit(2);
        }
    }
}
