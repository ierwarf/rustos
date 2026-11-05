//! Filesystem-independent owned range plans and bounded positional transfers.
//!
//! Ownership: a backend mints file identities and validates their generations;
//! this module owns only immutable mappings and transfer progress, never paths,
//! mount policy, physical authority, cache policy, or filesystem metadata.
//! Lifecycle: build/validate a map before use; a backend must reject stale maps.
//! Concurrency: no borrowed filesystem cursor escapes into an owned map. Source
//! and sink retain their capabilities until transfer completion/cancellation.
//! Failure: overflow, allocation pressure, missing coverage and zero/overlong
//! progress fail explicitly; an incomplete transfer is never a complete image.
//! Evidence: native range, partial-I/O, cancellation and >4-GiB offset tests.

use alloc::vec::Vec;
use crate::{BlockDevice, StorageError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub volume: u64,
    pub object: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileCapabilities {
    pub max_file_bytes: u64,
    pub writable: bool,
    pub sparse: bool,
}

pub trait ReadAt {
    type Error;
    fn identity(&self) -> FileIdentity;
    fn len(&self) -> u64;
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, Self::Error>;
}

pub trait WriteAt {
    type Error;
    fn write_at(&mut self, offset: u64, input: &[u8]) -> Result<usize, Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeKind {
    Data { device_offset: u64 },
    /// Both a hole and an allocated-but-unwritten range read as zero. Their
    /// allocation/commit semantics remain distinct in the filesystem backend.
    Hole,
    Unwritten,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileRange {
    pub offset: u64,
    pub len: u64,
    pub kind: RangeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeError {
    Invalid,
    Stale,
    Budget,
    NoMemory,
    Io(StorageError),
}

/// An optional read accelerator, not a required on-disk extent format. Future
/// backends can implement ReadAt directly for compressed/inline/dynamic data.
#[derive(Debug)]
pub struct FileReadMap {
    identity: FileIdentity,
    len: u64,
    device_bytes: u64,
    ranges: Vec<FileRange>,
    covered: u64,
    max_ranges: usize,
}

impl FileReadMap {
    pub fn new(identity: FileIdentity, len: u64, device_bytes: u64, metadata_budget: usize) -> Result<Self, RangeError> {
        if identity.volume == 0 || identity.object == 0 || identity.generation == 0 {
            return Err(RangeError::Invalid);
        }
        Ok(Self { identity, len, device_bytes, ranges: Vec::new(), covered: 0,
            max_ranges: metadata_budget / core::mem::size_of::<FileRange>() })
    }

    pub fn identity(&self) -> FileIdentity { self.identity }
    pub fn len(&self) -> u64 { self.len }
    pub fn metadata_bytes(&self) -> usize { self.ranges.capacity() * core::mem::size_of::<FileRange>() }
    pub fn range_count(&self) -> usize { self.ranges.len() }
    pub fn ranges(&self) -> &[FileRange] { &self.ranges }

    pub fn push(&mut self, len: u64, kind: RangeKind) -> Result<(), RangeError> {
        let end = self.covered.checked_add(len).ok_or(RangeError::Invalid)?;
        if len == 0 || end > self.len { return Err(RangeError::Invalid); }
        if let RangeKind::Data { device_offset } = kind {
            if device_offset.checked_add(len).is_none_or(|end| end > self.device_bytes) {
                return Err(RangeError::Invalid);
            }
        }
        if let Some(last) = self.ranges.last_mut() {
            let adjacent = match (last.kind, kind) {
                (RangeKind::Data { device_offset: a }, RangeKind::Data { device_offset: b }) => a.checked_add(last.len) == Some(b),
                (RangeKind::Hole, RangeKind::Hole) | (RangeKind::Unwritten, RangeKind::Unwritten) => true,
                _ => false,
            };
            if adjacent {
                last.len = last.len.checked_add(len).ok_or(RangeError::Invalid)?;
                self.covered = end;
                return Ok(());
            }
        }
        if self.ranges.len() >= self.max_ranges { return Err(RangeError::Budget); }
        // Geometric growth remains inside explicit admission; reserving one
        // entry at a time would turn fragmented-map construction quadratic.
        if self.ranges.len() == self.ranges.capacity() {
            let target = self.ranges.capacity().saturating_mul(2).max(8).min(self.max_ranges);
            self.ranges.try_reserve_exact(target - self.ranges.len()).map_err(|_| RangeError::NoMemory)?;
        }
        self.ranges.push(FileRange { offset: self.covered, len, kind });
        self.covered = end;
        Ok(())
    }

    pub fn finish(self) -> Result<Self, RangeError> {
        if self.covered != self.len { return Err(RangeError::Invalid); }
        Ok(self)
    }

    pub fn read_at<D: BlockDevice>(&self, dev: &mut D, offset: u64, output: &mut [u8], scratch: &mut [u8]) -> Result<usize, RangeError> {
        if self.covered != self.len { return Err(RangeError::Invalid); }
        if output.is_empty() || offset >= self.len { return Ok(0); }
        let block = dev.logical_block_size();
        if block == 0 || scratch.len() < block || dev.block_count().checked_mul(block as u64) != Some(self.device_bytes) {
            return Err(RangeError::Stale);
        }
        let count = output.len().min(usize::try_from((self.len - offset).min(usize::MAX as u64)).map_err(|_| RangeError::Invalid)?);
        let mut done = 0;
        // Binary search supports random reads; sequential data never traverses
        // a FAT chain here. Each subsequent range is visited just once.
        let mut index = self.ranges.partition_point(|range| range.offset <= offset).saturating_sub(1);
        while done < count {
            let position = offset + done as u64;
            let range = self.ranges.get(index).ok_or(RangeError::Invalid)?;
            let delta = position.checked_sub(range.offset).ok_or(RangeError::Invalid)?;
            let remaining = range.len.checked_sub(delta).ok_or(RangeError::Invalid)?;
            let take = (count - done).min(usize::try_from(remaining.min(usize::MAX as u64)).map_err(|_| RangeError::Invalid)?);
            if take == 0 { return Err(RangeError::Invalid); }
            match range.kind {
                RangeKind::Hole | RangeKind::Unwritten => output[done..done + take].fill(0),
                RangeKind::Data { device_offset } => {
                    let mut source = device_offset.checked_add(delta).ok_or(RangeError::Invalid)?;
                    let mut copied = 0;
                    while copied < take {
                        let lba = source / block as u64;
                        let in_block = (source % block as u64) as usize;
                        let left = take - copied;
                        let n = if in_block == 0 && left >= block {
                            let n = left / block * block;
                            dev.read_blocks(lba, &mut output[done + copied..done + copied + n]).map_err(RangeError::Io)?;
                            n
                        } else {
                            dev.read_blocks(lba, &mut scratch[..block]).map_err(RangeError::Io)?;
                            let n = left.min(block - in_block);
                            output[done + copied..done + copied + n].copy_from_slice(&scratch[in_block..in_block + n]);
                            n
                        };
                        source = source.checked_add(n as u64).ok_or(RangeError::Invalid)?;
                        copied += n;
                    }
                }
            }
            done += take;
            index += 1;
        }
        Ok(done)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferControl { Cancelled, Expired }

#[derive(Debug, Eq, PartialEq)]
pub enum TransferError<R, W> {
    Read(R), Write(W), Control(TransferControl), InvalidWindow,
    SourceChanged, UnexpectedEof, WriteZero, InvalidCount,
}

/// Confirmed prefix from earlier successful provider calls. A provider error
/// may additionally describe an unknown outcome; this is not a claim that its
/// failing write stored nothing and is not permission to replay a mutation.
#[derive(Debug, Eq, PartialEq)]
pub struct TransferFailure<R, W> {
    pub completed_bytes: u64,
    pub error: TransferError<R, W>,
}
fn failure<R, W>(completed_bytes: u64, error: TransferError<R, W>) -> TransferFailure<R, W> {
    TransferFailure { completed_bytes, error }
}

/// Transfer an exact immutable length, using only the caller's scratch window.
/// `control` runs before every provider operation; `committed` sees only bytes
/// actually accepted by the sink (suitable for an incremental content digest).
pub fn transfer_exact<R: ReadAt, W: WriteAt>(
    source: &mut R, sink: &mut W, window: &mut [u8],
    mut control: impl FnMut(u64) -> Result<(), TransferControl>,
    mut committed: impl FnMut(&[u8]),
) -> Result<u64, TransferFailure<R::Error, W::Error>> {
    if window.is_empty() { return Err(failure(0, TransferError::InvalidWindow)); }
    let identity = source.identity();
    let len = source.len();
    let mut done = 0u64;
    while done < len {
        control(done).map_err(|error| failure(done, TransferError::Control(error)))?;
        if source.identity() != identity || source.len() != len { return Err(failure(done, TransferError::SourceChanged)); }
        let requested = window.len().min((len - done).min(usize::MAX as u64) as usize);
        let read = source.read_at(done, &mut window[..requested]).map_err(|error| failure(done, TransferError::Read(error)))?;
        if read == 0 { return Err(failure(done, TransferError::UnexpectedEof)); }
        if read > requested { return Err(failure(done, TransferError::InvalidCount)); }
        if source.identity() != identity || source.len() != len { return Err(failure(done, TransferError::SourceChanged)); }
        let mut sent = 0;
        while sent < read {
            let offset = done + sent as u64;
            control(offset).map_err(|error| failure(offset, TransferError::Control(error)))?;
            let n = sink.write_at(offset, &window[sent..read]).map_err(|error| failure(offset, TransferError::Write(error)))?;
            if n == 0 { return Err(failure(offset, TransferError::WriteZero)); }
            if n > read - sent { return Err(failure(offset, TransferError::InvalidCount)); }
            committed(&window[sent..sent + n]);
            sent += n;
        }
        // `read <= requested <= len - done`, so this cannot wrap.
        done += read as u64;
    }
    if source.identity() != identity || source.len() != len { return Err(failure(done, TransferError::SourceChanged)); }
    control(done).map_err(|error| failure(done, TransferError::Control(error)))?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemBlockDevice;
    const ID: FileIdentity = FileIdentity { volume: 1, object: 2, generation: 3 };

    #[test]
    fn range_reads_handle_fragments_holes_unwritten_and_eof() {
        let mut dev = MemBlockDevice::new_zeroed(512, 16);
        let bytes = [0x42; 512];
        dev.write_blocks(2, &bytes).unwrap();
        dev.write_blocks(7, &[0x73; 512]).unwrap();
        let mut map = FileReadMap::new(ID, 1536, 8192, 4096).unwrap();
        map.push(400, RangeKind::Data { device_offset: 1024 }).unwrap();
        map.push(200, RangeKind::Hole).unwrap();
        map.push(424, RangeKind::Unwritten).unwrap();
        map.push(512, RangeKind::Data { device_offset: 3584 }).unwrap();
        let map = map.finish().unwrap();
        let mut out = [0xa5; 1600];
        let mut scratch = [0; 512];
        assert_eq!(map.read_at(&mut dev, 399, &mut out, &mut scratch).unwrap(), 1137);
        assert_eq!(out[0], 0x42);
        assert!(out[1..625].iter().all(|x| *x == 0));
        assert!(out[625..1137].iter().all(|x| *x == 0x73));
        assert_eq!(out[1137], 0xa5);
        assert_eq!(map.read_at(&mut dev, u64::MAX, &mut out, &mut scratch).unwrap(), 0);
    }

    #[test]
    fn maps_coalesce_and_reject_overflow_missing_coverage_and_budget() {
        let mut map = FileReadMap::new(ID, 1024, 8192, 32).unwrap();
        map.push(512, RangeKind::Data { device_offset: 0 }).unwrap();
        map.push(512, RangeKind::Data { device_offset: 512 }).unwrap();
        assert_eq!(map.finish().unwrap().range_count(), 1);
        let mut map = FileReadMap::new(ID, 1024, 8192, 32).unwrap();
        map.push(512, RangeKind::Hole).unwrap();
        assert_eq!(map.push(512, RangeKind::Data { device_offset: 0 }), Err(RangeError::Budget));
        assert!(map.finish().is_err());
        let mut map = FileReadMap::new(ID, 1, u64::MAX, 4096).unwrap();
        assert_eq!(map.push(1, RangeKind::Data { device_offset: u64::MAX }), Err(RangeError::Invalid));
    }

    struct Source { len: u64, maximum: usize, eof: bool }
    impl ReadAt for Source {
        type Error = ();
        fn identity(&self) -> FileIdentity { ID }
        fn len(&self) -> u64 { self.len }
        fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<usize, ()> {
            assert!(offset < self.len);
            if self.eof { return Ok(0); }
            Ok(bytes.len().min(self.maximum))
        }
    }
    struct Sink { done: u64, maximum: usize }
    impl WriteAt for Sink {
        type Error = ();
        fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, ()> {
            assert_eq!(offset, self.done);
            let n = bytes.len().min(self.maximum);
            self.done += n as u64;
            Ok(n)
        }
    }

    #[test]
    fn transfer_handles_partial_progress_and_offsets_beyond_four_gib() {
        let len = (1u64 << 32) + 71;
        let mut source = Source { len, maximum: usize::MAX, eof: false };
        let mut sink = Sink { done: 0, maximum: usize::MAX };
        let mut window = alloc::vec![0; 1024 * 1024];
        let mut counted = 0u64;
        assert_eq!(transfer_exact(&mut source, &mut sink, &mut window, |_| Ok(()), |bytes| counted += bytes.len() as u64), Ok(len));
        assert_eq!(counted, len);
        let mut source = Source { len: 307, maximum: 17, eof: false };
        let mut sink = Sink { done: 0, maximum: 3 };
        assert_eq!(transfer_exact(&mut source, &mut sink, &mut window[..64], |_| Ok(()), |_| {}), Ok(307));
    }

    #[test]
    fn failed_writes_preserve_confirmed_prefix_and_provider_outcome() {
        struct FailingSink { done: u64 }
        impl WriteAt for FailingSink {
            type Error = &'static str;
            fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, Self::Error> {
                assert_eq!(offset, self.done);
                if self.done == 19 { return Err("provider outcome unknown"); }
                let n = bytes.len().min(3).min((19 - self.done) as usize);
                self.done += n as u64;
                Ok(n)
            }
        }
        let mut source = Source { len: 100, maximum: 64, eof: false };
        let mut sink = FailingSink { done: 0 };
        let mut digest_bytes = 0;
        let result = transfer_exact(&mut source, &mut sink, &mut [0; 64], |_| Ok(()), |bytes| digest_bytes += bytes.len());
        assert_eq!(result, Err(failure(19, TransferError::Write("provider outcome unknown"))));
        assert_eq!(digest_bytes, 19);
    }
    #[test]
    fn source_version_change_during_read_aborts_before_writing_that_window() {
        struct ChangingSource { generation: u64 }
        impl ReadAt for ChangingSource {
            type Error = ();
            fn identity(&self) -> FileIdentity { FileIdentity { generation: self.generation, ..ID } }
            fn len(&self) -> u64 { 64 }
            fn read_at(&mut self, _: u64, output: &mut [u8]) -> Result<usize, ()> { self.generation += 1; Ok(output.len()) }
        }
        let mut source = ChangingSource { generation: 1 };
        let mut sink = Sink { done: 0, maximum: 64 };
        assert_eq!(transfer_exact(&mut source, &mut sink, &mut [0; 64], |_| Ok(()), |_| {}),
            Err(failure(0, TransferError::SourceChanged)));
        assert_eq!(sink.done, 0);
    }
    #[test]
    fn transfer_rejects_eof_zero_writes_and_cancellation() {
        let mut source = Source { len: 1024, maximum: 64, eof: true };
        let mut sink = Sink { done: 0, maximum: 1 };
        let mut window = [0; 64];
        assert_eq!(transfer_exact(&mut source, &mut sink, &mut window, |_| Ok(()), |_| {}), Err(failure(0, TransferError::UnexpectedEof)));
        source.eof = false;
        sink.maximum = 0;
        assert_eq!(transfer_exact(&mut source, &mut sink, &mut window, |_| Ok(()), |_| {}), Err(failure(0, TransferError::WriteZero)));
        sink.maximum = 64;
        let error = transfer_exact(&mut source, &mut sink, &mut window,
            |done| if done >= 64 { Err(TransferControl::Cancelled) } else { Ok(()) }, |_| {});
        assert_eq!(error, Err(failure(64, TransferError::Control(TransferControl::Cancelled))));
        assert_eq!(sink.done, 64);
    }
}
