//! Template parsing and rendering.
//!
//! A template string contains literal characters interleaved with `{token}`
//! references. Tokens may carry a single modifier separated by a colon
//! (`{seq:05}`, `{camera:raw}`, `{hash:8}`). The grammar is:
//!
//! ```text
//! template := (literal | escape | field)*
//! escape   := "{{" | "}}"
//! field    := "{" name (":" modifier)? "}"
//! ```
//!
//! The sequence number is assigned by a later pass and is not available when
//! a candidate path is first rendered. When `RenderContext.seq` is `None`,
//! `{seq}` renders as a width-tagged sentinel (see [`seq_sentinel`] and
//! [`substitute_seq`]); NUL bytes in the wrapper guarantee no collision with
//! literal path content.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{Datelike, NaiveDateTime, Timelike};
use thiserror::Error;

use crate::metadata::Metadata;

/// Sentinel prefix wrapping a deferred `{seq}` substitution. The body
/// between [`SEQ_SENTINEL_PREFIX`] and [`SEQ_SENTINEL_SUFFIX`] is the ASCII
/// decimal width modifier (`"0"` for unpadded, `"05"` for `{seq:05}`).
pub const SEQ_SENTINEL_PREFIX: &str = "\0__SHELF_SEQ:";
pub const SEQ_SENTINEL_SUFFIX: &str = "__\0";

/// Build the sentinel string for a `{seq}` token with the given width.
#[must_use]
pub fn seq_sentinel(width: usize) -> String {
    format!("{SEQ_SENTINEL_PREFIX}{width}{SEQ_SENTINEL_SUFFIX}")
}

