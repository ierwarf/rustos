use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

use crate::user::abi::UserAbi;
use crate::user::process_state::UserProcessState;

use super::VfsError;

const DEFAULT_RUNTIME_LIBRARY_DIRS: &[&str] = &["/lib", "/lib64", "/usr/lib", "/usr/lib64"];
const DEFAULT_RUNTIME_LINKER_FILES: &[&str] =
    &["/etc/ld.so.cache", "/etc/ld.so.preload", "/etc/ld.so.conf"];
const DEFAULT_RUNTIME_LINKER_INCLUDE_DIR: &str = "/etc/ld.so.conf.d";
const DEFAULT_RUNTIME_ASSET_DIRS: &[&str] = &[
    "/usr/lib/locale",
    "/usr/share/locale",
    "/usr/lib/gconv",
    "/usr/share/zoneinfo",
];
const DEFAULT_RUNTIME_ASSET_FILES: &[&str] = &[
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/localtime",
];

static SYSTEM_IMAGE_RUNTIME_POLICY_CACHE: Mutex<Option<SystemImageRuntimePolicyCache>> =
    Mutex::new(None);

#[derive(Clone, Debug)]
struct SystemImageRuntimePolicyCache {
    generation: u64,
    policy: LinuxRuntimeAccessPolicy,
}

#[derive(Clone, Debug, Default)]
struct LinuxRuntimeAccessPolicy {
    directory_prefixes: Vec<String>,
    exact_files: Vec<String>,
}

pub(super) fn linux_runtime_access_allows_path(
    absolute_path: &str,
    abi: UserAbi,
    process_state: &UserProcessState,
) -> bool {
    if abi != UserAbi::Linux {
        return false;
    }

    let policy = system_image_runtime_policy(super::current_mount_generation());
    if policy.allows_path(absolute_path) {
        return true;
    }

    process_state
        .linux_runtime_profile()
        .map(|profile| {
            profile.kernel_runtime_access_dirs().iter().any(|dir| {
                absolute_path == dir
                    || path_is_under_directory(absolute_path, dir)
                    || path_is_directory_ancestor(absolute_path, dir)
            })
        })
        .unwrap_or(false)
}

fn system_image_runtime_policy(generation: u64) -> LinuxRuntimeAccessPolicy {
    {
        let cache = SYSTEM_IMAGE_RUNTIME_POLICY_CACHE.lock();
        if let Some(cache) = cache.as_ref() {
            if cache.generation == generation {
                return cache.policy.clone();
            }
        }
    }

    let policy = build_system_image_runtime_policy();
    let mut cache = SYSTEM_IMAGE_RUNTIME_POLICY_CACHE.lock();
    *cache = Some(SystemImageRuntimePolicyCache {
        generation,
        policy: policy.clone(),
    });
    policy
}

fn build_system_image_runtime_policy() -> LinuxRuntimeAccessPolicy {
    let mut policy = LinuxRuntimeAccessPolicy::with_defaults();
    let mut visited = Vec::new();
    load_runtime_linker_config_file("/etc/ld.so.conf", &mut policy, &mut visited);
    policy
}

impl LinuxRuntimeAccessPolicy {
    fn with_defaults() -> Self {
        let mut policy = Self::default();
        for dir in DEFAULT_RUNTIME_LIBRARY_DIRS {
            policy.allow_directory(dir);
        }
        for file in DEFAULT_RUNTIME_LINKER_FILES {
            policy.allow_exact_file(file);
        }
        policy.allow_directory(DEFAULT_RUNTIME_LINKER_INCLUDE_DIR);
        for dir in DEFAULT_RUNTIME_ASSET_DIRS {
            policy.allow_directory(dir);
        }
        for file in DEFAULT_RUNTIME_ASSET_FILES {
            policy.allow_exact_file(file);
        }
        policy
    }

    fn allows_path(&self, path: &str) -> bool {
        if path == "/" {
            return !self.directory_prefixes.is_empty() || !self.exact_files.is_empty();
        }

        self.exact_files.iter().any(|file| file == path)
            || self
                .directory_prefixes
                .iter()
                .any(|dir| path == dir || path_is_under_directory(path, dir))
            || self
                .exact_files
                .iter()
                .any(|file| path_is_directory_ancestor(path, file))
            || self
                .directory_prefixes
                .iter()
                .any(|dir| path_is_directory_ancestor(path, dir))
    }

    fn allow_directory(&mut self, path: &str) {
        let Some(path) = normalize_runtime_config_path(path) else {
            return;
        };
        push_unique_path(&mut self.directory_prefixes, path.as_str());
    }

    fn allow_exact_file(&mut self, path: &str) {
        let Some(path) = normalize_runtime_config_path(path) else {
            return;
        };
        push_unique_path(&mut self.exact_files, path.as_str());
    }
}

