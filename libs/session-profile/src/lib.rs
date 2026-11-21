//! Launch-environment template grammar.
//!
//! - **Owner:** `runtimed` resolves a template per launch; `runtime-control`
//!   validates registry values when it loads them and `xtask` validates the
//!   tables it stages. This crate owns only the grammar, never a resolved fact.
//! - **Why:** the image builder must not decide runtime facts (which uid a
//!   launch runs as, whether a persistent volume exists). The staged
//!   `runtime-env.tsv` and catalog `env` carry templates; runtimed substitutes
//!   facts it established at launch time.
//! - **Grammar:** literal text and `{name}` placeholders, `name` one of
//!   [`Placeholder`]. There is no escape: a `{` or `}` that does not belong to
//!   a known placeholder is a hard error, never passed through to the app.
//!   `{profile_root}` may appear only at the start of a value and only literal
//!   text may follow it, so the directory a value names under the profile root
//!   is statically known (`Template::profile_suffix`).

use std::fmt;

/// A runtime fact a template may name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Placeholder {
    /// Directory that holds the launch's persistent or fallback profile.
    ProfileRoot,
    /// Per-uid session directory (`XDG_RUNTIME_DIR`).
    RuntimeDir,
    /// World-writable volatile temporary directory (`TMPDIR`).
    TempDir,
    /// Wayland socket name inside the runtime directory (`WAYLAND_DISPLAY`).
    WaylandDisplay,
}

impl Placeholder {
    pub const ALL: [Placeholder; 4] = [
        Placeholder::ProfileRoot,
        Placeholder::RuntimeDir,
        Placeholder::TempDir,
        Placeholder::WaylandDisplay,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Placeholder::ProfileRoot => "profile_root",
            Placeholder::RuntimeDir => "runtime_dir",
            Placeholder::TempDir => "temp_dir",
            Placeholder::WaylandDisplay => "wayland_display",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|placeholder| placeholder.name() == name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemplateError {
    /// `{name}` where `name` is not a [`Placeholder`] (includes `{}`).
    UnknownPlaceholder(String),
    /// A `{` with no closing `}`.
    Unterminated,
    /// A `}` that closes nothing.
    StrayCloseBrace,
    /// `{profile_root}` not at the start, or followed by another placeholder.
    ProfileRootPosition,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlaceholder(name) => write!(f, "unknown placeholder {{{name}}}"),
            Self::Unterminated => f.write_str("unterminated placeholder"),
            Self::StrayCloseBrace => f.write_str("stray '}'"),
            Self::ProfileRootPosition => f.write_str(
                "{profile_root} must start the value and only literal text may follow it",
            ),
        }
    }
}

impl std::error::Error for TemplateError {}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Segment {
    Literal(String),
    Placeholder(Placeholder),
}

/// A parsed, validated value template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Template {
    segments: Vec<Segment>,
}

