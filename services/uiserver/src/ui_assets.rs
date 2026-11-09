//! UI assets start with build-time bundled atlases and the bundled clock offset.
//! Configured assets load off the UI thread; the main loop explicitly promotes
//! them between frames. Failed or unavailable overrides retain usable text.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::app::{ConsoleWindow, DesktopSurfaceCache};
use crate::glyph_atlas::{build_atlas, GlyphAtlas};
use crate::startup::Dependency;
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
    Ready(UiAssets),
    Failed(String),
}

struct AssetStore {
    configured: OnceLock<UiAssets>,
    revision: AtomicU64,
}

impl AssetStore {
    const fn new() -> Self {
        Self {
            configured: OnceLock::new(),
            revision: AtomicU64::new(1),
        }
    }

    fn assets(&self) -> &UiAssets {
        self.configured.get().unwrap_or_else(|| bundled_assets())
    }

    // Only the UI thread promotes, before any drawing in that loop iteration.
    // No renderer can see new geometry with old retained cache revisions.
    fn promote(
        &self,
        loaded: UiAssets,
        desktop: &mut DesktopSurfaceCache,
        consoles: &mut [ConsoleWindow],
    ) -> bool {
        if self.configured.set(loaded).is_err() {
            return false;
        }
        desktop.invalidate_chrome();
        desktop.clock_checked = None;
        for window in consoles {
            window.terminal_dirty = true;
            window.invalidate_surface();
        }
        self.revision.store(2, Ordering::Release);
        true
    }
}

static ASSETS: AssetStore = AssetStore::new();
static BUNDLED: OnceLock<UiAssets> = OnceLock::new();
include!(concat!(env!("OUT_DIR"), "/ui_bundled_atlases.rs"));

pub(crate) fn bundled_assets() -> &'static UiAssets {
    BUNDLED.get_or_init(make_bundled_assets)
}

pub(crate) fn asset_revision() -> u64 {
    ASSETS.revision.load(Ordering::Acquire)
}

pub(crate) fn promote(
    loaded: UiAssets,
    desktop: &mut DesktopSurfaceCache,
    consoles: &mut [ConsoleWindow],
) -> bool {
    ASSETS.promote(loaded, desktop, consoles)
}

/// The wait between attempts and the total budget are the `Assets`
/// dependency's owned policy (`crate::startup::Dependency::policy`), so the
/// published readiness record and this loop can never disagree about when the
/// loader gives up.
fn retry_budget() -> Duration {
    Dependency::Assets.policy().total_budget
}

fn retry_interval() -> Duration {
    Dependency::Assets.policy().initial_backoff
}

/// The active set; bundled geometry and pixels are available without storage.
pub(crate) fn assets() -> &'static UiAssets {
    ASSETS.assets()
}

