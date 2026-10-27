//! The user interface's runtime configuration: which font file backs each text
//! role at what size, and the home screen clock's UTC offset.
//!
//! The configuration is a small INI-like text file shipped in the `ui-assets`
//! data package next to the font files it names. Its location comes from the
//! `RUSTOS_UI_CONFIG` environment variable, so nothing about fonts or the time
//! zone is compiled into the server. Parsing is strict: an unknown section, an
//! unknown key, a duplicate or an out-of-range value rejects the whole file
//! with the offending line number, because a silently ignored typo would show
//! up as missing text.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};

/// Larger than any realistic hand-written configuration.
pub(crate) const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// Bounds one font file. The Inter and Maple Mono faces are a few hundred KiB;
/// a face past this would cost the server more memory than its text is worth.
pub(crate) const MAX_FONT_FILE_BYTES: usize = 4 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;

const MIN_FONT_SIZE_PX: u16 = 4;
const MAX_FONT_SIZE_PX: u16 = 512;
const MAX_LINE_HEIGHT_PX: u16 = 1024;
const MIN_UTC_OFFSET_MINUTES: i32 = -12 * 60;
const MAX_UTC_OFFSET_MINUTES: i32 = 14 * 60;

/// A place text is drawn. The code only knows these roles; the file, size and
/// metrics behind each one come from the configuration.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum FontRole {
    /// Home screen date line and other larger labels.
    UiLarge,
    /// Window titles and app labels.
    UiMedium,
    /// The home screen time of day.
    Clock,
    /// Terminal cells; must be monospace.
    Terminal,
}

impl FontRole {
    #[cfg(test)]
    pub(crate) const ALL: [FontRole; 4] = [
        FontRole::UiLarge,
        FontRole::UiMedium,
        FontRole::Clock,
        FontRole::Terminal,
    ];

    fn from_section_name(name: &str) -> Option<Self> {
        match name {
            "ui_large" => Some(FontRole::UiLarge),
            "ui_medium" => Some(FontRole::UiMedium),
            "clock" => Some(FontRole::Clock),
            "terminal" => Some(FontRole::Terminal),
            _ => None,
        }
    }
}

/// Which characters a role rasterizes. A large display face only needs the
/// glyphs it draws, which keeps its atlas small.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Charset {
    /// Printable ASCII, U+0020 through U+007E.
    Ascii,
    /// Digits, the colon and the space: enough for a clock.
    Clock,
}

