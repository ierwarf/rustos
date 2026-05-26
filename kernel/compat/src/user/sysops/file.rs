use alloc::string::String;
use alloc::vec::Vec;

use crate::multitask;

const MAX_OPEN_PATH_LEN: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(crate) enum FileSysopError {
    InvalidArgument,
    NotFound,
}

pub(crate) fn resolve_path_for_process(
    process_id: u64,
    path: &str,
) -> Result<String, FileSysopError> {
    if path.is_empty() || path.len() > MAX_OPEN_PATH_LEN {
        return Err(FileSysopError::InvalidArgument);
    }

    let base_path = if path.starts_with('/') {
        String::from("/")
    } else {
        multitask::with_process_state_by_pid_mut(process_id, |process_state| {
            String::from(process_state.cwd())
        })
        .ok_or(FileSysopError::NotFound)?
    };

    normalize_absolute_path(base_path.as_str(), path)
}

fn normalize_absolute_path(base_path: &str, path: &str) -> Result<String, FileSysopError> {
    let mut components: Vec<&str> = Vec::new();
    if !path.starts_with('/') {
        for component in base_path.split('/') {
            if !component.is_empty() {
                components.push(component);
            }
        }
    }

    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            components.pop();
            continue;
        }
        components.push(component);
    }

    let mut normalized = String::from("/");
    for component in components {
        if normalized.len() > 1 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }

    if normalized.len() > MAX_OPEN_PATH_LEN {
        return Err(FileSysopError::InvalidArgument);
    }

    Ok(normalized)
}
