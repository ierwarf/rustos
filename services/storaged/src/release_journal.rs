//! Storaged-owned, append-only stable-file journal backend for releases.
//!
//! IMPORTANT: A successful fsync is only as durable as the mounted medium,
//! its storage driver and power-loss guarantees. The caller must externally
//! establish an authenticated, dedicated persistent mount; a path under an
//! ephemeral rootfs or read-only guest image is NOT a durable release journal.
//! This module does not select a path, grant a partition, or accept an IPC
//! requester merely because it knows a filename.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, MetadataExt};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use rustos_component_model::durable_recovery::{self,Cursor,Record};

const RECORD_BYTES:usize=96;
const MAX_RECORDS:u64=1_048_576;
const HEADER_BYTES:usize=64;
const HEADER_MAGIC:[u8;8]=*b"RSTRJNL1";
const HEADER_VERSION:u16=1;
const ERROR_EINVAL:i32=22;
const ERROR_EIO:i32=5;
const ERROR_EPERM:i32=1;
const ERROR_ENOSPC:i32=28;
const ERROR_EPROTO:i32=71;

/// Rootd owns one release transaction; storaged must reject a second
/// filesystem writer before examining any record. Advisory locks are not
/// a security boundary against a compromised privileged writer: the mount
/// itself and file ACL remain external trust anchors.
fn lock_exclusive(file:&File)->Result<(),i32>{
    let result=unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)};
    if result!=0{return Err(std::io::Error::last_os_error()
        .raw_os_error().unwrap_or(ERROR_EIO))}
    Ok(())
}
fn status(error:std::io::Error)->i32{
    error.raw_os_error().unwrap_or(ERROR_EIO)
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub(super) struct JournalIdentity{
    pub device_id:u64,
    pub namespace_generation:u64,
    pub volume_generation:u64,
    pub rootd_owner:u64,
}
impl JournalIdentity{
    pub fn valid(self)->bool{
        self.device_id!=0&&self.namespace_generation!=0
            &&self.volume_generation!=0&&self.rootd_owner!=0
    }
}
fn header(id:JournalIdentity)->[u8;HEADER_BYTES]{
    let mut out=[0;HEADER_BYTES];
    out[0..8].copy_from_slice(&HEADER_MAGIC);
    out[8..10].copy_from_slice(&HEADER_VERSION.to_le_bytes());
    out[16..24].copy_from_slice(&id.device_id.to_le_bytes());
    out[24..32].copy_from_slice(&id.namespace_generation.to_le_bytes());
    out[32..40].copy_from_slice(&id.volume_generation.to_le_bytes());
    out[40..48].copy_from_slice(&id.rootd_owner.to_le_bytes());
    out
}
fn exact_header(value:&[u8;HEADER_BYTES],id:JournalIdentity)->bool{
    value==&header(id)
}
/// Reboot-time file reopening is not the same as trustworthy replay.
/// Verify the entire append-only prefix without loading it into RAM; a torn,
/// re-ordered, skipped or malformed transaction prevents journal activation.
/// This is a pure local media check, not authentication of the storage device.
fn scan_canonical_records(file:&mut File,records:u64)->Result<Cursor,i32>{
    file.seek(SeekFrom::Start(HEADER_BYTES as u64)).map_err(status)?;
    let mut cursor=Cursor::new();
    for index in 0..records {
        let mut bytes=[0u8;RECORD_BYTES];
        file.read_exact(&mut bytes).map_err(status)?;
        let record=durable_recovery::decode(&bytes).map_err(|_|ERROR_EPROTO)?;
        if record.sequence!=index+1{return Err(ERROR_EPROTO)}
        cursor=cursor.admit(record).map_err(|_|ERROR_EPROTO)?;
    }
    Ok(cursor)
}
/// The backing location is opened through an already established trusted
/// mount. Never accept an arbitrary client-controlled path as authority.
pub(super) struct ReleaseJournalFile{
    file:File,
    path:PathBuf,
    id:JournalIdentity,
    records:u64,
    fail_closed:bool,
    /// Replayed transaction position. No media ACK is trusted without the
    /// corresponding canonical sequence transition.
    cursor:Cursor,
}
impl ReleaseJournalFile{
    pub(super) fn open_existing(
        path:&Path,id:JournalIdentity,authenticated_mount:bool,
    )->Result<Self,i32>{
        if !id.valid()||!authenticated_mount{return Err(ERROR_EPERM)}
        let mut opts=OpenOptions::new();
        opts.read(true).write(true).custom_flags(
            libc::O_CLOEXEC|libc::O_NOFOLLOW,
        );
        let mut file=opts.open(path).map_err(status)?;
        lock_exclusive(&file)?;
        let meta=file.metadata().map_err(status)?;
        if !meta.is_file()||meta.len()<HEADER_BYTES as u64
            ||(meta.len()-HEADER_BYTES as u64)%RECORD_BYTES as u64!=0 {
            return Err(ERROR_EPROTO)
        }
        let mut raw=[0u8;HEADER_BYTES];
        file.read_exact(&mut raw).map_err(status)?;
        if !exact_header(&raw,id){return Err(ERROR_EPERM)}
        let count=(meta.len()-HEADER_BYTES as u64)/RECORD_BYTES as u64;
        if count>MAX_RECORDS{return Err(ERROR_ENOSPC)}
        let cursor=scan_canonical_records(&mut file,count)?;
        Ok(Self{file,path:path.to_owned(),id,records:count,
            fail_closed:false,cursor})
    }
    /// Crash-recovery open. An append is acknowledged only after write, sync
    /// and readback, and the exclusive writer lock allows at most one append
    /// in flight, so an acknowledged record is never part of the damaged
    /// tail. A tail of at most one record after the longest canonical prefix
    /// (a partial record, or one complete record that fails decode/sequence/
    /// order) is therefore an unacknowledged torn append: it is truncated and
    /// synced. Anything larger, or damage that is not confined to the tail,
    /// stays fail-closed. The caller receives the discarded byte count and
    /// must treat a nonzero count as possible loss of an acknowledged record
    /// (media rot is indistinguishable from a torn tail) and so as
    /// rollback-required evidence, never as success.
    pub(super) fn open_recovering(
        path:&Path,id:JournalIdentity,authenticated_mount:bool,
    )->Result<(Self,u64),i32>{
        if !id.valid()||!authenticated_mount{return Err(ERROR_EPERM)}
        let mut opts=OpenOptions::new();
        opts.read(true).write(true).custom_flags(
            libc::O_CLOEXEC|libc::O_NOFOLLOW,
        );
        let mut file=opts.open(path).map_err(status)?;
        lock_exclusive(&file)?;
        let meta=file.metadata().map_err(status)?;
        if !meta.is_file()||meta.len()<HEADER_BYTES as u64{
            return Err(ERROR_EPROTO)
        }
        let mut raw=[0u8;HEADER_BYTES];
        file.read_exact(&mut raw).map_err(status)?;
        if !exact_header(&raw,id){return Err(ERROR_EPERM)}
        let body=meta.len()-HEADER_BYTES as u64;
        let complete=body/RECORD_BYTES as u64;
        if complete>MAX_RECORDS+1{return Err(ERROR_ENOSPC)}
        let mut cursor=Cursor::new();
        let mut valid=0u64;
        while valid<complete {
            let mut bytes=[0u8;RECORD_BYTES];
            file.read_exact(&mut bytes).map_err(status)?;
            let next=durable_recovery::decode(&bytes).ok()
                .filter(|record|record.sequence==valid+1)
                .and_then(|record|cursor.admit(record).ok());
            let Some(next)=next else {break};
            cursor=next;
            valid+=1;
        }
        if valid>MAX_RECORDS{return Err(ERROR_ENOSPC)}
        let keep=HEADER_BYTES as u64+valid*RECORD_BYTES as u64;
        let discarded=meta.len()-keep;
        if discarded>RECORD_BYTES as u64{return Err(ERROR_EPROTO)}
        if discarded!=0 {
            file.set_len(keep).map_err(status)?;
            file.sync_all().map_err(status)?;
        }
        Ok((Self{file,path:path.to_owned(),id,records:valid,
            fail_closed:false,cursor},discarded))
    }
    /// Creating a new file is an explicit privileged provisioning action,
    /// NOT an implicit recovery fallback. If open_existing fails on boot,
    /// never silently call this function to discard prior acknowledgments.
    pub(super) fn provision_new(
        path:&Path,id:JournalIdentity,authenticated_mount:bool,
    )->Result<Self,i32>{
        if !id.valid()||!authenticated_mount{return Err(ERROR_EPERM)}
        let mut opts=OpenOptions::new();
        opts.read(true).write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW);
        let mut file=opts.open(path).map_err(status)?;
        lock_exclusive(&file)?;
        let header=header(id);
        file.write_all(&header).map_err(status)?;
        file.sync_all().map_err(status)?;
        // Directory fsync ensures the name and inode survive a crash on
        // supporting filesystems. It is not a substitute for media honesty.
        let parent=path.parent().ok_or(ERROR_EINVAL)?;
        File::open(parent).map_err(status)?.sync_all().map_err(status)?;
        Ok(Self{file,path:path.to_owned(),id,records:0,
            fail_closed:false,cursor:Cursor::new()})
    }
    pub(super) fn id(&self)->JournalIdentity{self.id}
    pub(super) fn records(&self)->u64{self.records}
    pub(super) fn recovered_cursor(&self)->Cursor{self.cursor}
    pub(super) fn failed(&self)->bool{self.fail_closed}
    fn check_file_shape(&mut self)->Result<(),i32>{
        if self.fail_closed{return Err(ERROR_EIO)}
        let meta=match self.file.metadata(){
            Ok(meta)=>meta,
            Err(error)=>{self.fail_closed=true;return Err(status(error))}
        };
        let expected=(HEADER_BYTES as u64)
            .checked_add(self.records.checked_mul(RECORD_BYTES as u64)
                .ok_or(ERROR_ENOSPC)?).ok_or(ERROR_ENOSPC)?;
        if !meta.is_file()||meta.len()!=expected {
            self.fail_closed=true;
            return Err(ERROR_EPROTO)
        }
        Ok(())
    }
    pub(super) fn read_record(&mut self,index:u64)
        ->Result<Option<[u8;RECORD_BYTES]>,i32>{
        self.check_file_shape()?;
        if index>=self.records{return Ok(None)}
        let offset=(HEADER_BYTES as u64)
            .checked_add(index.checked_mul(RECORD_BYTES as u64)
                .ok_or(ERROR_ENOSPC)?).ok_or(ERROR_ENOSPC)?;
        if self.file.seek(SeekFrom::Start(offset)).map_err(status)?!=offset {
            self.fail_closed=true;return Err(ERROR_EIO)
        }
        let mut result=[0u8;RECORD_BYTES];
        if let Err(error)=self.file.read_exact(&mut result){
            self.fail_closed=true;return Err(status(error))
        }
        let admitted=durable_recovery::decode(&result).map_err(|_|{
            self.fail_closed=true;ERROR_EPROTO
        })?;
        if admitted.sequence!=index.checked_add(1).ok_or(ERROR_ENOSPC)?{
            self.fail_closed=true;return Err(ERROR_EPROTO)
        }
        Ok(Some(result))
    }
    pub(super) fn append_and_verify(
        &mut self,index:u64,record:[u8;RECORD_BYTES],
        authenticated_owner:JournalIdentity,
    )->Result<(),i32>{
        if self.fail_closed||authenticated_owner!=self.id{
            return Err(ERROR_EPERM)
        }
        if index!=self.records||index>=MAX_RECORDS{return Err(ERROR_ENOSPC)}
        self.check_file_shape()?;
        let next_record=durable_recovery::decode(&record)
            .map_err(|_|ERROR_EPROTO)?;
        let next_cursor=self.cursor.admit(next_record)
            .map_err(|_|ERROR_EPROTO)?;
        if next_record.sequence!=index+1{return Err(ERROR_EPROTO)}
        let offset=(HEADER_BYTES as u64)
            .checked_add(index.checked_mul(RECORD_BYTES as u64)
                .ok_or(ERROR_ENOSPC)?).ok_or(ERROR_ENOSPC)?;
        if self.file.seek(SeekFrom::End(0)).map_err(status)?!=offset{
            self.fail_closed=true;return Err(ERROR_EPROTO)
        }
        if let Err(error)=self.file.write_all(&record){
            self.fail_closed=true;return Err(status(error))
        }
        if let Err(error)=self.file.sync_all(){
            self.fail_closed=true;return Err(status(error))
        }
        if let Err(error)=self.file.seek(SeekFrom::Start(offset)){
            self.fail_closed=true;return Err(status(error))
        }
        let mut observed=[0u8;RECORD_BYTES];
        if let Err(error)=self.file.read_exact(&mut observed){
            self.fail_closed=true;return Err(status(error))
        }
        if observed!=record {
            self.fail_closed=true;return Err(ERROR_EPROTO)
        }
        self.records+=1;
        self.cursor=next_cursor;
        self.check_file_shape()?;
        Ok(())
    }
    /// Never truncate a replayed torn tail without independent transaction
    /// custody and recovery evidence. The caller cannot bypass fail_closed.
    pub(super) fn path(&self)->&Path{&self.path}
    pub(super) fn metadata_device(&self)->Result<u64,i32>{
        self.file.metadata().map(|m|m.dev()).map_err(status)
    }
}

