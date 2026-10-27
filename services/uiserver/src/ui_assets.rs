//! The user interface's runtime assets: the parsed configuration and the glyph
//! atlases rasterized from the font files it names.
//!
//! Loading happens once, at startup, from the file named by `RUSTOS_UI_CONFIG`,
//! or from where the `ui-assets` data package installs it when that is unset.
//! A missing or invalid asset set never takes the desktop down: the server runs
//! without text and says why in its boot log, which beats a service that exits
//! because a font did not parse.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::font::{build_atlas, GlyphAtlas};
use crate::sys::{spawn_ui_thread, UiThreadRole};
use crate::ui_config::{self, FontRole, MAX_CONFIG_BYTES, MAX_FONT_FILE_BYTES};

pub(crate) struct UiAssets {
    atlases: BTreeMap<FontRole, GlyphAtlas>,
    clock_utc_offset_minutes: i32,
    /// One line for the boot log: what loaded.
    summary: String,
}

/// Why a load failed, which decides whether trying again can help.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LoadError {
    /// A file could not be read. uiserver starts before the storage stack has
    /// published the boot volume, so this is expected for a while after boot.
    Io(String),
    /// The files were read but are not a valid asset set; retrying cannot help.
    Invalid(String),
}

impl LoadError {
    fn message(&self) -> &str {
        match self {
            LoadError::Io(message) | LoadError::Invalid(message) => message,
        }
    }
}

/// What the loader thread reports to the main loop.
pub(crate) enum AssetLoad {
    Ready(String),
    Failed(String),
}

static ASSETS: OnceLock<UiAssets> = OnceLock::new();

/// Wait between attempts while the boot volume is not readable yet.
const RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// How long the loader keeps trying before giving up, which bounds how long a
/// permanently missing volume keeps a thread polling.
const RETRY_BUDGET: Duration = Duration::from_secs(60);

/// The loaded assets, or `None` until the loader has installed them. Text is
/// simply not drawn in the meantime.
pub(crate) fn assets() -> Option<&'static UiAssets> {
    #[cfg(test)]
    {
        Some(ASSETS.get_or_init(|| {
            UiAssets::load(&repo_bundle_config()).expect("the shipped UI bundle loads")
        }))
    }
    #[cfg(not(test))]
    {
        ASSETS.get()
    }
}

/// The configured UTC offset, or UTC until the assets are loaded.
pub(crate) fn clock_utc_offset_minutes() -> i32 {
    assets().map_or(0, UiAssets::clock_utc_offset_minutes)
}

/// Where the `ui-assets` data package installs its configuration. runtimed
/// launches uiserver with a fixed environment, so the variable below is an
/// override for development rather than something the image has to set.
const INSTALLED_CONFIG_PATH: &str = "/assets/ui/ui.conf";

fn config_path() -> PathBuf {
    std::env::var_os("RUSTOS_UI_CONFIG")
        .map_or_else(|| PathBuf::from(INSTALLED_CONFIG_PATH), PathBuf::from)
}

#[cfg(test)]
pub(crate) fn repo_bundle_config() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/ui/bundle/ui.conf")
}

/// Load the assets on a background thread, retrying while the boot volume is
/// not readable yet, and install them for [`assets`]. The result arrives on the
/// returned channel so the main loop can repaint what was drawn without text.
pub(crate) fn start_asset_loader() -> Receiver<AssetLoad> {
    let (sender, receiver) = mpsc::channel();
    let spawned = spawn_ui_thread(
        UiThreadRole::Background,
        "uiserver-asset-loader",
        move || {
            let started = Instant::now();
            let path = config_path();
            let mut waited = false;
            let outcome = retry_transient(RETRY_BUDGET, RETRY_INTERVAL, || {
                UiAssets::load(&path).inspect_err(|error| {
                    // One line for the whole wait, not one per attempt.
                    if !std::mem::replace(&mut waited, true) {
                        crate::sys::diag_line(format!(
                            "uiserver: ui assets not readable yet, retrying: {}",
                            error.message()
                        ));
                    }
                })
            });
            let elapsed_ms = started.elapsed().as_millis();
            let report = match outcome {
                Ok(loaded) => {
                    let summary = format!("{} elapsed_ms={elapsed_ms}", loaded.summary());
                    // Only this thread installs, and only once.
                    let _ = ASSETS.set(loaded);
                    AssetLoad::Ready(summary)
                }
                Err(message) => AssetLoad::Failed(format!(
                    "unavailable, text is not drawn: {message} elapsed_ms={elapsed_ms}"
                )),
            };
            // Report from this thread too: the main loop prints the same line when
            // it picks the result up, but may be busy or gone by then.
            if let AssetLoad::Ready(summary) = &report {
                crate::sys::diag_line(format!("uiserver: ui assets loaded {summary}"));
            }
            let _ = sender.send(report);
        },
    );
    if spawned.is_err() {
        // The thread could not start; with no sender left the channel just
        // stays empty and the server runs without text.
        crate::sys::diag_line("uiserver: ui asset loader thread failed to start");
    }
    receiver
}

/// Run `attempt` until it succeeds, fails for a reason retrying cannot fix, or
/// `budget` has passed, waiting `interval` between tries.
fn retry_transient<T>(
    budget: Duration,
    interval: Duration,
    mut attempt: impl FnMut() -> Result<T, LoadError>,
) -> Result<T, String> {
    let started = Instant::now();
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(LoadError::Io(_)) if started.elapsed() < budget => std::thread::sleep(interval),
            Err(error) => return Err(error.message().to_string()),
        }
    }
}

