extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelPathError {
    InvalidArgument,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MountRole {
    #[default]
    Standard,
    SystemImage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedMountOptions {
    pub role: MountRole,
    pub backend_options: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountConfigError {
    InvalidArgument,
    UnsupportedMountFlags,
}

pub fn normalize_kernel_path(path: &str) -> Result<String, KernelPathError> {
    if path.is_empty() {
        return Err(KernelPathError::InvalidArgument);
    }

    let absolute = if path.starts_with('/') {
        path
    } else {
        return normalize_absolute_kernel_path(alloc::format!("/{path}").as_str());
    };

    normalize_absolute_kernel_path(absolute)
}

pub fn normalize_absolute_kernel_path(path: &str) -> Result<String, KernelPathError> {
    if !path.starts_with('/') {
        return Err(KernelPathError::InvalidArgument);
    }

    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            components.pop();
            continue;
        }
        if component
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(KernelPathError::InvalidArgument);
        }
        components.push(component);
    }

    if components.is_empty() {
        return Ok(String::from("/"));
    }

    let mut normalized = String::new();
    for component in components {
        normalized.push('/');
        normalized.push_str(component);
    }
    Ok(normalized)
}

pub fn path_is_within_mount(absolute_path: &str, mount_path: &str) -> bool {
    if mount_path == "/" {
        return absolute_path.starts_with('/');
    }

    absolute_path == mount_path
        || (absolute_path.starts_with(mount_path)
            && absolute_path.as_bytes().get(mount_path.len()) == Some(&b'/'))
}

pub fn path_relative_to_mount(absolute_path: &str, mount_path: &str) -> String {
    if mount_path == "/" {
        return String::from(absolute_path);
    }

    let suffix = &absolute_path[mount_path.len()..];
    if suffix.is_empty() {
        String::from("/")
    } else {
        String::from(suffix)
    }
}

pub fn mount_child_name<'a>(parent_path: &str, mount_path: &'a str) -> Option<&'a str> {
    if parent_path == "/" {
        return mount_path
            .strip_prefix('/')?
            .split('/')
            .next()
            .filter(|name| !name.is_empty());
    }

    let suffix = mount_path.strip_prefix(parent_path)?.strip_prefix('/')?;
    suffix.split('/').next().filter(|name| !name.is_empty())
}

pub fn validate_mount_flags(flags: u64, supported_flags: u64) -> Result<(), MountConfigError> {
    if flags & !supported_flags != 0 {
        return Err(MountConfigError::UnsupportedMountFlags);
    }
    Ok(())
}

pub fn parse_mount_options(options: Option<&str>) -> Result<ParsedMountOptions, MountConfigError> {
    let Some(options) = options else {
        return Ok(ParsedMountOptions::default());
    };

    let mut role = MountRole::Standard;
    let mut backend = String::new();
    for option in options.split(',') {
        let option = option.trim();
        if option.is_empty() {
            continue;
        }

        match option {
            "role=standard" => role = MountRole::Standard,
            "role=system-image" => role = MountRole::SystemImage,
            _ => {
                if option
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control())
                {
                    return Err(MountConfigError::InvalidArgument);
                }
                if !backend.is_empty() {
                    backend.push(',');
                }
                backend.push_str(option);
            }
        }
    }

    Ok(ParsedMountOptions {
        role,
        backend_options: (!backend.is_empty()).then_some(backend),
    })
}

pub fn path_inode(path: &[u8]) -> u64 {
    fnv1a64(path).max(1)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        KernelPathError, MountConfigError, MountRole, mount_child_name,
        normalize_absolute_kernel_path, normalize_kernel_path, parse_mount_options, path_inode,
        path_is_within_mount, path_relative_to_mount, validate_mount_flags,
    };

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize_kernel_path("a/b").unwrap(), "/a/b");
        assert_eq!(normalize_kernel_path("/a/./b/../c").unwrap(), "/a/c");
        assert_eq!(
            normalize_absolute_kernel_path("a/b"),
            Err(KernelPathError::InvalidArgument)
        );
    }

    #[test]
    fn mount_helpers_work() {
        assert!(path_is_within_mount("/mnt/data/file", "/mnt"));
        assert!(!path_is_within_mount("/mnt2/data", "/mnt"));
        assert_eq!(
            path_relative_to_mount("/mnt/data/file", "/mnt"),
            "/data/file"
        );
        assert_eq!(mount_child_name("/", "/proc/self"), Some("proc"));
        assert_eq!(mount_child_name("/mnt", "/mnt/sub/path"), Some("sub"));
    }

    #[test]
    fn parse_mount_options_tracks_role_and_backend_options() {
        let parsed =
            parse_mount_options(Some("role=system-image,uid=0,gid=0,shortname=mixed")).unwrap();
        assert_eq!(parsed.role, MountRole::SystemImage);
        assert_eq!(
            parsed.backend_options.as_deref(),
            Some("uid=0,gid=0,shortname=mixed")
        );

        let parsed = parse_mount_options(None).unwrap();
        assert_eq!(parsed.role, MountRole::Standard);
        assert_eq!(parsed.backend_options, None);
    }

    #[test]
    fn validate_mount_flags_rejects_unknown_bits() {
        assert_eq!(validate_mount_flags(0, 1), Ok(()));
        assert_eq!(validate_mount_flags(1, 1), Ok(()));
        assert_eq!(
            validate_mount_flags(2, 1),
            Err(MountConfigError::UnsupportedMountFlags)
        );
    }

    #[test]
    fn path_inode_is_stable_and_non_zero() {
        let inode = path_inode(b"/dev/input0");
        assert_ne!(inode, 0);
        assert_eq!(inode, path_inode(b"/dev/input0"));
        assert_ne!(inode, path_inode(b"/dev/input1"));
    }
}