/// Replace every `{seq}` sentinel in `s` with `seq` rendered at the
/// sentinel's encoded width. Returns `None` if a sentinel is malformed.
#[must_use]
pub fn substitute_seq(s: &str, seq: u64) -> Option<String> {
    if !s.contains(SEQ_SENTINEL_PREFIX) {
        return Some(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(SEQ_SENTINEL_PREFIX) {
        out.push_str(&rest[..start]);
        let after_prefix = &rest[start + SEQ_SENTINEL_PREFIX.len()..];
        let end = after_prefix.find(SEQ_SENTINEL_SUFFIX)?;
        let width_raw = &after_prefix[..end];
        if width_raw.is_empty() || !width_raw.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let width: usize = width_raw.parse().ok()?;
        let formatted = format!("{seq:0width$}");
        out.push_str(&formatted);
        rest = &after_prefix[end + SEQ_SENTINEL_SUFFIX.len()..];
    }
    out.push_str(rest);
    Some(out)
}

/// A parsed template ready for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    tokens: Vec<Token>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Literal(String),
    Field {
        name: String,
        modifier: Option<Modifier>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    /// Zero-pad to at least `n` characters. Only valid on `{seq}`.
    Width(usize),
    /// Skip slugification. Only valid on string metadata tokens.
    Raw,
    /// Truncate hex hash to the first `n` characters. Only valid on `{hash}`.
    HashWidth(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    pub taken_at: &'a NaiveDateTime,
    pub metadata: &'a Metadata,
    /// Canonical extension (no leading dot). Renders as the empty string when
    /// `None`.
    pub canonical_ext: Option<&'a str>,
    pub sha256_hex: &'a str,
    /// Sequence number for this file, if assigned. `None` defers to a
    /// width-tagged sentinel (see [`seq_sentinel`]).
    pub seq: Option<u64>,
    /// `[templates.fallbacks]` from the profile.
    pub fallbacks: &'a BTreeMap<String, String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TemplateParseError {
    #[error("unclosed `{{` at byte {pos}")]
    Unclosed { pos: usize },
    #[error("stray `}}` at byte {pos}")]
    Stray { pos: usize },
    #[error("empty token `{{}}` at byte {pos}")]
    Empty { pos: usize },
    #[error("token missing name at byte {pos}")]
    MissingName { pos: usize },
    #[error("token `{name}` has empty modifier at byte {pos}")]
    EmptyModifier { name: String, pos: usize },
    #[error("nested `{{` inside token at byte {pos}")]
    NestedBrace { pos: usize },
    #[error("unknown token `{name}` at byte {pos}")]
    UnknownToken { name: String, pos: usize },
    #[error("malformed modifier `:{modifier}` on `{{{name}}}` at byte {pos}")]
    MalformedModifier {
        name: String,
        modifier: String,
        pos: usize,
    },
    #[error("modifier `:{modifier}` is not valid on `{{{name}}}` at byte {pos}")]
    IllegalModifier {
        name: String,
        modifier: String,
        pos: usize,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RenderError {
    /// Parse-time validation should have caught this; surfacing it explicitly
    /// makes the bug obvious if it ever leaks.
    #[error("internal: token `{{{name}}}` carries an inapplicable modifier at render time")]
    InapplicableModifier { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Date,
    Ext,
    Hash,
    Seq,
    StringMeta,
}

fn field_kind(name: &str) -> Option<FieldKind> {
    match name {
        "yyyy" | "yy" | "mm" | "dd" | "hh" | "min" | "ss" => Some(FieldKind::Date),
        "ext" => Some(FieldKind::Ext),
        "hash" => Some(FieldKind::Hash),
        "seq" => Some(FieldKind::Seq),
        "camera" | "lens" | "kind" | "author" | "title" | "vendor" => Some(FieldKind::StringMeta),
        _ => None,
    }
}

impl Template {
    /// Parse a template string into tokens.
    pub fn parse(s: &str) -> Result<Self, TemplateParseError> {
        let bytes = s.as_bytes();
        let mut tokens: Vec<Token> = Vec::new();
        let mut literal = String::new();
        let mut i = 0;

        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'{' => {
                    if bytes.get(i + 1) == Some(&b'{') {
                        literal.push('{');
                        i += 2;
                        continue;
                    }
                    let open = i;
                    let start = i + 1;
                    let mut end = None;
                    let mut j = start;
                    while j < bytes.len() {
                        match bytes[j] {
                            b'}' => {
                                end = Some(j);
                                break;
                            }
                            b'{' => {
                                return Err(TemplateParseError::NestedBrace { pos: j });
                            }
                            _ => j += 1,
                        }
                    }
                    let end = end.ok_or(TemplateParseError::Unclosed { pos: open })?;
                    let body = &s[start..end];
                    if body.is_empty() {
                        return Err(TemplateParseError::Empty { pos: open });
                    }

                    let (name, modifier_raw) = match body.split_once(':') {
                        Some((n, m)) => (n, Some(m)),
                        None => (body, None),
                    };
                    if name.is_empty() {
                        return Err(TemplateParseError::MissingName { pos: open });
                    }
                    if let Some(m) = modifier_raw
                        && m.is_empty()
                    {
                        return Err(TemplateParseError::EmptyModifier {
                            name: name.to_string(),
                            pos: open,
                        });
                    }

                    let kind =
                        field_kind(name).ok_or_else(|| TemplateParseError::UnknownToken {
                            name: name.to_string(),
                            pos: open,
                        })?;

                    let modifier = match modifier_raw {
                        None => None,
                        Some(raw) => Some(parse_modifier(name, kind, raw, open)?),
                    };

                    if !literal.is_empty() {
                        tokens.push(Token::Literal(std::mem::take(&mut literal)));
                    }
                    tokens.push(Token::Field {
                        name: name.to_string(),
                        modifier,
                    });

                    i = end + 1;
                }
                b'}' => {
                    if bytes.get(i + 1) == Some(&b'}') {
                        literal.push('}');
                        i += 2;
                        continue;
                    }
                    return Err(TemplateParseError::Stray { pos: i });
                }
                _ => {
                    literal.push(s[i..].chars().next().expect("non-empty"));
                    i += s[i..].chars().next().expect("non-empty").len_utf8();
                }
            }
        }

        if !literal.is_empty() {
            tokens.push(Token::Literal(literal));
        }

        Ok(Self { tokens })
    }

    /// Render the template against `ctx`. `{seq}` renders as a width-tagged
    /// sentinel when `ctx.seq.is_none()`; see [`substitute_seq`].
    pub fn render(&self, ctx: &RenderContext<'_>) -> Result<String, RenderError> {
        let mut out = String::new();
        for tok in &self.tokens {
            match tok {
                Token::Literal(s) => out.push_str(s),
                Token::Field { name, modifier } => {
                    render_field(name, modifier.as_ref(), ctx, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for tok in &self.tokens {
            match tok {
                Token::Literal(s) => {
                    // Re-escape braces so the rendered string round-trips.
                    for c in s.chars() {
                        match c {
                            '{' => f.write_str("{{")?,
                            '}' => f.write_str("}}")?,
                            other => f.write_char(other)?,
                        }
                    }
                }
                Token::Field { name, modifier } => match modifier {
                    None => write!(f, "{{{name}}}")?,
                    Some(Modifier::Raw) => write!(f, "{{{name}:raw}}")?,
                    Some(Modifier::Width(n)) => write!(f, "{{{name}:0{n}}}")?,
                    Some(Modifier::HashWidth(n)) => write!(f, "{{{name}:{n}}}")?,
                },
            }
        }
        Ok(())
    }
}

fn parse_modifier(
    name: &str,
    kind: FieldKind,
    raw: &str,
    pos: usize,
) -> Result<Modifier, TemplateParseError> {
    match kind {
        FieldKind::StringMeta => {
            if raw == "raw" {
                Ok(Modifier::Raw)
            } else {
                Err(TemplateParseError::IllegalModifier {
                    name: name.to_string(),
                    modifier: raw.to_string(),
                    pos,
                })
            }
        }
        FieldKind::Seq => {
            let n = parse_uint(raw).ok_or_else(|| TemplateParseError::MalformedModifier {
                name: name.to_string(),
                modifier: raw.to_string(),
                pos,
            })?;
            Ok(Modifier::Width(n))
        }
        FieldKind::Hash => {
            let n = parse_uint(raw).ok_or_else(|| TemplateParseError::MalformedModifier {
                name: name.to_string(),
                modifier: raw.to_string(),
                pos,
            })?;
            Ok(Modifier::HashWidth(n))
        }
        FieldKind::Date | FieldKind::Ext => Err(TemplateParseError::IllegalModifier {
            name: name.to_string(),
            modifier: raw.to_string(),
            pos,
        }),
    }
}

fn parse_uint(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn render_field(
    name: &str,
    modifier: Option<&Modifier>,
    ctx: &RenderContext<'_>,
    out: &mut String,
) -> Result<(), RenderError> {
    let kind = field_kind(name).expect("parse rejects unknown tokens");

    match kind {
        FieldKind::Date => {
            if modifier.is_some() {
                return Err(RenderError::InapplicableModifier {
                    name: name.to_string(),
                });
            }
            render_date(name, ctx.taken_at, out);
            Ok(())
        }
        FieldKind::Ext => {
            if modifier.is_some() {
                return Err(RenderError::InapplicableModifier {
                    name: name.to_string(),
                });
            }
            if let Some(ext) = ctx.canonical_ext {
                out.push_str(ext);
            }
            Ok(())
        }
        FieldKind::Hash => {
            let n = match modifier {
                None => ctx.sha256_hex.len(),
                Some(Modifier::HashWidth(n)) => *n,
                Some(_) => {
                    return Err(RenderError::InapplicableModifier {
                        name: name.to_string(),
                    });
                }
            };
            let take = n.min(ctx.sha256_hex.len());
            out.push_str(&ctx.sha256_hex[..take]);
            Ok(())
        }
        FieldKind::Seq => {
            let width = match modifier {
                None => 0,
                Some(Modifier::Width(n)) => *n,
                Some(_) => {
                    return Err(RenderError::InapplicableModifier {
                        name: name.to_string(),
                    });
                }
            };
            match ctx.seq {
                None => out.push_str(&seq_sentinel(width)),
                Some(n) => {
                    let s = format!("{n:0width$}");
                    out.push_str(&s);
                }
            }
            Ok(())
        }
        FieldKind::StringMeta => {
            let raw_mode = matches!(modifier, Some(Modifier::Raw));
            if let Some(Modifier::Width(_) | Modifier::HashWidth(_)) = modifier {
                return Err(RenderError::InapplicableModifier {
                    name: name.to_string(),
                });
            }
            let value = string_meta_value(name, ctx);
            let resolved = value.unwrap_or_else(|| {
                ctx.fallbacks
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string())
            });
            if raw_mode {
                out.push_str(&resolved);
            } else {
                out.push_str(&slugify(&resolved));
            }
            Ok(())
        }
    }
}

fn render_date(name: &str, dt: &NaiveDateTime, out: &mut String) {
    use std::fmt::Write as _;
    match name {
        "yyyy" => {
            let _ = write!(out, "{:04}", dt.year());
        }
        "yy" => {
            let y = dt.year().rem_euclid(100);
            let _ = write!(out, "{y:02}");
        }
        "mm" => {
            let _ = write!(out, "{:02}", dt.month());
        }
        "dd" => {
            let _ = write!(out, "{:02}", dt.day());
        }
        "hh" => {
            let _ = write!(out, "{:02}", dt.hour());
        }
        "min" => {
            let _ = write!(out, "{:02}", dt.minute());
        }
        "ss" => {
            let _ = write!(out, "{:02}", dt.second());
        }
        _ => unreachable!("field_kind classified this as Date"),
    }
}

fn string_meta_value(name: &str, ctx: &RenderContext<'_>) -> Option<String> {
    match name {
        "camera" => ctx.metadata.camera.clone(),
        "lens" => ctx.metadata.lens.clone(),
        "kind" => Some(ctx.metadata.kind.clone()),
        "author" => ctx.metadata.author.clone(),
        "title" => ctx.metadata.title.clone(),
        "vendor" => ctx.metadata.vendor.clone(),
        _ => unreachable!("field_kind classified this as StringMeta"),
    }
}

/// Lowercase, spaces become `_`, anything outside `[a-z0-9_-]` is stripped.
///
/// ```
/// use shelf::template::slugify;
/// assert_eq!(slugify("Canon EOS R5"), "canon_eos_r5");
/// assert_eq!(slugify("iPhone 15 Pro"), "iphone_15_pro");
/// assert_eq!(slugify("NIKON D750"), "nikon_d750");
/// ```
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == ' ' {
            out.push('_');
            continue;
        }
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' || lower == '-' {
            out.push(lower);
        }
    }
    out
}

use std::fmt::Write as _;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DateSource, Metadata};
    use chrono::NaiveDate;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
    }

    fn md(camera: Option<&str>, lens: Option<&str>, kind: &str) -> Metadata {
        Metadata {
            taken_at: dt(2024, 3, 15, 14, 22, 10),
            taken_at_source: DateSource::Exif,
            camera: camera.map(str::to_string),
            lens: lens.map(str::to_string),
            kind: kind.to_string(),
            width: None,
            height: None,
            author: None,
            title: None,
            vendor: None,
        }
    }

    fn md_doc(author: Option<&str>, title: Option<&str>, vendor: Option<&str>) -> Metadata {
        Metadata {
            taken_at: dt(2024, 3, 15, 14, 22, 10),
            taken_at_source: DateSource::Pdf,
            camera: None,
            lens: None,
            kind: "document".into(),
            width: None,
            height: None,
            author: author.map(str::to_string),
            title: title.map(str::to_string),
            vendor: vendor.map(str::to_string),
        }
    }

    fn empty_fb() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn ctx<'a>(
        when: &'a NaiveDateTime,
        m: &'a Metadata,
        fb: &'a BTreeMap<String, String>,
        seq: Option<u64>,
        ext: Option<&'a str>,
        hash: &'a str,
    ) -> RenderContext<'a> {
        RenderContext {
            taken_at: when,
            metadata: m,
            canonical_ext: ext,
            sha256_hex: hash,
            seq,
            fallbacks: fb,
        }
    }

    #[test]
    fn parses_literal_only() {
        let t = Template::parse("plain/path").unwrap();
        assert_eq!(t.tokens, vec![Token::Literal("plain/path".to_string())]);
    }

    #[test]
    fn parses_simple_field() {
        let t = Template::parse("{yyyy}").unwrap();
        assert_eq!(
            t.tokens,
            vec![Token::Field {
                name: "yyyy".into(),
                modifier: None
            }]
        );
    }

    #[test]
    fn parses_mixed() {
        let t = Template::parse("{yyyy}/{mm}-{dd}_{seq:05}").unwrap();
        assert_eq!(t.tokens.len(), 7);
    }

    #[test]
    fn parses_raw_modifier() {
        let t = Template::parse("{camera:raw}").unwrap();
        assert_eq!(
            t.tokens,
            vec![Token::Field {
                name: "camera".into(),
                modifier: Some(Modifier::Raw)
            }]
        );
    }

    #[test]
    fn parses_hash_width() {
        let t = Template::parse("{hash:8}").unwrap();
        assert_eq!(
            t.tokens,
            vec![Token::Field {
                name: "hash".into(),
                modifier: Some(Modifier::HashWidth(8))
            }]
        );
    }

    #[test]
    fn parse_rejects_unclosed_brace() {
        let e = Template::parse("{yyyy").unwrap_err();
        assert_eq!(e, TemplateParseError::Unclosed { pos: 0 });
    }

    #[test]
    fn parse_rejects_stray_close() {
        let e = Template::parse("oops}").unwrap_err();
        assert_eq!(e, TemplateParseError::Stray { pos: 4 });
    }

    #[test]
    fn parse_rejects_empty_token() {
        let e = Template::parse("{}").unwrap_err();
        assert_eq!(e, TemplateParseError::Empty { pos: 0 });
    }

    #[test]
    fn parse_rejects_empty_modifier() {
        let e = Template::parse("{seq:}").unwrap_err();
        assert_eq!(
            e,
            TemplateParseError::EmptyModifier {
                name: "seq".into(),
                pos: 0
            }
        );
    }

    #[test]
    fn parse_rejects_unknown_token() {
        let e = Template::parse("{nope}").unwrap_err();
        assert_eq!(
            e,
            TemplateParseError::UnknownToken {
                name: "nope".into(),
                pos: 0
            }
        );
    }

    #[test]
    fn parse_rejects_raw_on_date_token() {
        let e = Template::parse("{yyyy:raw}").unwrap_err();
        assert!(matches!(
            e,
            TemplateParseError::IllegalModifier { ref name, .. } if name == "yyyy"
        ));
    }

    #[test]
    fn parse_rejects_width_on_camera() {
        let e = Template::parse("{camera:05}").unwrap_err();
        assert!(matches!(
            e,
            TemplateParseError::IllegalModifier { ref name, .. } if name == "camera"
        ));
    }

    #[test]
    fn parse_rejects_malformed_seq_width() {
        let e = Template::parse("{seq:abc}").unwrap_err();
        assert!(matches!(
            e,
            TemplateParseError::MalformedModifier { ref name, .. } if name == "seq"
        ));
    }

    #[test]
    fn parse_handles_escaped_braces() {
        let t = Template::parse("{{lit}} {yyyy}").unwrap();
        let mut iter = t.tokens.iter();
        match iter.next().unwrap() {
            Token::Literal(s) => assert_eq!(s, "{lit} "),
            other => panic!("expected literal, got {other:?}"),
        }
        match iter.next().unwrap() {
            Token::Field { name, .. } => assert_eq!(name, "yyyy"),
            other => panic!("expected field, got {other:?}"),
        }
    }

    #[test]
    fn renders_library_directory_example() {
        let m = md(Some("Canon EOS R5"), None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{yyyy}/{mm}").unwrap();
        let when = dt(2024, 3, 15, 14, 22, 10);
        let c = ctx(&when, &m, &fb, Some(42), Some("jpg"), "abc");
        assert_eq!(t.render(&c).unwrap(), "2024/03");
    }

    #[test]
    fn renders_library_filename_example() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{yyyy}-{mm}-{dd}_{seq:05}").unwrap();
        let when = dt(2024, 3, 15, 14, 22, 10);
        let c = ctx(&when, &m, &fb, Some(42), None, "");
        assert_eq!(t.render(&c).unwrap(), "2024-03-15_00042");
    }

    #[test]
    fn end_to_end_library_render() {
        let m = md(Some("Canon EOS R5"), None, "photo");
        let fb = empty_fb();
        let dir = Template::parse("{yyyy}/{mm}").unwrap();
        let name = Template::parse("{yyyy}-{mm}-{dd}_{seq:05}").unwrap();
        let when = dt(2024, 3, 15, 14, 22, 10);
        let c = ctx(&when, &m, &fb, Some(42), Some("jpg"), "deadbeef");
        let combined = format!("{}/{}", dir.render(&c).unwrap(), name.render(&c).unwrap());
        assert_eq!(combined, "2024/03/2024-03-15_00042");
    }

    #[test]
    fn renders_all_date_tokens() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let when = dt(2024, 3, 15, 14, 22, 10);
        let t = Template::parse("{yyyy}-{yy}-{mm}-{dd}T{hh}:{min}:{ss}").unwrap();
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "2024-24-03-15T14:22:10");
    }

    #[test]
    fn camera_default_slugifies() {
        let m = md(Some("Canon EOS R5"), None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{camera}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "canon_eos_r5");
    }

    #[test]
    fn camera_raw_keeps_original() {
        let m = md(Some("Canon EOS R5"), None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{camera:raw}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "Canon EOS R5");
    }

    #[test]
    fn missing_token_without_fallback_is_unknown() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{camera}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "unknown");
    }

    #[test]
    fn missing_token_with_fallback_is_slugified_fallback() {
        let m = md(None, None, "photo");
        let mut fb = empty_fb();
        fb.insert("camera".into(), "Unknown Camera".into());
        let t = Template::parse("{camera}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "unknown_camera");
    }

    #[test]
    fn missing_token_with_fallback_raw_keeps_original() {
        let m = md(None, None, "photo");
        let mut fb = empty_fb();
        fb.insert("camera".into(), "Unknown Camera".into());
        let t = Template::parse("{camera:raw}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "Unknown Camera");
    }

    #[test]
    fn author_token_uses_metadata_when_present() {
        let m = md_doc(Some("Jane Doe"), None, None);
        let fb = empty_fb();
        let t = Template::parse("{author}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "jane_doe");
    }

    #[test]
    fn author_token_falls_back_when_metadata_missing() {
        let m = md(None, None, "document");
        let mut fb = empty_fb();
        fb.insert("author".into(), "unknown_vendor".into());
        let t = Template::parse("{author}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "unknown_vendor");
    }

    #[test]
    fn title_and_vendor_render_from_metadata() {
        let m = md_doc(Some("Jane Doe"), Some("Invoice March"), Some("Acme Corp"));
        let fb = empty_fb();
        let t = Template::parse("{title}/{vendor:raw}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), "invoice_march/Acme Corp");
    }

    #[test]
    fn hash_with_no_modifier_is_full() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{hash}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "abcdef0123");
        assert_eq!(t.render(&c).unwrap(), "abcdef0123");
    }

    #[test]
    fn hash_with_width_truncates() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{hash:8}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "abcdef0123456789");
        assert_eq!(t.render(&c).unwrap(), "abcdef01");
    }

    #[test]
    fn ext_renders_when_present() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{ext}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, Some("jpg"), "");
        assert_eq!(t.render(&c).unwrap(), "jpg");
    }

    #[test]
    fn seq_with_assigned_value_pads() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{seq:05}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, Some(42), None, "");
        assert_eq!(t.render(&c).unwrap(), "00042");
    }

    #[test]
    fn seq_without_value_renders_width_tagged_sentinel() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{seq:05}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), seq_sentinel(5));
    }

    #[test]
    fn seq_no_modifier_renders_bare_number() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{seq}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, Some(7), None, "");
        assert_eq!(t.render(&c).unwrap(), "7");
    }

    #[test]
    fn seq_no_modifier_no_value_is_zero_width_sentinel() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{seq}").unwrap();
        let when = dt(2024, 1, 1, 0, 0, 0);
        let c = ctx(&when, &m, &fb, None, None, "");
        assert_eq!(t.render(&c).unwrap(), seq_sentinel(0));
    }

    #[test]
    fn substitute_seq_replaces_padded_sentinel() {
        let s = format!("2024-03-15_{}", seq_sentinel(5));
        assert_eq!(substitute_seq(&s, 42).unwrap(), "2024-03-15_00042");
    }

    #[test]
    fn substitute_seq_replaces_unpadded_sentinel() {
        let s = format!("img-{}", seq_sentinel(0));
        assert_eq!(substitute_seq(&s, 7).unwrap(), "img-7");
    }

    #[test]
    fn substitute_seq_passes_through_strings_without_sentinel() {
        assert_eq!(substitute_seq("no-seq-here", 99).unwrap(), "no-seq-here");
    }

    #[test]
    fn substitute_seq_handles_multiple_sentinels_with_same_width() {
        let s = format!("{}/{}", seq_sentinel(3), seq_sentinel(3));
        assert_eq!(substitute_seq(&s, 5).unwrap(), "005/005");
    }

    #[test]
    fn substitute_seq_renders_through_template() {
        let m = md(None, None, "photo");
        let fb = empty_fb();
        let t = Template::parse("{yyyy}-{mm}-{dd}_{seq:05}").unwrap();
        let when = dt(2024, 3, 15, 14, 22, 10);
        let c = ctx(&when, &m, &fb, None, None, "");
        let rendered = t.render(&c).unwrap();
        assert_eq!(substitute_seq(&rendered, 1).unwrap(), "2024-03-15_00001");
    }

    #[test]
    fn slugify_documented_cases() {
        assert_eq!(slugify("Canon EOS R5"), "canon_eos_r5");
        assert_eq!(slugify("iPhone 15 Pro"), "iphone_15_pro");
        assert_eq!(slugify("NIKON D750"), "nikon_d750");
    }

    #[test]
    fn slugify_strips_punctuation() {
        assert_eq!(slugify("Sony A7R-IV!!"), "sony_a7r-iv");
        assert_eq!(
            slugify("RF 24-70mm F2.8 L IS USM"),
            "rf_24-70mm_f28_l_is_usm"
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::metadata::{DateSource, Metadata};
    use chrono::NaiveDate;
    use proptest::prelude::*;

    fn md() -> Metadata {
        Metadata {
            taken_at: NaiveDate::from_ymd_opt(2024, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            taken_at_source: DateSource::Exif,
            camera: None,
            lens: None,
            kind: "photo".into(),
            width: None,
            height: None,
            author: None,
            title: None,
            vendor: None,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn seq_width_pads_to_at_least_width(width in 1usize..=20, value in any::<u64>()) {
            let tpl_str = format!("{{seq:{width:02}}}");
            let t = Template::parse(&tpl_str).unwrap();
            let m = md();
            let fb = BTreeMap::new();
            let when = m.taken_at;
            let c = RenderContext {
                taken_at: &when,
                metadata: &m,
                canonical_ext: None,
                sha256_hex: "",
                seq: Some(value),
                fallbacks: &fb,
            };
            let out = t.render(&c).unwrap();
            prop_assert!(out.len() >= width, "expected >= {width} chars, got {out:?}");
        }

        #[test]
        fn substitute_seq_pads_to_width(width in 0usize..=20, value in any::<u64>()) {
            let s = format!("x{}y", seq_sentinel(width));
            let out = substitute_seq(&s, value).unwrap();
            prop_assert!(out.starts_with('x') && out.ends_with('y'));
            let middle = &out[1..out.len() - 1];
            prop_assert!(middle.len() >= width, "expected >= {width} chars, got {middle:?}");
        }
    }
}