fn read_error(path: &Path, what: &str, err: &std::io::Error) -> LoadError {
    let message = format!("cannot read {what}{}: {err}", path.display());
    // An oversized file stays oversized. Anything else, including "no such
    // file" and "no such device", can be the boot volume not being published
    // yet, so the retry budget decides when to stop waiting.
    match err.kind() {
        std::io::ErrorKind::InvalidData => LoadError::Invalid(message),
        _ => LoadError::Io(message),
    }
}

impl UiAssets {
    pub(crate) fn load(config_path: &Path) -> Result<Self, LoadError> {
        let text = ui_config::read_bounded(config_path, MAX_CONFIG_BYTES)
            .map_err(|err| read_error(config_path, "", &err))?;
        let text = String::from_utf8(text)
            .map_err(|_| LoadError::Invalid(format!("{} is not UTF-8", config_path.display())))?;
        let config = ui_config::parse(&text).map_err(|err| {
            LoadError::Invalid(format!("invalid {}: {err}", config_path.display()))
        })?;
        let base = config_path.parent().unwrap_or(Path::new("."));

        // Several roles may share one face; parse each file once.
        let mut faces: BTreeMap<PathBuf, fontdue::Font> = BTreeMap::new();
        let mut atlases = BTreeMap::new();
        for (role, spec) in &config.fonts {
            let path = base.join(&spec.file);
            if !faces.contains_key(&path) {
                let bytes = ui_config::read_bounded(&path, MAX_FONT_FILE_BYTES)
                    .map_err(|err| read_error(&path, "font ", &err))?;
                let face = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                    .map_err(|err| {
                        LoadError::Invalid(format!("cannot parse font {}: {err}", path.display()))
                    })?;
                faces.insert(path.clone(), face);
            }
            let atlas = build_atlas(&faces[&path], spec)
                .map_err(|err| LoadError::Invalid(format!("font {}: {err}", path.display())))?;
            atlases.insert(*role, atlas);
        }
        let summary = format!(
            "loaded config={} fonts={} files={} utc_offset_minutes={}",
            config_path.display(),
            atlases.len(),
            faces.len(),
            config.clock_utc_offset_minutes
        );
        Ok(Self {
            atlases,
            clock_utc_offset_minutes: config.clock_utc_offset_minutes,
            summary,
        })
    }

    pub(crate) fn atlas(&self, role: FontRole) -> Option<&GlyphAtlas> {
        self.atlases.get(&role)
    }

    pub(crate) fn clock_utc_offset_minutes(&self) -> i32 {
        self.clock_utc_offset_minutes
    }

    pub(crate) fn summary(&self) -> &str {
        &self.summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_bundle_loads_every_role() {
        let assets = assets().expect("shipped bundle loads");
        for role in FontRole::ALL {
            assert!(assets.atlas(role).is_some(), "{role:?} has no atlas");
        }
        assert!(assets.atlas(FontRole::Terminal).unwrap().monospace);
        assert!(!assets.atlas(FontRole::UiMedium).unwrap().monospace);
        assert!(assets.summary().contains("fonts=4"));
    }

    #[test]
    fn a_missing_config_is_an_io_error_worth_retrying() {
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-ui.conf");
        match UiAssets::load(&missing) {
            Err(LoadError::Io(message)) => assert!(message.contains("cannot read"), "{message}"),
            other => panic!("expected an I/O error, got {:?}", other.err()),
        }
    }

    fn scratch_bundle(name: &str, conf: &str, font: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uiserver-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.ttf"), font).unwrap();
        std::fs::write(dir.join("ui.conf"), conf).unwrap();
        dir
    }

    #[test]
    fn a_font_file_that_is_not_a_font_is_invalid_not_retryable() {
        let dir = scratch_bundle(
            "badfont",
            "[font.ui_medium]\nfile = bad.ttf\nsize = 15\n",
            b"not a font",
        );
        let result = UiAssets::load(&dir.join("ui.conf"));
        std::fs::remove_dir_all(&dir).unwrap();
        match result {
            Err(LoadError::Invalid(message)) => {
                assert!(message.contains("cannot parse font"), "{message}")
            }
            other => panic!("expected an invalid-asset error, got {:?}", other.err()),
        }
    }

    #[test]
    fn a_malformed_config_is_invalid_and_names_its_line() {
        let dir = scratch_bundle("badconf", "[clock]\nzone = 1\n", b"");
        let result = UiAssets::load(&dir.join("ui.conf"));
        std::fs::remove_dir_all(&dir).unwrap();
        match result {
            Err(LoadError::Invalid(message)) => assert!(message.contains("line 2"), "{message}"),
            other => panic!("expected an invalid-asset error, got {:?}", other.err()),
        }
    }

    #[test]
    fn transient_errors_are_retried_until_the_volume_appears() {
        let mut attempts = 0;
        let result = retry_transient(Duration::from_secs(5), Duration::ZERO, || {
            attempts += 1;
            if attempts < 4 {
                Err(LoadError::Io("No such device".into()))
            } else {
                Ok(attempts)
            }
        });
        assert_eq!(result, Ok(4));
    }

    #[test]
    fn invalid_assets_are_not_retried() {
        let mut attempts = 0;
        let result: Result<(), String> =
            retry_transient(Duration::from_secs(5), Duration::ZERO, || {
                attempts += 1;
                Err(LoadError::Invalid("bad".into()))
            });
        assert_eq!(result, Err("bad".to_string()));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn retrying_stops_when_the_budget_is_spent() {
        let mut attempts = 0;
        let result: Result<(), String> = retry_transient(Duration::ZERO, Duration::ZERO, || {
            attempts += 1;
            Err(LoadError::Io("still not there".into()))
        });
        assert_eq!(result, Err("still not there".to_string()));
        assert_eq!(attempts, 1, "an exhausted budget makes one attempt");
    }
}
