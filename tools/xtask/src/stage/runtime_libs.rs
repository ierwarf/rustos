use crate::config::Config;
use crate::package_manifest::{BuilderKind, PackageManifest};
use anyhow::{Context, Result, bail, ensure};
use fs_err as fs;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

const DEFAULT_DIRS: &[&str] = &[
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib64",
    "/usr/lib64",
    "/lib",
    "/usr/lib",
];

#[derive(Debug)]
struct Dynamic {
    object_type: u16,
    dynamic: bool,
    needed: Vec<String>,
    rpath: Option<String>,
    runpath: Option<String>,
    interp: Option<String>,
    soname: Option<String>,
}
fn u16at(b: &[u8], n: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(n..n.checked_add(2).context("offset overflow")?)
            .context("truncated ELF")?
            .try_into()?,
    ))
}
fn u32at(b: &[u8], n: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(n..n.checked_add(4).context("offset overflow")?)
            .context("truncated ELF")?
            .try_into()?,
    ))
}
fn u64at(b: &[u8], n: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        b.get(n..n.checked_add(8).context("offset overflow")?)
            .context("truncated ELF")?
            .try_into()?,
    ))
}
fn num(x: u64) -> Result<usize> {
    Ok(usize::try_from(x)?)
}
fn parse_elf(b: &[u8]) -> Result<Dynamic> {
    ensure!(
        b.len() >= 64 && &b[..6] == b"\x7fELF\x02\x01" && b[6] == 1,
        "not a little-endian ELF64"
    );
    ensure!(
        u16at(b, 18)? == 62 && matches!(u16at(b, 16)?, 2 | 3),
        "not an x86-64 executable or shared object"
    );
    let off = num(u64at(b, 32)?)?;
    let entsize = usize::from(u16at(b, 54)?);
    let count = usize::from(u16at(b, 56)?);
    ensure!(
        entsize >= 56 && count > 0 && count != 0xffff,
        "invalid program header table"
    );
    ensure!(
        off.checked_add(
            entsize
                .checked_mul(count)
                .context("program table overflow")?
        )
        .context("program table overflow")?
            <= b.len(),
        "truncated program header table"
    );
    let mut loads = Vec::new();
    let mut dynamic = None;
    let mut interp = None;
    for i in 0..count {
        let p = off + i * entsize;
        let ty = u32at(b, p)?;
        let file = num(u64at(b, p + 8)?)?;
        let addr = u64at(b, p + 16)?;
        let size = num(u64at(b, p + 32)?)?;
        ensure!(
            file.checked_add(size).context("segment overflow")? <= b.len(),
            "segment outside file"
        );
        if ty == 1 {
            loads.push((addr, file, size));
        } else if ty == 2 {
            ensure!(
                dynamic.replace((file, size)).is_none(),
                "multiple PT_DYNAMIC"
            );
        } else if ty == 3 {
            ensure!(interp.is_none(), "multiple PT_INTERP");
            ensure!((2..=4096).contains(&size), "invalid PT_INTERP length");
            let name = &b[file..file + size];
            ensure!(name[size - 1] == 0 && !name[..size - 1].contains(&0), "malformed PT_INTERP string");
            let name = std::str::from_utf8(&name[..size - 1])?;
            safe_virtual(Path::new(name)).context("invalid PT_INTERP path")?;
            interp = Some(name.to_owned());
        }
    }
    let Some((d, sz)) = dynamic else {
        ensure!(interp.is_none() && u16at(b, 16)? == 2, "missing PT_DYNAMIC");
        return Ok(Dynamic {
            object_type: u16at(b, 16)?,
            dynamic: false,
            needed: Vec::new(),
            rpath: None,
            runpath: None,
            interp: None,
            soname: None,
        });
    };
    ensure!(sz % 16 == 0, "malformed PT_DYNAMIC");
    let mut entries = Vec::new();
    let mut straddr = None;
    let mut strsz = None;
    let mut terminated = false;
    for p in (d..d + sz).step_by(16) {
        let tag = u64at(b, p)?;
        let val = u64at(b, p + 8)?;
        if tag == 0 {
            terminated = true;
            break;
        }
        if tag == 5 {
            ensure!(straddr.replace(val).is_none(), "duplicate DT_STRTAB");
        } else if tag == 10 {
            ensure!(strsz.replace(num(val)?).is_none(), "duplicate DT_STRSZ");
        }
        entries.push((tag, val));
    }
    ensure!(terminated, "unterminated PT_DYNAMIC");
    let addr = straddr.context("missing DT_STRTAB")?;
    let len = strsz.context("missing DT_STRSZ")?;
    let mut str_off = None;
    for (base, file, size) in loads {
        if let Some(delta) = addr.checked_sub(base).and_then(|v| usize::try_from(v).ok()) {
            if delta.checked_add(len).is_some_and(|end| end <= size) {
                ensure!(
                    str_off.replace(file + delta).is_none(),
                    "ambiguous DT_STRTAB"
                );
            }
        }
    }
    let off = str_off.context("DT_STRTAB not in file-backed PT_LOAD")?;
    let strings = &b[off..off + len];
    let string = |v: u64| -> Result<String> {
        let tail = strings
            .get(num(v)?..)
            .context("dynamic string outside table")?;
        let end = tail
            .iter()
            .position(|x| *x == 0)
            .context("unterminated dynamic string")?;
        Ok(std::str::from_utf8(&tail[..end])?.to_owned())
    };
    let mut result = Dynamic {
        object_type: u16at(b, 16)?,
        dynamic: true,
        needed: Vec::new(),
        rpath: None,
        runpath: None,
        interp,
        soname: None,
    };
    for (tag, val) in entries {
        match tag {
            1 => {
                let name = string(val)?;
                ensure!(
                    !name.is_empty()
                        && name != "."
                        && name != ".."
                        && !name.contains('/')
                        && !name.contains('$'),
                    "unsafe DT_NEEDED {name:?}"
                );
                result.needed.push(name);
            }
            14 => {
                ensure!(result.soname.replace(string(val)?).is_none(), "duplicate DT_SONAME");
            }
            15 => {
                ensure!(
                    result.rpath.replace(string(val)?).is_none(),
                    "duplicate DT_RPATH"
                );
            }
            29 => {
                ensure!(
                    result.runpath.replace(string(val)?).is_none(),
                    "duplicate DT_RUNPATH"
                );
            }
            _ => {}
        }
    }
    Ok(result)
}
fn safe_virtual(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "relative loader path {}",
        path.display()
    );
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(out.pop(), "loader path escapes root");
            }
            _ => bail!("invalid loader path"),
        }
    }
    Ok(out)
}
fn expand_paths(value: &str, origin: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for part in value.split(':') {
        ensure!(!part.is_empty(), "empty loader search path");
        let origin = origin.to_str().context("non-UTF8 origin")?;
        let mut rest = part;
        let mut expanded = String::new();
        while let Some(index) = rest.find('$') {
            expanded.push_str(&rest[..index]);
            rest = &rest[index..];
            if let Some(suffix) = rest.strip_prefix("${ORIGIN}") {
                expanded.push_str(origin);
                rest = suffix;
            } else if let Some(suffix) = rest.strip_prefix("$ORIGIN") {
                ensure!(
                    !suffix.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'),
                    "unsupported dynamic loader token in {part}"
                );
                expanded.push_str(origin);
                rest = suffix;
            } else {
                bail!("unsupported dynamic loader token in {part}");
            }
        }
        expanded.push_str(rest);
        result.push(safe_virtual(Path::new(&expanded))?);
    }
    Ok(result)
}
pub(super) fn image_path(root: &Path, virtual_path: &Path) -> Result<PathBuf> {
    let rel = virtual_path.strip_prefix("/")?;
    let candidate = root.join(rel);
    let mut ancestor = candidate.as_path();
    while std::fs::symlink_metadata(ancestor).is_err() {
        ancestor = ancestor
            .parent()
            .context("image path has no existing ancestor")?;
    }
    let resolved = fs::canonicalize(ancestor)?;
    let base = fs::canonicalize(root)?;
    ensure!(
        resolved.starts_with(base),
        "image path escapes root: {}",
        candidate.display()
    );
    Ok(candidate)
}
fn inspect(path: &Path) -> Result<Dynamic> {
    parse_elf(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("inspect {}", path.display()))
}