impl rustos_component_model::storage_replay::JournalDevice for ReleaseJournalFile {
    fn read(
        &mut self,index:u64,out:&mut [u8;RECORD_BYTES]
    )->Result<bool,rustos_component_model::storage_replay::IoError>{
        use rustos_component_model::storage_replay::IoError;
        match self.read_record(index){
            Ok(Some(bytes))=>{*out=bytes;Ok(true)}
            Ok(None)=>Ok(false),
            Err(_)=>Err(IoError::Corrupt),
        }
    }
    fn append(
        &mut self,index:u64,record:&[u8;RECORD_BYTES]
    )->Result<(),rustos_component_model::storage_replay::IoError>{
        use rustos_component_model::storage_replay::IoError;
        self.append_and_verify(index,*record,self.id).map_err(|_|IoError::SyncFailed)
    }
    fn sync(&mut self)->Result<(),rustos_component_model::storage_replay::IoError>{
        use rustos_component_model::storage_replay::IoError;
        if self.fail_closed{return Err(IoError::SyncFailed)}
        self.file.sync_all().map_err(|_|{
            self.fail_closed=true;
            IoError::SyncFailed
        })
    }
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn header_identity_and_reserved_bytes_are_exact(){
        let id=JournalIdentity{
            device_id:2,namespace_generation:3,
            volume_generation:4,rootd_owner:5,
        };
        let h=header(id);
        assert!(exact_header(&h,id));
        assert!(!exact_header(&h,JournalIdentity{rootd_owner:6,..id}));
        let mut bad=h;
        bad[11]=1;
        assert!(!exact_header(&bad,id));
    }

