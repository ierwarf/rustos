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
use std::path::{Path, PathBuf};

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
/// The backing location is opened through an already established trusted
/// mount. Never accept an arbitrary client-controlled path as authority.
pub(super) struct ReleaseJournalFile{
    file:File,
    path:PathBuf,
    id:JournalIdentity,
    records:u64,
    fail_closed:bool,
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
        Ok(Self{file,path:path.to_owned(),id,records:count,fail_closed:false})
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
        let header=header(id);
        file.write_all(&header).map_err(status)?;
        file.sync_all().map_err(status)?;
        // Directory fsync ensures the name and inode survive a crash on
        // supporting filesystems. It is not a substitute for media honesty.
        let parent=path.parent().ok_or(ERROR_EINVAL)?;
        File::open(parent).map_err(status)?.sync_all().map_err(status)?;
        Ok(Self{file,path:path.to_owned(),id,records:0,fail_closed:false})
    }
    pub(super) fn id(&self)->JournalIdentity{self.id}
    pub(super) fn records(&self)->u64{self.records}
    pub(super) fn failed(&self)->bool{self.fail_closed}
    pub(super) fn read_record(&mut self,index:u64)
        ->Result<Option<[u8;RECORD_BYTES]>,i32>{
        if self.fail_closed{return Err(ERROR_EIO)}
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
}