impl Charset {
    pub(crate) fn includes(self, ch: char) -> bool {
        match self {
            Charset::Ascii => true,
            Charset::Clock => ch.is_ascii_digit() || ch == ':' || ch == ' ',
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "ascii" => Some(Charset::Ascii),
            "clock" => Some(Charset::Clock),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FontSpec {
    /// Relative to the configuration file's directory.
    pub(crate) file: PathBuf,
    pub(crate) size_px: u16,
    /// `None` uses the face's own line height.
    pub(crate) line_height_px: Option<u16>,
    pub(crate) monospace: bool,
    pub(crate) charset: Charset,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UiConfig {
    /// Minutes east of UTC. RustOS keeps UTC and has no timezone database.
    pub(crate) clock_utc_offset_minutes: i32,
    pub(crate) fonts: BTreeMap<FontRole, FontSpec>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ConfigError {
    line: usize,
    message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

fn error(line: usize, message: impl Into<String>) -> ConfigError {
    ConfigError {
        line,
        message: message.into(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Section {
    Clock,
    Font(FontRole),
}

#[derive(Default)]
struct PartialFont {
    file: Option<PathBuf>,
    size_px: Option<u16>,
    line_height_px: Option<u16>,
    monospace: Option<bool>,
    charset: Option<Charset>,
}

pub(crate) fn parse(text: &str) -> Result<UiConfig, ConfigError> {
    let mut clock_offset = None;
    let mut seen_clock = false;
    let mut partial: BTreeMap<FontRole, PartialFont> = BTreeMap::new();
    let mut section = None;

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            let name = name
                .strip_suffix(']')
                .ok_or_else(|| error(line_no, "section header is missing its closing ]"))?
                .trim();
            section = Some(if name == "clock" {
                if seen_clock {
                    return Err(error(line_no, "duplicate [clock] section"));
                }
                seen_clock = true;
                Section::Clock
            } else if let Some(role_name) = name.strip_prefix("font.") {
                let role = FontRole::from_section_name(role_name.trim())
                    .ok_or_else(|| error(line_no, format!("unknown font role `{role_name}`")))?;
                if partial.insert(role, PartialFont::default()).is_some() {
                    return Err(error(line_no, format!("duplicate [{name}] section")));
                }
                Section::Font(role)
            } else {
                return Err(error(line_no, format!("unknown section [{name}]")));
            });
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| error(line_no, "expected `key = value`"))?;
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            return Err(error(line_no, format!("`{key}` has no value")));
        }
        match section {
            None => return Err(error(line_no, "key outside any section")),
            Some(Section::Clock) => match key {
                "utc_offset_minutes" => {
                    if clock_offset.is_some() {
                        return Err(error(line_no, "duplicate `utc_offset_minutes`"));
                    }
                    let minutes = value
                        .parse::<i32>()
                        .map_err(|_| error(line_no, "`utc_offset_minutes` must be an integer"))?;
                    if !(MIN_UTC_OFFSET_MINUTES..=MAX_UTC_OFFSET_MINUTES).contains(&minutes) {
                        return Err(error(
                            line_no,
                            format!(
                                "`utc_offset_minutes` must be within \
                                 {MIN_UTC_OFFSET_MINUTES}..={MAX_UTC_OFFSET_MINUTES}"
                            ),
                        ));
                    }
                    clock_offset = Some(minutes);
                }
                _ => return Err(error(line_no, format!("unknown clock key `{key}`"))),
            },
            Some(Section::Font(role)) => {
                let font = partial.get_mut(&role).expect("section inserted its font");
                parse_font_key(font, key, value, line_no)?;
            }
        }
    }

    let mut fonts = BTreeMap::new();
    for (role, font) in partial {
        let missing = |what: &str| error(0, format!("font {role:?} is missing `{what}`"));
        let size_px = font.size_px.ok_or_else(|| missing("size"))?;
        fonts.insert(
            role,
            FontSpec {
                file: font.file.ok_or_else(|| missing("file"))?,
                size_px,
                line_height_px: font.line_height_px,
                monospace: font.monospace.unwrap_or(false),
                charset: font.charset.unwrap_or(Charset::Ascii),
            },
        );
    }
    Ok(UiConfig {
        clock_utc_offset_minutes: clock_offset.unwrap_or(0),
        fonts,
    })
}

fn parse_font_key(
    font: &mut PartialFont,
    key: &str,
    value: &str,
    line_no: usize,
) -> Result<(), ConfigError> {
    fn set<T>(
        slot: &mut Option<T>,
        value: T,
        key: &str,
        line_no: usize,
    ) -> Result<(), ConfigError> {
        if slot.replace(value).is_some() {
            return Err(error(line_no, format!("duplicate `{key}`")));
        }
        Ok(())
    }
    let number = |max: u16, min: u16| -> Result<u16, ConfigError> {
        let parsed = value
            .parse::<u16>()
            .map_err(|_| error(line_no, format!("`{key}` must be a whole number")))?;
        if !(min..=max).contains(&parsed) {
            return Err(error(
                line_no,
                format!("`{key}` must be within {min}..={max}"),
            ));
        }
        Ok(parsed)
    };
    match key {
        "file" => {
            let path = PathBuf::from(value);
            let confined = path
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
            if !confined {
                return Err(error(
                    line_no,
                    "`file` must be a relative path without `..` or a leading `/`",
                ));
            }
            set(&mut font.file, path, key, line_no)
        }
        "size" => set(
            &mut font.size_px,
            number(MAX_FONT_SIZE_PX, MIN_FONT_SIZE_PX)?,
            key,
            line_no,
        ),
        "line_height" => set(
            &mut font.line_height_px,
            number(MAX_LINE_HEIGHT_PX, 1)?,
            key,
            line_no,
        ),
        "monospace" => {
            let flag = match value {
                "true" => true,
                "false" => false,
                _ => return Err(error(line_no, "`monospace` must be true or false")),
            };
            set(&mut font.monospace, flag, key, line_no)
        }
        "charset" => {
            let charset = Charset::parse(value)
                .ok_or_else(|| error(line_no, "`charset` must be `ascii` or `clock`"))?;
            set(&mut font.charset, charset, key, line_no)
        }
        _ => Err(error(line_no, format!("unknown font key `{key}`"))),
    }
}

/// Read a whole file of at most `max_bytes`, in bounded positioned reads so a
/// large or growing file can neither exhaust memory nor move a shared cursor.
pub(crate) fn read_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    let mut chunk = vec![0_u8; READ_CHUNK_BYTES.min(max_bytes.saturating_add(1))];
    loop {
        // Ask for one byte past the limit so a file of exactly `max_bytes` is
        // accepted and anything longer is detected rather than truncated.
        let want = chunk.len().min(max_bytes + 1 - bytes.len());
        match file.read_at(&mut chunk[..want], bytes.len() as u64) {
            Ok(0) => return Ok(bytes),
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() > max_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} is larger than {max_bytes} bytes", path.display()),
                    ));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "\
# comment
[clock]
utc_offset_minutes = 540

[font.ui_large]   # trailing comment
file = fonts/A.ttf
size = 18
line_height = 22

[font.terminal]
file = fonts/B.ttf
size = 16
monospace = true

[font.clock]
file = fonts/C.ttf
size = 88
charset = clock
";

    #[test]
    fn a_full_configuration_parses() {
        let config = parse(FULL).expect("valid configuration");
        assert_eq!(config.clock_utc_offset_minutes, 540);
        assert_eq!(config.fonts.len(), 3);
        let large = &config.fonts[&FontRole::UiLarge];
        assert_eq!(large.file, PathBuf::from("fonts/A.ttf"));
        assert_eq!((large.size_px, large.line_height_px), (18, Some(22)));
        assert!(!large.monospace);
        assert_eq!(large.charset, Charset::Ascii);
        assert!(config.fonts[&FontRole::Terminal].monospace);
        assert_eq!(config.fonts[&FontRole::Clock].charset, Charset::Clock);
        assert_eq!(config.fonts[&FontRole::Clock].line_height_px, None);
    }

    #[test]
    fn the_clock_defaults_to_utc() {
        assert_eq!(parse("").unwrap().clock_utc_offset_minutes, 0);
    }

    #[test]
    fn mistakes_are_rejected_with_their_line() {
        let cases = [
            ("[nope]\n", "unknown section"),
            ("[font.bold]\n", "unknown font role"),
            ("[clock\n", "closing ]"),
            ("size = 4\n", "outside any section"),
            ("[clock]\nzone = 1\n", "unknown clock key"),
            ("[clock]\nutc_offset_minutes = 900\n", "must be within"),
            ("[clock]\nutc_offset_minutes = x\n", "must be an integer"),
            ("[clock]\n[clock]\n", "duplicate [clock]"),
            ("[font.clock]\n[font.clock]\n", "duplicate [font.clock]"),
            ("[font.clock]\nfile = a\nfile = b\n", "duplicate `file`"),
            ("[font.clock]\nfile = /etc/x\n", "relative path"),
            ("[font.clock]\nfile = ../x\n", "relative path"),
            ("[font.clock]\nsize = 3\n", "must be within"),
            ("[font.clock]\nsize = 9999\n", "must be within"),
            ("[font.clock]\nmonospace = yes\n", "true or false"),
            ("[font.clock]\ncharset = utf8\n", "ascii"),
            ("[font.clock]\nweight = 1\n", "unknown font key"),
            ("[font.clock]\nsize =\n", "has no value"),
            ("[font.clock]\nfile\n", "key = value"),
        ];
        for (text, expected) in cases {
            let err = parse(text).expect_err(text).to_string();
            assert!(err.contains(expected), "{text:?}: {err}");
            assert!(err.starts_with("line "), "{text:?}: {err}");
        }
    }

    #[test]
    fn a_font_needs_a_file_and_a_size() {
        let err = parse("[font.clock]\nsize = 20\n").unwrap_err().to_string();
        assert!(err.contains("missing `file`"), "{err}");
        let err = parse("[font.clock]\nfile = a.ttf\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing `size`"), "{err}");
    }

    #[test]
    fn bounded_reads_accept_the_limit_and_reject_more() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(read_bounded(&path, len).unwrap().len(), len);
        let err = read_bounded(&path, len - 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(read_bounded(&path.with_extension("missing"), len).is_err());
    }
}