    fn temp_journal(tag:&str)->PathBuf{
        let dir=std::env::temp_dir().join(format!(
            "rstr-jnl-{}-{}-{}",std::process::id(),tag,
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("release.journal")
    }
    fn id()->JournalIdentity{
        JournalIdentity{device_id:2,namespace_generation:3,
            volume_generation:4,rootd_owner:5}
    }
    fn rec(sequence:u64,kind:durable_recovery::Kind)->[u8;RECORD_BYTES]{
        durable_recovery::encode(Record{sequence,transaction:9,service:7,
            kind,generation:1,payload_hash:[sequence as u8;32]}).unwrap()
    }
    fn two_record_journal(tag:&str)->PathBuf{
        use durable_recovery::Kind;
        let path=temp_journal(tag);
        let mut j=ReleaseJournalFile::provision_new(&path,id(),true).unwrap();
        j.append_and_verify(0,rec(1,Kind::Begin),id()).unwrap();
        j.append_and_verify(1,rec(2,Kind::OldImagePinned),id()).unwrap();
        path
    }
    #[test]
    fn strict_open_replays_and_second_writer_is_refused(){
        let path=two_record_journal("strict");
        let j=ReleaseJournalFile::open_existing(&path,id(),true).unwrap();
        assert_eq!(j.records(),2);
        assert_eq!(j.recovered_cursor().last_sequence,2);
        assert_eq!(ReleaseJournalFile::open_existing(&path,id(),true).err(),
            Some(libc::EWOULDBLOCK));
        assert_eq!(ReleaseJournalFile::open_existing(
            &path,JournalIdentity{rootd_owner:6,..id()},true).err().is_some(),true);
    }
    #[test]
    fn partial_tail_blocks_strict_open_but_recovers_without_losing_acked_records(){
        use durable_recovery::Kind;
        let path=two_record_journal("partial");
        {
            let mut f=OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&rec(3,Kind::CandidateVerified)[..40]).unwrap();
        }
        assert_eq!(ReleaseJournalFile::open_existing(&path,id(),true).err(),
            Some(ERROR_EPROTO));
        let (mut j,discarded)=ReleaseJournalFile::open_recovering(
            &path,id(),true).unwrap();
        assert_eq!(discarded,40);
        assert_eq!(j.records(),2);
        // The next append continues the exact sequence.
        j.append_and_verify(2,rec(3,Kind::CandidateVerified),id()).unwrap();
        drop(j);
        let j=ReleaseJournalFile::open_existing(&path,id(),true).unwrap();
        assert_eq!(j.recovered_cursor().last_kind,Some(Kind::CandidateVerified));
    }
    #[test]
    fn one_corrupt_complete_tail_record_is_discarded_but_midfile_damage_is_not(){
        use durable_recovery::Kind;
        // A full-length record whose bytes are garbage at the very tail.
        let path=two_record_journal("torn");
        {
            let mut f=OpenOptions::new().append(true).open(&path).unwrap();
            let mut bad=rec(3,Kind::CandidateVerified);
            bad[20]^=0xff;
            f.write_all(&bad).unwrap();
        }
        let (j,discarded)=ReleaseJournalFile::open_recovering(
            &path,id(),true).unwrap();
        assert_eq!(discarded,RECORD_BYTES as u64);
        assert_eq!(j.records(),2);
        drop(j);
        // Damage to a record that is followed by more data is not a tail.
        let path=two_record_journal("mid");
        {
            let mut j=ReleaseJournalFile::open_existing(&path,id(),true).unwrap();
            j.append_and_verify(2,rec(3,Kind::CandidateVerified),id()).unwrap();
            j.append_and_verify(3,rec(4,Kind::Quiesced),id()).unwrap();
        }
        {
            let mut f=OpenOptions::new().read(true).write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start((HEADER_BYTES+RECORD_BYTES+20) as u64)).unwrap();
            f.write_all(&[0xaa]).unwrap();
        }
        assert_eq!(ReleaseJournalFile::open_recovering(&path,id(),true).err(),
            Some(ERROR_EPROTO));
        // An unauthenticated mount or wrong identity never recovers.
        assert_eq!(ReleaseJournalFile::open_recovering(&path,id(),false).err(),
            Some(ERROR_EPERM));
    }
    #[test]
    fn append_rejects_gaps_reorders_and_stale_indices(){
        use durable_recovery::Kind;
        let path=temp_journal("order");
        let mut j=ReleaseJournalFile::provision_new(&path,id(),true).unwrap();
        j.append_and_verify(0,rec(1,Kind::Begin),id()).unwrap();
        // Skipped state, repeated index, wrong sequence and foreign owner.
        assert_eq!(j.append_and_verify(1,rec(2,Kind::Quiesced),id()),
            Err(ERROR_EPROTO));
        assert_eq!(j.append_and_verify(0,rec(1,Kind::Begin),id()),
            Err(ERROR_ENOSPC));
        assert_eq!(j.append_and_verify(1,rec(3,Kind::OldImagePinned),id()),
            Err(ERROR_EPROTO));
        assert_eq!(j.append_and_verify(1,rec(2,Kind::OldImagePinned),
            JournalIdentity{rootd_owner:99,..id()}),Err(ERROR_EPERM));
        j.append_and_verify(1,rec(2,Kind::OldImagePinned),id()).unwrap();
        assert_eq!(j.records(),2);
    }
}