pub(crate) fn clock_utc_offset_minutes() -> i32 {
    assets().clock_utc_offset_minutes()
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
/// not readable yet. The channel transfers ownership to the UI loop; the
/// loader never publishes fonts in the middle of rendering a frame.
pub(crate) fn start_asset_loader() -> Receiver<AssetLoad> {
    let (sender, receiver) = mpsc::channel();
    let spawned = spawn_ui_thread(
        UiThreadRole::Background,
        "uiserver-asset-loader",
        move || {
            let started = Instant::now();
            let path = config_path();
            let mut waited = false;
            let startup = crate::startup::global();
            startup.begin(Dependency::Assets);
            let outcome = retry_transient(retry_budget(), retry_interval(), || {
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
            match &outcome {
                Ok(_) => startup.succeed(Dependency::Assets),
                // Bundled typography is a complete fallback, so the desktop
                // degrades instead of failing; the record still says why.
                Err(_) => {
                    startup.fail(Dependency::Assets, libc::ENOENT, false);
                }
            }
            let report = match outcome {
                Ok(loaded) => {
                    let summary = format!("{} elapsed_ms={elapsed_ms}", loaded.summary());
                    crate::sys::diag_line(format!("uiserver: ui assets loaded {summary}"));
                    AssetLoad::Ready(loaded)
                }
                Err(message) => AssetLoad::Failed(format!(
                    "unavailable, keeping bundled text: {message} elapsed_ms={elapsed_ms}"
                )),
            };
            let _ = sender.send(report);
        },
    );
    if spawned.is_err() {
        // The thread could not start; with no sender left the channel just
        // disconnects and the server retains bundled typography.
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
        let assets = bundled_assets();
        for role in FontRole::ALL {
            assert!(assets.atlas(role).is_some(), "{role:?} has no atlas");
        }
        assert!(assets.atlas(FontRole::Terminal).unwrap().monospace);
        assert!(!assets.atlas(FontRole::UiMedium).unwrap().monospace);
        assert_eq!(assets.summary(), "bundled build-time atlases");
    }

    #[test]
    fn build_time_and_runtime_atlases_have_identical_geometry_and_pixels() {
        let runtime = UiAssets::load(&repo_bundle_config()).expect("shipped runtime bundle");
        let bundled = bundled_assets();
        assert_eq!(
            bundled.clock_utc_offset_minutes(),
            runtime.clock_utc_offset_minutes()
        );
        for role in FontRole::ALL {
            let a = bundled.atlas(role).unwrap();
            let b = runtime.atlas(role).unwrap();
            assert_eq!(a.glyphs, b.glyphs, "{role:?} geometry");
            assert_eq!(a.alpha, b.alpha, "{role:?} pixels");
            assert_eq!(
                (a.line_height, a.cell_width, a.monospace),
                (b.line_height, b.cell_width, b.monospace)
            );
            assert!(matches!(a.alpha, std::borrow::Cow::Borrowed(_)));
            assert!(matches!(a.glyphs, std::borrow::Cow::Borrowed(_)));
        }
    }

    #[test]
    fn only_ui_loop_promotion_changes_revision_and_invalidates_caches_once() {
        let store = AssetStore::new();
        let mut desktop = DesktopSurfaceCache {
            background_valid: true,
            chrome_valid: true,
            clock_checked: Some(Instant::now()),
            ..DesktopSurfaceCache::default()
        };
        let mut windows = [ConsoleWindow::new(
            7,
            "Shell".into(),
            crate::canvas::Rect {
                x: 10,
                y: 10,
                width: 200,
                height: 100,
            },
            0,
        )];
        windows[0].surface_cache.valid = true;
        windows[0].terminal_dirty = false;
        let mut loaded = make_bundled_assets();
        loaded.clock_utc_offset_minutes = -300;
        // Loader completion/queued assets are not publication.
        assert_eq!(store.revision.load(Ordering::Acquire), 1);
        assert_eq!(
            store.assets().clock_utc_offset_minutes(),
            bundled_assets().clock_utc_offset_minutes()
        );
        assert!(desktop.chrome_valid && windows[0].surface_cache.valid);
        assert!(store.promote(loaded, &mut desktop, &mut windows));
        assert_eq!(store.revision.load(Ordering::Acquire), 2);
        assert_eq!(store.assets().clock_utc_offset_minutes(), -300);
        assert!(!desktop.chrome_valid);
        assert!(
            desktop.background_valid,
            "font changes do not discard wallpaper"
        );
        assert!(desktop.clock_checked.is_none());
        assert!(!windows[0].surface_cache.valid);
        assert!(windows[0].terminal_dirty);
        desktop.chrome_valid = true;
        windows[0].surface_cache.valid = true;
        windows[0].terminal_dirty = false;
        assert!(!store.promote(make_bundled_assets(), &mut desktop, &mut windows));
        assert_eq!(store.revision.load(Ordering::Acquire), 2);
        assert!(desktop.chrome_valid && windows[0].surface_cache.valid);
        assert!(!windows[0].terminal_dirty);
    }

    #[test]
    fn custom_fonts_and_timezone_are_preserved_at_explicit_handoff() {
        let bytes = std::fs::read(
            repo_bundle_config()
                .parent()
                .unwrap()
                .join("fonts/MapleMono-Regular.ttf"),
        )
        .unwrap();
        let dir = scratch_bundle("custom-font", "[clock]\nutc_offset_minutes = -300\n[font.terminal]\nfile = bad.ttf\nsize = 24\nline_height = 30\nmonospace = true\n", &bytes);
        let loaded =
            UiAssets::load(&dir.join("ui.conf")).unwrap_or_else(|error| panic!("{error:?}"));
        std::fs::remove_dir_all(dir).unwrap();
        let bundled = bundled_assets().atlas(FontRole::Terminal).unwrap();
        let custom = loaded.atlas(FontRole::Terminal).unwrap();
        assert!(custom.cell_width > bundled.cell_width);
        assert_eq!(custom.line_height, 30);
        let store = AssetStore::new();
        assert!(store.promote(loaded, &mut DesktopSurfaceCache::default(), &mut []));
        assert_eq!(store.assets().clock_utc_offset_minutes(), -300);
        assert_eq!(
            store
                .assets()
                .atlas(FontRole::Terminal)
                .unwrap()
                .line_height,
            30
        );
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