fn load_runtime_linker_config_file(
    path: &str,
    policy: &mut LinuxRuntimeAccessPolicy,
    visited: &mut Vec<String>,
) {
    let Some(path) = normalize_runtime_config_path(path) else {
        return;
    };
    if visited.iter().any(|current| current == &path) {
        return;
    }
    visited.push(path.clone());
    policy.allow_exact_file(path.as_str());

    let bytes = match super::read_path_to_vec_for_kernel(path.as_str()) {
        Ok(bytes) => bytes,
        Err(VfsError::NotFound) => return,
        Err(_) => return,
    };
    let Ok(text) = core::str::from_utf8(bytes.as_slice()) else {
        return;
    };

    for raw_line in text.lines() {
        let line = strip_runtime_config_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(include_spec) = parse_runtime_config_include(line) {
            load_runtime_linker_config_include(include_spec, policy, visited);
            continue;
        }

        policy.allow_directory(line);
    }
}

fn load_runtime_linker_config_include(
    include_spec: &str,
    policy: &mut LinuxRuntimeAccessPolicy,
    visited: &mut Vec<String>,
) {
    let Some(include_path) = normalize_runtime_config_path(include_spec) else {
        return;
    };

    if !include_path.contains('*') {
        load_runtime_linker_config_file(include_path.as_str(), policy, visited);
        return;
    }

    let (dir, pattern) = split_runtime_include_path(include_path.as_str());
    let Ok(mut entries) = super::read_dir_names_for_kernel(dir) else {
        return;
    };
    entries.sort();

    for entry in entries {
        if !runtime_config_pattern_matches(pattern, entry.as_str()) {
            continue;
        }

        let child = if dir == "/" {
            alloc::format!("/{entry}")
        } else {
            alloc::format!("{dir}/{entry}")
        };
        load_runtime_linker_config_file(child.as_str(), policy, visited);
    }
}

fn normalize_runtime_config_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return None;
    }

    let mut components = Vec::new();
    for component in trimmed.split('/') {
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
    for (index, component) in components.iter().enumerate() {
        if index != 0 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    Some(normalized)
}

fn split_runtime_include_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("/", path))
}

fn strip_runtime_config_comment(line: &str) -> &str {
    line.split_once('#')
        .map(|(before, _)| before)
        .unwrap_or(line)
}

fn parse_runtime_config_include(line: &str) -> Option<&str> {
    let mut parts = line.split_whitespace();
    let directive = parts.next()?;
    if directive != "include" {
        return None;
    }
    parts.next()
}

fn runtime_config_pattern_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star = None;
    let mut retry_t = 0usize;

    while t < text.len() {
        if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
            continue;
        }
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry_t = t;
            continue;
        }
        if let Some(star_index) = star {
            p = star_index + 1;
            retry_t += 1;
            t = retry_t;
            continue;
        }
        return false;
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn path_is_under_directory(path: &str, directory: &str) -> bool {
    if directory == "/" {
        return path.starts_with('/') && path.len() > 1;
    }

    path.strip_prefix(directory)
        .map(|suffix| suffix.starts_with('/'))
        .unwrap_or(false)
}

fn path_is_directory_ancestor(directory: &str, target: &str) -> bool {
    directory == "/" || path_is_under_directory(target, directory)
}

fn push_unique_path(dest: &mut Vec<String>, value: &str) {
    if dest.iter().any(|current| current == value) {
        return;
    }
    dest.push(value.to_string());
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{
        LinuxRuntimeAccessPolicy, normalize_runtime_config_path, runtime_config_pattern_matches,
    };

    #[test]
    fn runtime_config_paths_are_normalized() {
        assert_eq!(
            normalize_runtime_config_path("/lib//x86_64-linux-gnu/./../ld-linux.so"),
            Some(String::from("/lib/ld-linux.so"))
        );
        assert_eq!(
            normalize_runtime_config_path(" /etc/ld.so.conf.d/*.conf "),
            Some(String::from("/etc/ld.so.conf.d/*.conf"))
        );
    }

    #[test]
    fn runtime_config_glob_matches_ld_so_conf_entries() {
        assert!(runtime_config_pattern_matches(
            "*.conf",
            "x86_64-linux-gnu.conf"
        ));
        assert!(runtime_config_pattern_matches(
            "rustos-*.conf",
            "rustos-extra.conf"
        ));
        assert!(!runtime_config_pattern_matches("*.conf", "libc.so.6"));
    }

    #[test]
    fn runtime_policy_allows_configured_dirs_and_files() {
        let mut policy = LinuxRuntimeAccessPolicy::with_defaults();
        policy.allow_directory("/opt/rustos/lib");
        policy.allow_exact_file("/etc/ld.so.conf.d/rustos.conf");

        assert!(policy.allows_path("/"));
        assert!(policy.allows_path("/lib64"));
        assert!(policy.allows_path("/lib64/ld-linux-x86-64.so.2"));
        assert!(policy.allows_path("/opt"));
        assert!(policy.allows_path("/opt/rustos/lib"));
        assert!(policy.allows_path("/opt/rustos/lib/libwayland-server.so.0"));
        assert!(policy.allows_path("/etc/ld.so.conf.d/rustos.conf"));
        assert!(!policy.allows_path("/home/user/libwayland-server.so.0"));
    }
}
