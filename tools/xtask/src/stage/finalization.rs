fn stage_image_asset_overlay(src_root: &Path, dst_root: &Path) -> Result<()> {
    copy_tree_files(src_root, dst_root)
}

/// ldconfig may replace a SONAME file with a relative symlink. Materialize
/// those aliases before verification/serialization: the FAT boot volume cannot
/// store symlinks, and its inventory must match the verified image tree.
fn materialize_image_file_aliases(image_dir: &Path) -> Result<()> {
    let entries = WalkDir::new(image_dir).into_iter().collect::<std::result::Result<Vec<_>, _>>()?;
    for entry in entries {
        if !entry.file_type().is_symlink() { continue; }
        let target = crate::util::contained_regular_file(image_dir, entry.path())?;
        let parent = entry.path().parent().context("image alias has no parent")?;
        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        fs::copy(target, tmp.path())?;
        tmp.persist(entry.path()).map_err(|error| error.error)
            .with_context(|| format!("materialize image alias {}", entry.path().display()))?;
    }
    Ok(())
}

fn generate_dynamic_linker_cache(image_dir: &Path) -> Result<()> {
    let ld_so_conf = image_dir.join(LD_SO_CONF_PATH);
    if !ld_so_conf.is_file() {
        return Ok(());
    }

    let Some(ldconfig) = command_in_path("ldconfig") else {
        bail!(
            "missing ldconfig required to generate dynamic linker cache from {}",
            ld_so_conf.display()
        );
    };

    run_command(Command::new(ldconfig).arg("-r").arg(image_dir))
}
