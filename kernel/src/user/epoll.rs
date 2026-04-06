use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::Mutex;

use crate::user::handles::KernelHandle;

const MAX_EPOLL_INTERESTS: usize = 256;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum EpollError {
    Busy,
    InvalidArgument,
    NotFound,
}

#[derive(Debug, Clone)]
struct EpollInterest {
    fd: u64,
    handle: KernelHandle,
    events: u32,
    data: u64,
}

#[derive(Debug)]
struct EpollState {
    interests: Vec<EpollInterest>,
}

#[derive(Debug, Clone)]
pub(crate) struct EpollHandle {
    inner: Arc<Mutex<EpollState>>,
}

#[derive(Debug, Clone)]
pub(crate) struct EpollInterestSnapshot {
    pub(crate) fd: u64,
    pub(crate) handle: KernelHandle,
    pub(crate) events: u32,
    pub(crate) data: u64,
}

impl EpollHandle {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EpollState {
                interests: Vec::new(),
            })),
        }
    }

    pub(crate) fn path(&self) -> &'static str {
        "anon_inode:[eventpoll]"
    }

    pub(crate) fn add(
        &self,
        fd: u64,
        handle: KernelHandle,
        events: u32,
        data: u64,
    ) -> Result<(), EpollError> {
        let mut state = self.inner.lock();
        if state.interests.iter().any(|interest| interest.fd == fd) {
            return Err(EpollError::Busy);
        }
        if state.interests.len() >= MAX_EPOLL_INTERESTS {
            return Err(EpollError::InvalidArgument);
        }
        state.interests.push(EpollInterest {
            fd,
            handle,
            events,
            data,
        });
        Ok(())
    }

    pub(crate) fn modify(
        &self,
        fd: u64,
        handle: KernelHandle,
        events: u32,
        data: u64,
    ) -> Result<(), EpollError> {
        let mut state = self.inner.lock();
        let Some(interest) = state
            .interests
            .iter_mut()
            .find(|interest| interest.fd == fd)
        else {
            return Err(EpollError::NotFound);
        };
        interest.handle = handle;
        interest.events = events;
        interest.data = data;
        Ok(())
    }

    pub(crate) fn delete(&self, fd: u64) -> Result<(), EpollError> {
        let mut state = self.inner.lock();
        let Some(index) = state
            .interests
            .iter()
            .position(|interest| interest.fd == fd)
        else {
            return Err(EpollError::NotFound);
        };
        state.interests.remove(index);
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Vec<EpollInterestSnapshot> {
        self.inner
            .lock()
            .interests
            .iter()
            .map(|interest| EpollInterestSnapshot {
                fd: interest.fd,
                handle: interest.handle.clone(),
                events: interest.events,
                data: interest.data,
            })
            .collect()
    }
}
