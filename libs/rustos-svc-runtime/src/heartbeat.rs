//! Periodic, exact-incarnation Ring3 -> rootd health control.
//! This is NEVER called by kernel IPC, scheduler, futex, fault, time or TLB
//! mechanisms. Services own the scheduling cadence and must not hold a
//! state mutex across this bounded supervisor RPC.
//! A lost ACK repeats the exact same sequence. Rootd accepts that replay
//! without renewing health or counting a second candidate health pulse.
use core::mem::size_of;
use rustos_component_model::health_wire::{self,Pulse};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest,CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_HEARTBEAT,IPC_SERVICE_ROOTD,
    SYS_RUSTOS_IPC_CALL_BOUNDED,
};
use crate::{ipc,syscall};
const SYS_GETPID:u64=39;
const SYS_GETTID:u64=186;
const EINVAL:i32=22;
const EAGAIN:i32=11;
const EPROTO:i32=71;
pub const INTERVAL_NS:u64=2_000_000_000;
pub const CALL_LIMIT_MS:u64=250;

pub struct Heartbeat {
    service:u64,sequence:u64,next_due_ns:u64,
    failures:u32,last_deadline_ns:u64,
}
impl Heartbeat {
    pub const fn new(service:u64)->Self{
        Self{service,sequence:0,next_due_ns:0,failures:0,last_deadline_ns:0}
    }
    pub fn service(&self)->u64{self.service}
    pub fn failures(&self)->u32{self.failures}
    pub fn last_deadline_ns(&self)->u64{self.last_deadline_ns}
    pub fn next_due_ns(&self)->u64{self.next_due_ns}
    pub fn poll(&mut self,now_ns:u64)->Result<bool,i32>{
        if now_ns==0||self.service==0{return Err(EINVAL)}
        if now_ns<self.next_due_ns{return Ok(false)}
        let seq=self.sequence.checked_add(1).ok_or(75)?;
        // A failed ACK intentionally leaves sequence unchanged: the next
        // attempt must not invent a fresh witness for a lost reply.
        self.next_due_ns=now_ns.saturating_add(INTERVAL_NS);
        match self.send(seq){
            Ok(deadline)=>{
                if deadline<=now_ns {
                    self.failures=self.failures.saturating_add(1);
                    return Err(EPROTO)
                }
                self.sequence=seq;
                self.last_deadline_ns=deadline;
                self.failures=0;
                Ok(true)
            }
            Err(error)=>{
                self.failures=self.failures.saturating_add(1);
                Err(error)
            }
        }
    }
    pub fn send(&self,sequence:u64)->Result<u64,i32>{
        let endpoint=ipc::lookup_service_endpoint(IPC_SERVICE_ROOTD);
        if endpoint<=0{return Err(EAGAIN)}
        let pid=unsafe{syscall::syscall0(SYS_GETPID)};
        let tid=unsafe{syscall::syscall0(SYS_GETTID)};
        if pid<=0||tid<=0{return Err(EINVAL)}
        let pulse=Pulse{
            service:self.service,incarnation_pid:pid as u64,sequence,
        };
        let wire=health_wire::encode_pulse(pulse).map_err(|_|EINVAL)?;
        let mut request=CommercialMaxProtocolRequest::default();
        if wire.len()>request.payload.len(){return Err(EINVAL)}
        request.header.version=COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
        request.header.protocol=COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
        request.header.op=COMMERCIAL_MAX_ROOTD_OP_SERVICE_HEARTBEAT;
        request.header.subject_pid=pid as u64;
        request.header.subject_tid=tid as u64;
        request.payload_len=wire.len() as u32;
        request.payload[..wire.len()].copy_from_slice(&wire);
        let mut response=CommercialMaxProtocolResponse::default();
        let got=unsafe{syscall::syscall6(
            SYS_RUSTOS_IPC_CALL_BOUNDED,endpoint as u64,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            CALL_LIMIT_MS,
        )};
        if got<0{return Err((-got) as i32)}
        if got as usize!=size_of::<CommercialMaxProtocolResponse>()
            ||!response.is_valid_envelope_for(&request)
            ||response.descriptor_count!=0
            ||response.value0!=0
            ||response.value1==0
        {
            return Err(EPROTO)
        }
        if response.status!=0{return Err(response.status)}
        health_wire::decode_accept(
            &response.payload[..response.payload_len as usize],pulse,
        ).map_err(|_|EPROTO)?;
        Ok(response.value1)
    }
}
/// Stack for the dedicated heartbeat thread of a single-receive-loop service.
#[repr(align(16))]
pub struct ThreadStack(pub [u8; THREAD_STACK_BYTES]);
pub const THREAD_STACK_BYTES: usize = 16 * 1024;
impl ThreadStack {
    pub const fn new() -> Self { Self([0; THREAD_STACK_BYTES]) }
}
static THREAD_SERVICE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static THREAD_STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Start the independent heartbeat thread of a freestanding service whose
/// receive loop blocks without a deadline (so an idle but healthy service
/// would otherwise never pulse). The thread shares the address space, owns
/// no service lock and only issues the bounded supervisor RPC; it ends with
/// the process. One thread per process.
#[cfg(all(target_arch = "x86_64", not(test)))]
pub fn spawn_thread(service: u64) -> Result<(), i64> {
    static mut STACK: ThreadStack = ThreadStack::new();
    use core::arch::asm;
    use core::sync::atomic::Ordering;
    const SYS_CLONE: u64 = 56;
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_FS: u64 = 0x0000_0200;
    const CLONE_FILES: u64 = 0x0000_0400;
    const CLONE_SIGHAND: u64 = 0x0000_0800;
    const CLONE_THREAD: u64 = 0x0001_0000;
    if service == 0 { return Err(22); }
    if THREAD_SERVICE.compare_exchange(0, service, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err(16);
    }
    let stack_top = unsafe {
        (core::ptr::addr_of_mut!(STACK) as *mut u8).add(THREAD_STACK_BYTES)
    } as u64;
    let flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            "test rax, rax",
            "jnz 2f",
            "call {entry}",
            "ud2",
            "2:",
            entry = sym heartbeat_thread_entry,
            inlateout("rax") SYS_CLONE as i64 => result,
            in("rdi") flags,
            in("rsi") stack_top,
            in("rdx") 0_u64,
            in("r10") 0_u64,
            in("r8") 0_u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if result <= 0 {
        THREAD_SERVICE.store(0, Ordering::Release);
        Err(if result < 0 { -result } else { 11 })
    } else {
        THREAD_STARTED.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Start the heartbeat thread as soon as the kernel permits one. A bootstrap
/// service can be running before procd (which plans thread creation) is up, in
/// which case `clone` answers ENOSYS; this retries from the service's own
/// receive loop, cheaply (one relaxed load once started, one clone attempt
/// per 32 calls until then), and never blocks or allocates.
#[cfg(all(target_arch = "x86_64", not(test)))]
pub fn ensure_thread(service: u64) {
    use core::sync::atomic::{AtomicU32, Ordering};
    static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
    if THREAD_STARTED.load(Ordering::Relaxed) {
        return;
    }
    if ATTEMPTS.fetch_add(1, Ordering::Relaxed) % 32 != 0 {
        return;
    }
    let _ = spawn_thread(service);
}

#[cfg(all(target_arch = "x86_64", not(test)))]
extern "C" fn heartbeat_thread_entry() -> ! {
    let service = THREAD_SERVICE.load(core::sync::atomic::Ordering::Acquire);
    let mut beat = Heartbeat::new(service);
    loop {
        let _ = beat.poll(crate::syscall::monotonic_nanos());
        crate::syscall::sleep_millis(250);
    }
}

#[cfg(test)]
mod tests{
    #[test]
    fn thread_stack_is_sixteen_byte_aligned(){
        assert_eq!(core::mem::align_of::<ThreadStack>(),16);
        assert_eq!(THREAD_STACK_BYTES%16,0);
    }
    use super::*;
    #[test]
    fn heartbeat_frames_leave_stack_headroom(){
        let frames=size_of::<CommercialMaxProtocolRequest>()+size_of::<CommercialMaxProtocolResponse>();
        assert!(frames+4096<=THREAD_STACK_BYTES,"heartbeat thread stack too small: {frames} bytes of frames");
    }
    #[test]
    fn default_interval_is_slow_and_timeout_below_interval(){
        let h=Heartbeat::new(3);
        assert_eq!(h.sequence,0);
        assert_eq!(h.failures(),0);
        assert!(CALL_LIMIT_MS*1_000_000<INTERVAL_NS);
    }
}
