use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};

// GENERATED: the build script compiles the runtime module only to bake the
// glyph atlas; it uses a subset of the module's items.
#[allow(dead_code)]
#[path = "src/glyph_atlas.rs"]
mod glyph_atlas;
// GENERATED: only the configuration parser is needed at build time.
#[allow(dead_code)]
#[path = "src/ui_config.rs"]
mod ui_config;

mod shared {
    include!("../../tools/build_log_cfg.rs");
}

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("failed to resolve repo root from CARGO_MANIFEST_DIR")?;
    println!("cargo:rerun-if-changed=src/ui_config.rs");
    println!("cargo:rerun-if-changed=src/glyph_atlas.rs");
    let logging_path = repo_root.join("config/rustos.toml");
    let log_cfg_path = repo_root.join("tools/build_log_cfg.rs");

    shared::emit_project_config_rerun(&logging_path);
    println!("cargo:rerun-if-changed={}", log_cfg_path.display());
    let logging = shared::read_project_config(&logging_path)?;
    shared::emit_log_cfgs(&logging);
    build_bundled_atlases(&repo_root.join("assets/ui/bundle"))?;
    Ok(())
}

/// Compile the shipped, licensed faces into the same atlas representation used
/// by custom runtime fonts. Target startup does no file I/O or font parsing.
fn build_bundled_atlases(bundle: &Path) -> Result<(), Box<dyn Error>> {
    use std::fmt::Write;
    let config_path = bundle.join("ui.conf");
    println!("cargo:rerun-if-changed={}", config_path.display());
    let config_bytes = ui_config::read_bounded(&config_path, ui_config::MAX_CONFIG_BYTES)?;
    let config =
        ui_config::parse(std::str::from_utf8(&config_bytes)?).map_err(|err| err.to_string())?;
    let out = PathBuf::from(env::var("OUT_DIR")?);
    let mut generated = String::from(
        "fn make_bundled_assets() -> UiAssets {\nlet mut atlases = BTreeMap::new();\n",
    );
    let mut faces = std::collections::BTreeMap::new();
    for role in ui_config::FontRole::ALL {
        let spec = config.fonts.get(&role).ok_or("bundled font role missing")?;
        let path = bundle.join(&spec.file);
        if !faces.contains_key(&path) {
            println!("cargo:rerun-if-changed={}", path.display());
            let bytes = ui_config::read_bounded(&path, ui_config::MAX_FONT_FILE_BYTES)?;
            let face = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())?;
            faces.insert(path.clone(), face);
        }
        let atlas = glyph_atlas::build_atlas(&faces[&path], spec)?;
        let alpha_name = format!("{role:?}.alpha");
        std::fs::write(out.join(&alpha_name), &atlas.alpha)?;
        writeln!(generated, "atlases.insert(FontRole::{role:?}, GlyphAtlas {{ glyphs: std::borrow::Cow::Borrowed(&[")?;
        for glyph in atlas.glyphs.iter() {
            writeln!(generated, "crate::glyph_atlas::GlyphMetrics {{ width: {}, height: {}, advance: {}, atlas_offset: {} }},", glyph.width, glyph.height, glyph.advance, glyph.atlas_offset)?;
        }
        writeln!(generated, "] ), alpha: std::borrow::Cow::Borrowed(include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{alpha_name}\"))), line_height: {}, cell_width: {}, monospace: {} }});", atlas.line_height, atlas.cell_width, atlas.monospace)?;
    }
    writeln!(generated, "UiAssets {{ atlases, clock_utc_offset_minutes: {}, summary: String::from(\"bundled build-time atlases\") }}\n}}", config.clock_utc_offset_minutes)?;
    std::fs::write(out.join("ui_bundled_atlases.rs"), generated)?;
    Ok(())
}
