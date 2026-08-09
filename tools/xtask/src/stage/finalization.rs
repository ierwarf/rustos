fn stage_image_asset_overlay(src_root: &Path, dst_root: &Path) -> Result<()> {
    copy_tree_files(src_root, dst_root)
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