const LIBC: &str = "/lib/x86_64-linux-gnu/libc.so.6";
const LOADER: &str = "/lib64/ld-linux-x86-64.so.2";

pub(super) struct RuntimeBaseline {
    libc: Vec<u8>,
    loader: Vec<u8>,
}
impl RuntimeBaseline {
    pub(super) fn capture(image: &Path) -> Result<Self> {
        Ok(Self { libc: fs::read(image_path(image, Path::new(LIBC))?)?,
            loader: fs::read(image_path(image, Path::new(LOADER))?)? })
    }
    pub(super) fn verify(&self, image: &Path) -> Result<()> {
        ensure!(fs::read(image_path(image, Path::new(LIBC))?)? == self.libc,
            "staging replaced the intended libc baseline");
        ensure!(fs::read(image_path(image, Path::new(LOADER))?)? == self.loader,
            "staging replaced the intended loader baseline");
        Ok(())
    }
}

fn collect_bundle_elfs(image: &Path, bundle: &Path, objects: &mut Vec<PathBuf>) -> Result<()> {
    use std::io::Read;
    let path = image_path(image, bundle)?;
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.is_dir() {
        let mut entries = fs::read_dir(&path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries { collect_bundle_elfs(image, &bundle.join(entry.file_name()), objects)?; }
    } else {
        ensure!(!metadata.is_symlink() || !path.is_dir(), "directory symlink in prebuilt bundle");
        ensure!(path.is_file(), "non-file prebuilt bundle entry {}", path.display());
        let mut magic = [0u8; 4];
        match fs::File::open(&path)?.read_exact(&mut magic) {
            Ok(()) if magic == *b"\x7fELF" => {
                let info = inspect(&path)?;
                if info.dynamic { objects.push(bundle.to_owned()); }
            }
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {},
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn stage_prebuilt_app_runtime_libraries(
    config: &Config,
    manifests: &[PackageManifest],
) -> Result<Vec<PathBuf>> {
    let mut apps = Vec::new();
    for m in manifests {
        if m.build.builder == BuilderKind::PrebuiltApp && m.artifact_path(config).exists() {
            let root = safe_virtual(Path::new(&format!("/{}", m.install.path.trim_start_matches('/'))))?;
            collect_bundle_elfs(&config.image_dir, &root, &mut apps)?;
        }
    }
    apps.sort();
    apps.dedup();
    if apps.is_empty() {
        return Ok(apps);
    }
    let status = std::process::Command::new("python3")
        .arg(
            config
                .root_dir
                .join("tools/xtask/src/stage/noble_runtime.py"),
        )
        .arg("provision")
        .arg(&config.build_dir)
        .status()
        .context("run verified Noble runtime provisioner")?;
    ensure!(status.success(), "Noble runtime provisioning failed");
    let source = config.build_dir.join("runtime-libs/noble/root");
    resolve_closures(&config.image_dir, &source, &apps)?;
    Ok(apps)
}
fn resolve_closures(image: &Path, source: &Path, apps: &[PathBuf]) -> Result<()> {
    for app in apps {
        let mut queue = VecDeque::from([(app.clone(), Vec::<PathBuf>::new())]);
        let mut visited = HashSet::new();
        let mut loaded: HashMap<String, PathBuf> = HashMap::new();
        while let Some((object, inherited)) = queue.pop_front() {
            if !visited.insert(object.clone()) {
                continue;
            }
            let info = inspect(&image_path(image, &object)?)?;
            let origin = object.parent().context("object has no parent")?;
            let own_rpath = if info.runpath.is_none() {
                info.rpath
                    .as_deref()
                    .map(|v| expand_paths(v, origin))
                    .transpose()?
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let runpath = info
                .runpath
                .as_deref()
                .map(|v| expand_paths(v, origin))
                .transpose()?
                .unwrap_or_default();
            let mut child_inherited = own_rpath.clone();
            child_inherited.extend(inherited.clone());
            let mut dirs = own_rpath;
            if info.runpath.is_none() {
                dirs.extend(inherited);
            }
            dirs.extend(runpath);
            dirs.extend(DEFAULT_DIRS.iter().map(PathBuf::from));
            for name in info.needed {
                if let Some(existing) = loaded.get(&name) {
                    queue.push_back((existing.clone(), child_inherited.clone()));
                    continue;
                }
                let mut found = None;
                for dir in &dirs {
                    let virt = dir.join(&name);
                    let path = image_path(image, &virt)?;
                    if path.exists() {
                        let dependency = inspect(&path)?;
                        ensure!(
                            dependency.object_type == 3 && dependency.dynamic,
                            "dependency is not dynamic ET_DYN: {}",
                            path.display()
                        );
                        let baseline = if name == "libc.so.6" {
                            Some("/lib/x86_64-linux-gnu/libc.so.6")
                        } else if name.starts_with("ld-linux") {
                            Some("/lib64/ld-linux-x86-64.so.2")
                        } else {
                            None
                        };
                        if let Some(baseline) = baseline {
                            ensure!(
                                fs::read(&path)?
                                    == fs::read(image_path(image, Path::new(baseline))?)?,
                                "private libc or loader is not the signed image baseline: {}",
                                path.display()
                            );
                        }
                        found = Some(virt);
                        break;
                    }
                }
                if found.is_none() {
                    let src = source.join("usr/lib/x86_64-linux-gnu").join(&name);
                    ensure!(
                        src.is_file(),
                        "missing {name}, required by {}",
                        object.display()
                    );
                    let canonical = fs::canonicalize(&src)?;
                    ensure!(
                        canonical.starts_with(fs::canonicalize(source)?),
                        "provisioned library escapes root"
                    );
                    let dependency = inspect(&canonical)?;
                    ensure!(
                        dependency.object_type == 3 && dependency.dynamic,
                        "provisioned dependency is not dynamic ET_DYN"
                    );
                    ensure!(
                        name != "libc.so.6" && !name.starts_with("ld-linux"),
                        "refusing to replace signed libc or loader"
                    );
                    let virt = PathBuf::from("/usr/lib/x86_64-linux-gnu").join(&name);
                    let dst = image_path(image, &virt)?;
                    fs::create_dir_all(dst.parent().unwrap())?;
                    ensure!(!dst.exists(), "refusing to overwrite {}", dst.display());
                    fs::copy(canonical, &dst)?;
                    found = Some(virt);
                }
                let found = found.unwrap();
                loaded.insert(name, found.clone());
                queue.push_back((found, child_inherited.clone()));
            }
        }
    }
    Ok(())
}

fn verify_baseline_resolution(image: &Path, trace: &str, baseline: &RuntimeBaseline) -> Result<Vec<(String, PathBuf)>> {
    let mut resolutions = Vec::new();
    for line in trace.lines().map(str::trim) {
        let Some((requested, resolution)) = line.split_once(" => ") else { continue; };
        let (path, address) = resolution.rsplit_once(" (").context("malformed loader resolution")?;
        let address = address.strip_suffix(')').and_then(|v| v.strip_prefix("0x"))
            .context("malformed loader address")?;
        ensure!(!address.is_empty() && address.bytes().all(|b| b.is_ascii_hexdigit()), "invalid loader address");
        ensure!(!path.chars().any(char::is_control), "control character in loader resolution");
        let virtual_path = safe_virtual(Path::new(path))?;
        let selected = image_path(image, &virtual_path)?;
        let bytes = fs::read(&selected)?;
        let info = parse_elf(&bytes)?;
        // A dependency may be requested through a different basename while
        // still declaring the libc/loader SONAME. Classify the actual object,
        // not only the trace's requested name (or the lookup path).
        for (name, expected) in [("libc.so.6", &baseline.libc),
            ("ld-linux-x86-64.so.2", &baseline.loader)] {
            if requested == name || selected.file_name().is_some_and(|v| v == name)
                || info.soname.as_deref() == Some(name) {
                ensure!(bytes == *expected,
                    "final loader/cache selected non-baseline {name} via {requested} => {path}");
            }
        }
        resolutions.push((requested.to_owned(), virtual_path));
    }
    Ok(resolutions)
}

pub(super) fn verify_relocations(image: &Path, apps: &[PathBuf]) -> Result<Vec<super::library_cache::LibraryUse>> {
    use std::os::unix::fs::PermissionsExt;
    // Image staging does not retain executable mode. Bind an executable copy of
    // the *same* signed loader over its image path inside the isolated root.
    let loader = image_path(image, Path::new("/lib64/ld-linux-x86-64.so.2"))?;
    let baseline = RuntimeBaseline::capture(image)?;
    let tmp = tempfile::tempdir()?;
    let copy = tmp.path().join("ld-linux-x86-64.so.2");
    fs::copy(&loader, &copy).context("copy image loader for relocation check")?;
    fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755))?;
    let mut uses = Vec::new();
    for app in apps {
        let info = inspect(&image_path(image, app)?)?;
        let interpreter = info.interp.as_deref().unwrap_or(LOADER);
        ensure!(fs::read(image_path(image, Path::new(interpreter))?)? == fs::read(&loader)?,
            "PT_INTERP is not the image loader baseline for {}", app.display());
        let output = std::process::Command::new("bwrap")
            .args(["--die-with-parent", "--unshare-all", "--ro-bind"])
            .arg(image)
            .arg("/")
            .arg("--ro-bind")
            .arg(&copy)
            .arg(interpreter)
            .args([
                "--clearenv",
                "--setenv",
                "LD_TRACE_LOADED_OBJECTS",
                "1",
                "--setenv",
                "LD_WARN",
                "1",
                "--setenv",
                "LD_BIND_NOW",
                "1",
                "--",
            ])
            .arg(interpreter)
            .arg(app)
            .output()
            .context("run isolated target loader (requires bubblewrap and user namespaces)")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        ensure!(
            output.status.success()
                && !stdout.contains("not found")
                && !stdout.contains("undefined symbol")
                && stderr.trim().is_empty(),
            "target loader relocation check failed for {}: status={} stdout={} stderr={}",
            app.display(),
            output.status,
            stdout,
            stderr
        );
        uses.extend(verify_baseline_resolution(image, &stdout, &baseline)?.into_iter()
            .map(|(requested, path)| super::library_cache::LibraryUse {
                application: app.clone(), path, requested,
            }));
    }
    Ok(uses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn gcc(args: &[&str]) {
        let status = Command::new("cc").args(args).status().unwrap();
        assert!(status.success());
    }
    fn fixture(image: &Path, app: &str, private_dep: &str) {
        let dir = image.join(app);
        fs::create_dir_all(&dir).unwrap();
        let c = dir.join("empty.c");
        fs::write(&c, "void f(void) {}\n").unwrap();
        let dep = dir.join(private_dep);
        gcc(&[
            "-nostdlib",
            "-shared",
            "-fPIC",
            c.to_str().unwrap(),
            "-o",
            dep.to_str().unwrap(),
        ]);
        let private = dir.join("libprivate.so");
        // Link by SONAME rather than passing a pathname as DT_NEEDED.
        gcc(&[
            "-nostdlib",
            "-shared",
            "-fPIC",
            c.to_str().unwrap(),
            "-Wl,-soname,libprivate.so",
            "-Wl,--no-as-needed",
            "-L",
            dir.to_str().unwrap(),
            &format!("-l:{}", private_dep),
            "-Wl,-rpath,$ORIGIN",
            "-o",
            private.to_str().unwrap(),
        ]);
        let bin = dir.join("program");
        gcc(&[
            "-nostdlib",
            "-no-pie",
            c.to_str().unwrap(),
            "-Wl,-e,f",
            "-Wl,--no-as-needed",
            "-L",
            dir.to_str().unwrap(),
            "-l:libprivate.so",
            "-Wl,-rpath,$ORIGIN",
            "-o",
            bin.to_str().unwrap(),
        ]);
    }
    #[test]
    fn resolves_private_runpaths_per_app_and_rejects_missing_transitive() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("image");
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fixture(&image, "a", "liba.so");
        fixture(&image, "b", "libb.so");
        let apps = [PathBuf::from("/a/program"), PathBuf::from("/b/program")];
        resolve_closures(&image, &source, &apps).unwrap();
        fs::remove_file(image.join("b/libb.so")).unwrap();
        let err = resolve_closures(&image, &source, &apps)
            .unwrap_err()
            .to_string();
        assert!(err.contains("libb.so"), "{err}");
    }
    #[test]
    fn parses_sectionless_elf_and_rejects_wrong_machine_and_paths() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path(), "fixture", "libdep.so");
        let mut elf = fs::read(tmp.path().join("fixture/program")).unwrap();
        let expected = parse_elf(&elf).unwrap().needed;
        let mut unsafe_name = elf.clone();
        let pos = unsafe_name
            .windows(b"libprivate.so\0".len())
            .position(|x| x == b"libprivate.so\0")
            .unwrap();
        unsafe_name[pos..pos + 2].copy_from_slice(b"./");
        assert!(parse_elf(&unsafe_name).is_err());
        elf[40..48].fill(0);
        elf[58..64].fill(0);
        assert_eq!(parse_elf(&elf).unwrap().needed, expected);
        elf[18..20].copy_from_slice(&183u16.to_le_bytes());
        assert!(parse_elf(&elf).is_err());
        assert!(safe_virtual(Path::new("/../../escape")).is_err());
        assert!(expand_paths(":/lib", Path::new("/app")).is_err());
        assert!(expand_paths("$LIB", Path::new("/app")).is_err());
        assert!(expand_paths("$ORIGIN_suffix", Path::new("/app")).is_err());
    }
    #[test]
    fn accepts_static_executable_without_dynamic_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path().join("static.c");
        let exe = tmp.path().join("static");
        fs::write(&c, "void _start(void) {}\n").unwrap();
        gcc(&[
            "-nostdlib",
            "-static",
            c.to_str().unwrap(),
            "-o",
            exe.to_str().unwrap(),
        ]);
        assert!(!inspect(&exe).unwrap().dynamic);
    }
    #[test]
    fn scans_helpers_and_shared_objects_without_desktop_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path(), "bundle", "libhelper.so");
        fs::write(tmp.path().join("bundle/asset.dat"), b"asset").unwrap();
        let mut objects = Vec::new();
        collect_bundle_elfs(tmp.path(), Path::new("/bundle"), &mut objects).unwrap();
        assert!(objects.iter().any(|p| p.ends_with("program")));
        assert!(objects.iter().any(|p| p.ends_with("libhelper.so")));
        assert!(objects.iter().any(|p| p.ends_with("libprivate.so")));
        fs::write(tmp.path().join("bundle/truncated-helper"), b"\x7fELF").unwrap();
        assert!(collect_bundle_elfs(tmp.path(), Path::new("/bundle"), &mut Vec::new()).is_err());
    }

    #[test]
    fn actual_interpreter_is_checked_and_must_be_terminated() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path(), "bundle", "libhelper.so");
        let app = tmp.path().join("bundle/program");
        let mut bytes = fs::read(&app).unwrap();
        let info = parse_elf(&bytes).unwrap();
        assert_eq!(info.interp.as_deref(), Some(LOADER));
        let ph = num(u64at(&bytes, 32).unwrap()).unwrap();
        let stride = usize::from(u16at(&bytes, 54).unwrap());
        let count = usize::from(u16at(&bytes, 56).unwrap());
        let header = (0..count).map(|n| ph + n * stride).find(|&p| u32at(&bytes, p).unwrap() == 3).unwrap();
        let offset = num(u64at(&bytes, header + 8).unwrap()).unwrap();
        let len = num(u64at(&bytes, header + 32).unwrap()).unwrap();
        bytes[offset + len - 1] = b'x';
        assert!(parse_elf(&bytes).is_err());
        bytes[offset + len - 1] = 0;
        bytes[offset] = b'.';
        assert!(parse_elf(&bytes).is_err(), "relative PT_INTERP must fail");
        // A valid ELF with an absent actual interpreter fails before any host
        // loader can substitute its own interpreter or library directories.
        let loader = tmp.path().join(LOADER.trim_start_matches('/'));
        fs::create_dir_all(loader.parent().unwrap()).unwrap();
        fs::write(&loader, b"baseline").unwrap();
        let libc = tmp.path().join(LIBC.trim_start_matches('/'));
        fs::create_dir_all(libc.parent().unwrap()).unwrap();
        fs::write(libc, b"libc baseline").unwrap();
        bytes[offset] = b'/';
        bytes[offset + 1] = b'X';
        fs::write(&app, bytes).unwrap();
        assert!(verify_relocations(tmp.path(), &[PathBuf::from("/bundle/program")]).is_err());
    }

    #[test]
    fn baseline_replacement_is_detected_before_signing() {
        let tmp = tempfile::tempdir().unwrap();
        for (path, content) in [(LIBC, b"libc".as_slice()), (LOADER, b"loader".as_slice())] {
            let file = tmp.path().join(path.trim_start_matches('/'));
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        let baseline = RuntimeBaseline::capture(tmp.path()).unwrap();
        baseline.verify(tmp.path()).unwrap();
        fs::write(tmp.path().join(LIBC.trim_start_matches('/')), b"replacement").unwrap();
        assert!(baseline.verify(tmp.path()).is_err());
    }

    #[test]
    fn final_cache_resolution_cannot_select_a_compatible_private_libc() {
        let tmp = tempfile::tempdir().unwrap();
        let alternative = tmp.path().join("opt/alternate-libc.so");
        fs::create_dir_all(alternative.parent().unwrap()).unwrap();
        let source = tmp.path().join("provider.c");
        fs::write(&source, "void f(void) {}\n").unwrap();
        gcc(&["-nostdlib", "-shared", "-fPIC", source.to_str().unwrap(),
            "-Wl,-soname,libc.so.6", "-o", alternative.to_str().unwrap()]);
        let baseline = RuntimeBaseline { libc: fs::read(&alternative).unwrap(), loader: Vec::new() };
        assert_eq!(parse_elf(&baseline.libc).unwrap().soname.as_deref(), Some("libc.so.6"));
        let mut other = baseline.libc.clone();
        other.extend_from_slice(b"non-baseline provider");
        fs::write(&alternative, &other).unwrap();
        let trace = "\tlibc.so.6 => /opt/alternate-libc.so (0x1234)\n";
        assert!(verify_baseline_resolution(tmp.path(), trace, &baseline).is_err());
        let alias_trace = "\tlibalternate.so => /opt/alternate-libc.so (0x1234)\n";
        let error = verify_baseline_resolution(tmp.path(), alias_trace, &baseline).unwrap_err();
        assert!(error.to_string().contains("non-baseline libc.so.6"));
        fs::write(&alternative, &baseline.libc).unwrap();
        verify_baseline_resolution(tmp.path(), trace, &baseline).unwrap();
        verify_baseline_resolution(tmp.path(), alias_trace, &baseline).unwrap();
        assert!(verify_baseline_resolution(tmp.path(), "libc.so.6 => /opt/alternate-libc.so (bad)\n", &baseline).is_err());
    }

    #[test]
    fn rejects_symlink_parent_escape() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("image");
        fs::create_dir(&image).unwrap();
        symlink(tmp.path(), image.join("out")).unwrap();
        assert!(image_path(&image, Path::new("/out/nonexistent")).is_err());
        symlink(tmp.path().join("missing"), image.join("dangling")).unwrap();
        assert!(image_path(&image, Path::new("/dangling")).is_err());
    }
}