impl Template {
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(at) = rest.find(['{', '}']) {
            literal.push_str(&rest[..at]);
            if rest.as_bytes()[at] == b'}' {
                return Err(TemplateError::StrayCloseBrace);
            }
            let after = &rest[at + 1..];
            let close = after.find(['{', '}']).filter(|&i| after.as_bytes()[i] == b'}');
            let Some(close) = close else {
                return Err(match after.find('{') {
                    Some(_) => TemplateError::UnknownPlaceholder(
                        after[..after.find('{').unwrap_or(0)].to_owned(),
                    ),
                    None => TemplateError::Unterminated,
                });
            };
            let name = &after[..close];
            let placeholder = Placeholder::from_name(name)
                .ok_or_else(|| TemplateError::UnknownPlaceholder(name.to_owned()))?;
            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(Segment::Placeholder(placeholder));
            rest = &after[close + 1..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        let template = Self { segments };
        let root_ok = template.segments.iter().enumerate().all(|(index, segment)| match segment {
            Segment::Placeholder(Placeholder::ProfileRoot) => {
                index == 0
                    && template.segments[1..]
                        .iter()
                        .all(|later| matches!(later, Segment::Literal(_)))
            }
            _ => true,
        });
        if !root_ok {
            return Err(TemplateError::ProfileRootPosition);
        }
        Ok(template)
    }

    pub fn is_literal(&self) -> bool {
        self.segments.iter().all(|segment| matches!(segment, Segment::Literal(_)))
    }

    pub fn placeholders(&self) -> impl Iterator<Item = Placeholder> + '_ {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Placeholder(placeholder) => Some(*placeholder),
            Segment::Literal(_) => None,
        })
    }

    pub fn uses(&self, wanted: Placeholder) -> bool {
        self.placeholders().any(|placeholder| placeholder == wanted)
    }

    /// For a value of the form `{profile_root}<literal>`: the literal part
    /// (possibly empty). `None` when the value does not name a profile path.
    pub fn profile_suffix(&self) -> Option<&str> {
        match self.segments.as_slice() {
            [Segment::Placeholder(Placeholder::ProfileRoot)] => Some(""),
            [Segment::Placeholder(Placeholder::ProfileRoot), Segment::Literal(suffix)] => {
                Some(suffix)
            }
            _ => None,
        }
    }

    /// Substitutes every placeholder through `resolve`; an error from the
    /// resolver aborts the expansion.
    pub fn expand<E>(
        &self,
        mut resolve: impl FnMut(Placeholder) -> Result<String, E>,
    ) -> Result<String, E> {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Placeholder(placeholder) => out.push_str(&resolve(*placeholder)?),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(text: &str) -> Result<String, TemplateError> {
        Template::parse(text)?.expand(|placeholder| {
            Ok::<_, TemplateError>(format!("<{}>", placeholder.name()))
        })
    }

    #[test]
    fn literals_pass_through_unchanged() {
        assert_eq!(expand("/bin:/usr/bin").unwrap(), "/bin:/usr/bin");
        assert_eq!(expand("").unwrap(), "");
        assert!(Template::parse("C.UTF-8").unwrap().is_literal());
    }

    #[test]
    fn every_placeholder_expands_and_round_trips_its_name() {
        for placeholder in Placeholder::ALL {
            assert_eq!(Placeholder::from_name(placeholder.name()), Some(placeholder));
        }
        assert_eq!(expand("{runtime_dir}/{wayland_display}").unwrap(), "<runtime_dir>/<wayland_display>");
        assert_eq!(expand("{temp_dir}").unwrap(), "<temp_dir>");
        assert_eq!(expand("{profile_root}/home").unwrap(), "<profile_root>/home");
    }

    #[test]
    fn unknown_placeholders_are_hard_errors_never_passed_through() {
        for text in ["{uid}", "{}", "{Runtime_Dir}", "/run/user/{uid}", "{ runtime_dir }"] {
            assert!(
                matches!(Template::parse(text), Err(TemplateError::UnknownPlaceholder(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn malformed_braces_are_rejected() {
        assert_eq!(Template::parse("{runtime_dir"), Err(TemplateError::Unterminated));
        assert_eq!(Template::parse("a{"), Err(TemplateError::Unterminated));
        assert_eq!(Template::parse("a}b"), Err(TemplateError::StrayCloseBrace));
        assert_eq!(Template::parse("}{temp_dir}"), Err(TemplateError::StrayCloseBrace));
        assert!(matches!(
            Template::parse("{{temp_dir}}"),
            Err(TemplateError::UnknownPlaceholder(_))
        ));
        assert!(matches!(
            Template::parse("{temp_dir{runtime_dir}"),
            Err(TemplateError::UnknownPlaceholder(_))
        ));
    }

    #[test]
    fn profile_root_must_lead_and_only_literal_text_may_follow() {
        assert_eq!(Template::parse("/x/{profile_root}"), Err(TemplateError::ProfileRootPosition));
        assert_eq!(
            Template::parse("{profile_root}/{temp_dir}"),
            Err(TemplateError::ProfileRootPosition)
        );
        assert_eq!(
            Template::parse("{profile_root}{profile_root}"),
            Err(TemplateError::ProfileRootPosition)
        );
        assert_eq!(Template::parse("{temp_dir}/{profile_root}"), Err(TemplateError::ProfileRootPosition));
    }

    #[test]
    fn profile_suffix_names_the_directory_under_the_root() {
        assert_eq!(Template::parse("{profile_root}/cache").unwrap().profile_suffix(), Some("/cache"));
        assert_eq!(Template::parse("{profile_root}").unwrap().profile_suffix(), Some(""));
        assert_eq!(Template::parse("{runtime_dir}").unwrap().profile_suffix(), None);
        assert_eq!(Template::parse("/data/home").unwrap().profile_suffix(), None);
        assert!(Template::parse("{profile_root}/a").unwrap().uses(Placeholder::ProfileRoot));
    }

    #[test]
    fn resolver_errors_abort_expansion() {
        let result = Template::parse("{runtime_dir}/x")
            .unwrap()
            .expand(|_| Err::<String, i32>(30));
        assert_eq!(result, Err(30));
    }
}
