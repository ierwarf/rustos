use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::user::handles::VfsDirectoryHandle;

use super::{
    FilesystemProvider, MountError, MountSource, VfsBackend, VfsContext, VfsError, VfsMetadata,
    VfsNodeKind, VfsOpenResult,
};

const RUN_ROOT_PATH: &str = "/run";
const RUN_USER_PATH: &str = "/run/user";

pub(crate) static RUNFS_PROVIDER: RunFsProvider = RunFsProvider;

pub(crate) struct RunFsProvider;
pub(crate) struct RunFsBackend;

impl FilesystemProvider for RunFsProvider {
    fn name(&self) -> &'static str {
        "runfs"
    }

    fn mount(
        &self,
        source: MountSource,
        _flags: u64,
        _options: Option<&str>,
    ) -> Result<Arc<dyn VfsBackend>, MountError> {
        if !matches!(source, MountSource::None) {
            return Err(MountError::InvalidSource);
        }
        Ok(Arc::new(RunFsBackend))
    }
}

impl VfsBackend for RunFsBackend {
    fn requires_live_process_state(&self) -> bool {
        true
    }

    fn open(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        flags: u64,
        _mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsOpenResult, VfsError> {
        super::validate_read_only_open_flags(flags)?;
        if runtime_dir_kind(absolute_path, context).is_none() {
            return Err(VfsError::NotFound);
        }
        Ok(VfsOpenResult::Directory(VfsDirectoryHandle::new(
            String::from(absolute_path),
            self.read_dir(absolute_path, _relative_path, context)?,
        )))
    }

    fn metadata(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsMetadata, VfsError> {
        if runtime_dir_kind(absolute_path, context).is_none() {
            return Err(VfsError::NotFound);
        }
        Ok(super::default_metadata(
            absolute_path,
            VfsNodeKind::Directory,
            0,
        ))
    }

    fn check_access(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<(), VfsError> {
        super::validate_access_mode(mode)?;
        if runtime_dir_kind(absolute_path, context).is_none() {
            return Err(VfsError::NotFound);
        }
        Ok(())
    }

    fn readlink(
        &self,
        _absolute_path: &str,
        _relative_path: &str,
        _context: &mut VfsContext<'_>,
    ) -> Result<String, VfsError> {
        Err(VfsError::NotFound)
    }

    fn read_dir(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        context: &mut VfsContext<'_>,
    ) -> Result<Vec<crate::user::handles::VfsDirectoryEntry>, VfsError> {
        match runtime_dir_kind(absolute_path, context) {
            Some(RuntimeDirKind::RunRoot) => Ok(vec![super::directory_entry(
                RUN_USER_PATH,
                VfsNodeKind::Directory,
            )]),
            Some(RuntimeDirKind::RunUser) => {
                let uid = current_uid(context)?;
                Ok(vec![super::directory_entry(
                    alloc::format!("{RUN_USER_PATH}/{uid}").as_str(),
                    VfsNodeKind::Directory,
                )])
            }
            Some(RuntimeDirKind::UserRuntime) => Ok(Vec::new()),
            None => Err(VfsError::NotFound),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeDirKind {
    RunRoot,
    RunUser,
    UserRuntime,
}

fn runtime_dir_kind(path: &str, context: &VfsContext<'_>) -> Option<RuntimeDirKind> {
    match path {
        RUN_ROOT_PATH => Some(RuntimeDirKind::RunRoot),
        RUN_USER_PATH => Some(RuntimeDirKind::RunUser),
        _ => {
            let uid = current_uid(context).ok()?;
            (path == alloc::format!("{RUN_USER_PATH}/{uid}")).then_some(RuntimeDirKind::UserRuntime)
        }
    }
}

fn current_uid(context: &VfsContext<'_>) -> Result<u32, VfsError> {
    context
        .process_state()
        .map(|process| process.security().euid())
        .ok_or(VfsError::PermissionDenied)
}