#[cfg(test)]
mod chrome_validation {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    #[ignore = "requires explicit Chrome bundle, baseline overlay, and downloaded Noble archive cache"]
    fn chrome_target_loader_relocates_without_host_fallbacks() {
        let overlay = PathBuf::from(
            std::env::var_os("RUSTOS_RUNTIME_FIXTURE_OVERLAY").expect("overlay required"),
        );
        let chrome = PathBuf::from(
            std::env::var_os("RUSTOS_RUNTIME_FIXTURE_CHROME").expect("Chrome bundle directory required"),
        );
        let cache = PathBuf::from(
            std::env::var_os("RUSTOS_RUNTIME_FIXTURE_CACHE").expect("archive cache required"),
        );
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("image");
        super::super::stage_image_asset_overlay(&overlay, &image).unwrap();
        let app = image.join("apps/chrome/chrome");
        fs::create_dir_all(app.parent().unwrap()).unwrap();
        assert!(chrome.is_dir(), "fixture must include helpers and bundled shared objects");
        crate::util::copy_tree_files(&chrome, app.parent().unwrap()).unwrap();
        let libc = image.join("lib/x86_64-linux-gnu/libc.so.6");
        let loader = image.join("lib64/ld-linux-x86-64.so.2");
        let before = (
            Sha256::digest(fs::read(&libc).unwrap()),
            Sha256::digest(fs::read(&loader).unwrap()),
        );
        let mut config = Config::from_env().unwrap();
        config.image_dir = image.clone();
        config.build_dir = tmp.path().join("build");
        config.artifact_dir = tmp.path().join("artifacts");
        let manifest: PackageManifest = toml::from_str(include_str!(
            "../../../../apps/chrome/RUSTOS.package.toml.example"
        ))
        .unwrap();
        fs::create_dir_all(manifest.artifact_path(&config)).unwrap();
        let dest_cache = config.build_dir.join("runtime-libs/noble/cache");
        fs::create_dir_all(&dest_cache).unwrap();
        for entry in fs::read_dir(cache).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), dest_cache.join(entry.file_name())).unwrap();
        }
        let baseline = RuntimeBaseline::capture(&image).unwrap();
        let apps = stage_prebuilt_app_runtime_libraries(&config, &[manifest]).unwrap();
        assert!(apps.iter().any(|p| p.ends_with("chrome_crashpad_handler")));
        assert!(apps.iter().any(|p| p.ends_with("libvk_swiftshader.so")));
        super::super::generate_dynamic_linker_cache(&image).unwrap();
        super::super::materialize_image_file_aliases(&image).unwrap();
        baseline.verify(&image).unwrap();
        let uses = verify_relocations(&image, &apps).unwrap();
        super::super::library_cache::stage_plan(&image, &uses).unwrap();
        let warmup = fs::read_to_string(image.join("etc/pagerd/library-cache.conf")).unwrap();
        let plan = rustos_library_cache_policy::Plan::parse(&warmup).unwrap();
        assert!(plan.entry_count() > 0, "common runtimes must be prewarm candidates");
        for entry in plan.entries() {
            let entry = entry.unwrap();
            let bytes = fs::read(image_path(&image, Path::new(entry.path)).unwrap()).unwrap();
            use sha2::Digest;
            let actual: [u8; 32] = sha2::Sha256::digest(&bytes).into();
            assert_eq!(entry.sha256, actual);
            assert_eq!(entry.bytes, bytes.len() as u64);
        }
        let files = super::super::collect_image_files(&image).unwrap();
        config.boot_disk_image = tmp.path().join("verified-runtime-fat.img");
        super::super::write_boot_disk_image(&config).unwrap();
        eprintln!("verified {} bundle ELF roots and {} serialized FAT files", apps.len(), files.len());
        let glib = image.join("usr/lib/x86_64-linux-gnu/libglib-2.0.so.0");
        let original = fs::read(&glib).unwrap();
        let mut incompatible = original.clone();
        let version = b"GLIBC_2.34\0";
        let pos = incompatible
            .windows(version.len())
            .position(|w| w == version)
            .expect("fixture GLIBC version");
        incompatible[pos..pos + version.len()].copy_from_slice(b"GLIBC_9.99\0");
        // A finalized cache may prefer a directory absent from the default
        // search list. Validate the real cache, not --inhibit-cache's easier
        // but different library closure.
        let conf = image.join("etc/ld.so.conf");
        let saved_conf = fs::read(&conf).unwrap();
        let conflicting = image.join("usr/local/lib/libglib-2.0.so.0");
        fs::create_dir_all(conflicting.parent().unwrap()).unwrap();
        fs::write(&conflicting, &incompatible).unwrap();
        fs::write(&conf, b"/usr/local/lib\n/usr/lib/x86_64-linux-gnu\n/lib/x86_64-linux-gnu\n").unwrap();
        super::super::generate_dynamic_linker_cache(&image).unwrap();
        super::super::materialize_image_file_aliases(&image).unwrap();
        assert!(verify_relocations(&image, &apps).is_err(), "conflicting final cache must fail");
        fs::remove_file(conflicting).unwrap();
        // The loader accepts extra non-loaded bytes, but the baseline policy
        // must still reject a cache-selected alternative with different bytes.
        let private_libc = image.join("usr/local/lib/libc.so.6");
        let mut other_libc = fs::read(&libc).unwrap();
        other_libc.extend_from_slice(b"non-baseline fixture");
        fs::write(&private_libc, other_libc).unwrap();
        super::super::generate_dynamic_linker_cache(&image).unwrap();
        super::super::materialize_image_file_aliases(&image).unwrap();
        let error = verify_relocations(&image, &apps).unwrap_err();
        assert!(error.to_string().contains("non-baseline libc.so.6"), "wrong private-libc refusal: {error:#}");
        fs::remove_file(private_libc).unwrap();
        fs::write(&conf, saved_conf).unwrap();
        super::super::generate_dynamic_linker_cache(&image).unwrap();
        super::super::materialize_image_file_aliases(&image).unwrap();
        verify_relocations(&image, &apps).unwrap();
        fs::write(&glib, &incompatible).unwrap();
        assert!(
            verify_relocations(&image, &apps).is_err(),
            "incompatible version must fail"
        );
        fs::write(&glib, &original).unwrap();
        fs::remove_file(&glib).unwrap();
        assert!(
            verify_relocations(&image, &apps).is_err(),
            "missing dependency must not fall back to host"
        );
        assert_eq!(
            before,
            (
                Sha256::digest(fs::read(libc).unwrap()),
                Sha256::digest(fs::read(loader).unwrap())
            )
        );
    }
}
