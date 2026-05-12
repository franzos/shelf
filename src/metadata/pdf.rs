//! PDF metadata extractor.
//!
//! Reads the PDF `Info` dictionary via the trailer. Keys read:
//! `/CreationDate`, `/ModDate` (PDF date format `D:YYYYMMDDHHmmSSOHH'mm'`,
//! offset dropped → `NaiveDateTime`); `/Author`, `/Title`, `/Producer`
//! (UTF-16BE or PDFDocEncoding strings).
//!
//! `try_vendor` returns `/Author`, falling back to `/Producer` for scanned
//! PDFs with no Author tag.
//!
//! Parser is hand-rolled (vs `lopdf`/`pdf`) because the public dicts we
//! need are a single dictionary lookup — ~250 LoC, no extra dependency.
//! XMP metadata streams are not parsed; deferred until a real PDF turns
//! up that only carries XMP (major office suites still populate `/Info`).

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chrono::{NaiveDate, NaiveDateTime};

use super::Extractor;
use crate::error::{Error, Result};

/// Hard ceiling on bytes read from the tail when locating `startxref`/trailer.
/// 64KiB covers linearization hints and large /Info dicts without letting a
/// hostile file pull in megabytes.
const TAIL_WINDOW_BYTES: u64 = 64 * 1024;

/// Hard ceiling on the linear scan past `startxref` when locating the
/// trailer dictionary (PDF 1.5+ xref streams can put the trailer inside an
/// object stream).
const XREF_SCAN_BYTES: u64 = 1024 * 1024;

#[derive(Default, Clone)]
struct PdfInfo {
    creation_date: Option<NaiveDateTime>,
    mod_date: Option<NaiveDateTime>,
    author: Option<String>,
    title: Option<String>,
    producer: Option<String>,
}

#[derive(Default)]
pub struct PdfExtractor {
    cache: RefCell<Option<(PathBuf, Option<Rc<PdfInfo>>)>>,
}

impl PdfExtractor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read_cached(&self, path: &Path) -> Result<Option<Rc<PdfInfo>>> {
        if let Some((cached_path, cached)) = self.cache.borrow().as_ref()
            && cached_path == path
        {
            return Ok(cached.clone());
        }
        let parsed = read_pdf_info(path)?.map(Rc::new);
        *self.cache.borrow_mut() = Some((path.to_path_buf(), parsed.clone()));
        Ok(parsed)
    }
}

impl Extractor for PdfExtractor {
    fn id(&self) -> &'static str {
        "pdf"
    }

    fn try_date(&self, path: &Path, _kind: &str) -> Result<Option<NaiveDateTime>> {
        let Some(info) = self.read_cached(path)? else {
            return Ok(None);
        };
        Ok(info.creation_date.or(info.mod_date))
    }

    fn try_author(&self, path: &Path, _kind: &str) -> Result<Option<String>> {
        let Some(info) = self.read_cached(path)? else {
            return Ok(None);
        };
        Ok(info.author.clone())
    }

    fn try_title(&self, path: &Path, _kind: &str) -> Result<Option<String>> {
        let Some(info) = self.read_cached(path)? else {
            return Ok(None);
        };
        Ok(info.title.clone())
    }

    fn try_vendor(&self, path: &Path, _kind: &str) -> Result<Option<String>> {
        let Some(info) = self.read_cached(path)? else {
            return Ok(None);
        };
        // Prefer /Author; fall back to /Producer for scanned invoices
        // that have no Author tag.
        Ok(info.author.clone().or_else(|| info.producer.clone()))
    }
}

fn read_pdf_info(path: &Path) -> Result<Option<PdfInfo>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(source) => {
            return Err(Error::MetadataIo {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if !looks_like_pdf(&bytes) {
        return Ok(None);
    }

    // Walk back from EOF for the trailer dict. Two layouts:
    //   1. Classic PDF: `trailer\n<< /Size N /Info N M R >>\nstartxref ...`
    //   2. PDF 1.5+ xref stream: trailer dict lives inline before `stream`.
    let tail_start = bytes.len().saturating_sub(TAIL_WINDOW_BYTES as usize);
    let tail = &bytes[tail_start..];

    let trailer_dict = match find_trailer_dict(tail) {
        Some(d) => d,
        None => return Ok(None),
    };

    let info_ref = match find_info_ref(trailer_dict) {
        Some(r) => r,
        None => return Ok(None),
    };

    let info_dict = match locate_object_dict(&bytes, info_ref, path) {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(None),
        Err(e) => return Err(e),
    };

    Ok(Some(parse_info_dict(info_dict)))
}

fn looks_like_pdf(bytes: &[u8]) -> bool {
    // %PDF- header must be in the first 1024 bytes per ISO 32000 §7.5.2.
    let head = &bytes[..bytes.len().min(1024)];
    head.windows(5).any(|w| w == b"%PDF-")
}

/// Locate the trailer dictionary body. Handles both the classic
/// `trailer\n<<` form and the inline xref-stream form.
fn find_trailer_dict(tail: &[u8]) -> Option<&[u8]> {
    if let Some(pos) = rfind(tail, b"trailer") {
        let after = &tail[pos + b"trailer".len()..];
        let dict_start = after.iter().position(|&b| b == b'<')?;
        let dict_bytes = &after[dict_start..];
        return extract_dict_body(dict_bytes);
    }

    // No `trailer` keyword: xref-stream PDF. Find the last `startxref` and
    // walk back to the most recent `<<`.
    let sx = rfind(tail, b"startxref")?;
    let before = &tail[..sx];
    let dict_open = rfind(before, b"<<")?;
    extract_dict_body(&tail[dict_open..])
}

/// Given a slice that begins with `<<`, return the bytes between `<<` and
/// the matching `>>` (handles nesting).
fn extract_dict_body(s: &[u8]) -> Option<&[u8]> {
    if !s.starts_with(b"<<") {
        return None;
    }
    let mut depth = 0i32;
    let mut i = 0;
    let start = 2;
    while i + 1 < s.len() {
        match (s[i], s[i + 1]) {
            (b'<', b'<') => {
                depth += 1;
                i += 2;
            }
            (b'>', b'>') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..i]);
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    None
}

/// Find the `/Info N M R` reference in the trailer dictionary.
fn find_info_ref(dict: &[u8]) -> Option<(u64, u64)> {
    let key = b"/Info";
    let mut i = 0;
    while let Some(at) = find_from(dict, i, key) {
        // Require a non-name-character break so we don't match `/InfoDict`.
        let after_idx = at + key.len();
        let next = dict.get(after_idx).copied();
        if matches!(next, Some(b) if is_name_char(b)) {
            i = at + 1;
            continue;
        }
        let rest = &dict[after_idx..];
        if let Some(r) = parse_indirect_ref(rest) {
            return Some(r);
        }
        i = at + 1;
    }
    None
}

fn parse_indirect_ref(s: &[u8]) -> Option<(u64, u64)> {
    let s = trim_left(s);
    let (n, rest) = parse_uint(s)?;
    let rest = trim_left(rest);
    let (m, rest) = parse_uint(rest)?;
    let rest = trim_left(rest);
    if rest.first().copied() == Some(b'R') {
        Some((n, m))
    } else {
        None
    }
}

/// Find the object with the given `(N, M)` id and return its dictionary
/// body. Scans for `N M obj` then the first `<<`; doesn't honor the xref
/// table, but the `/Info` target is always a regular indirect object in
/// well-formed PDFs.
fn locate_object_dict<'a>(
    bytes: &'a [u8],
    (n, m): (u64, u64),
    path: &Path,
) -> Result<Option<&'a [u8]>> {
    let needle = format!("{n} {m} obj");
    let Some(pos) = find_from(bytes, 0, needle.as_bytes()) else {
        return Ok(None);
    };
    let after = &bytes[pos + needle.len()..];
    let cap = after.len().min(XREF_SCAN_BYTES as usize);
    let window = &after[..cap];
    let Some(open) = window.iter().position(|&b| b == b'<') else {
        return Ok(None);
    };
    let dict_bytes = &window[open..];
    if !dict_bytes.starts_with(b"<<") {
        return Err(Error::PdfParse {
            path: path.to_path_buf(),
            reason: format!("object {n} {m} R has no dictionary body"),
        });
    }
    Ok(extract_dict_body(dict_bytes))
}

fn parse_info_dict(dict: &[u8]) -> PdfInfo {
    let mut info = PdfInfo::default();
    for (key, value) in DictIter::new(dict) {
        match key.as_slice() {
            b"CreationDate" => info.creation_date = decode_string(&value).and_then(parse_pdf_date),
            b"ModDate" => info.mod_date = decode_string(&value).and_then(parse_pdf_date),
            b"Author" => info.author = decode_string(&value).filter(|s| !s.is_empty()),
            b"Title" => info.title = decode_string(&value).filter(|s| !s.is_empty()),
            b"Producer" => info.producer = decode_string(&value).filter(|s| !s.is_empty()),
            _ => {}
        }
    }
    info
}

/// Iterator over `(key, value_bytes)` pairs in a dictionary body. Keys are
/// PDF names (`/Foo` → `Foo`); values are the raw bytes up to the next key
/// or end-of-dict.
struct DictIter<'a> {
    rest: &'a [u8],
}

impl<'a> DictIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { rest: buf }
    }
}

impl<'a> Iterator for DictIter<'a> {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        let slash = self.rest.iter().position(|&b| b == b'/')?;
        let after_slash = &self.rest[slash + 1..];
        let key_end = after_slash
            .iter()
            .position(|&b| !is_name_char(b))
            .unwrap_or(after_slash.len());
        let key = after_slash[..key_end].to_vec();
        let after_key = &after_slash[key_end..];

        let val_end = scan_value_end(after_key);
        let value = after_key[..val_end].to_vec();
        self.rest = &after_key[val_end..];
        Some((key, value))
    }
}

/// Locate the end of the current value: stops at the next top-level `/`,
/// at end-of-input, or at the closing `>>` of the enclosing dict. Skips
/// nested `<<...>>` and `(...)` literal strings.
fn scan_value_end(s: &[u8]) -> usize {
    let mut depth = 0i32;
    let mut paren = 0i32;
    let mut i = 0;
    while i < s.len() {
        if paren > 0 {
            match s[i] {
                b'\\' if i + 1 < s.len() => i += 2,
                b'(' => {
                    paren += 1;
                    i += 1;
                }
                b')' => {
                    paren -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        if depth == 0 && s[i] == b'/' {
            return i;
        }
        if i + 1 < s.len() && s[i] == b'<' && s[i + 1] == b'<' {
            depth += 1;
            i += 2;
            continue;
        }
        if i + 1 < s.len() && s[i] == b'>' && s[i + 1] == b'>' {
            if depth == 0 {
                return i;
            }
            depth -= 1;
            i += 2;
            continue;
        }
        if s[i] == b'(' {
            paren += 1;
            i += 1;
            continue;
        }
        i += 1;
    }
    s.len()
}

/// Decode a PDF string value. Handles `(literal)` and `<hex>` forms; the
/// literal form may be UTF-16BE with a `\xfe\xff` BOM.
fn decode_string(value: &[u8]) -> Option<String> {
    let v = trim(value);
    if v.starts_with(b"(") && v.ends_with(b")") && v.len() >= 2 {
        let body = &v[1..v.len() - 1];
        let decoded = decode_literal_escapes(body);
        return Some(pdf_bytes_to_string(&decoded));
    }
    if v.starts_with(b"<") && v.ends_with(b">") && v.len() >= 2 {
        // Single-`<` ... `>` hex string. Two-`<<` was already consumed
        // by the dict body extractor.
        if v.starts_with(b"<<") {
            return None;
        }
        let hex_body = &v[1..v.len() - 1];
        let raw = decode_hex_string(hex_body)?;
        return Some(pdf_bytes_to_string(&raw));
    }
    None
}

fn decode_literal_escapes(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\\' && i + 1 < body.len() {
            let c = body[i + 1];
            match c {
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'b' => {
                    out.push(0x08);
                    i += 2;
                }
                b'f' => {
                    out.push(0x0c);
                    i += 2;
                }
                b'(' | b')' | b'\\' => {
                    out.push(c);
                    i += 2;
                }
                b'\n' => i += 2,
                b'0'..=b'7' => {
                    // Up to three octal digits.
                    let mut j = i + 1;
                    let end = (i + 4).min(body.len());
                    let mut val: u32 = 0;
                    while j < end && (b'0'..=b'7').contains(&body[j]) {
                        val = val * 8 + u32::from(body[j] - b'0');
                        j += 1;
                    }
                    out.push((val & 0xff) as u8);
                    i = j;
                }
                _ => {
                    out.push(c);
                    i += 2;
                }
            }
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    out
}

fn decode_hex_string(body: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(body.len() / 2);
    let mut nibble: Option<u8> = None;
    for &b in body {
        if b.is_ascii_whitespace() {
            continue;
        }
        let n = hex_nibble(b)?;
        match nibble {
            None => nibble = Some(n),
            Some(hi) => {
                bytes.push((hi << 4) | n);
                nibble = None;
            }
        }
    }
    // Trailing odd nibble: spec says treat as if followed by 0.
    if let Some(hi) = nibble {
        bytes.push(hi << 4);
    }
    Some(bytes)
}

/// Convert PDF string bytes to a Rust `String`. UTF-16BE with BOM is
/// honored; everything else is interpreted as PDFDocEncoding (a superset
/// of ASCII for our purposes).
fn pdf_bytes_to_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xfe && bytes[1] == 0xff {
        let body = &bytes[2..];
        let mut units = Vec::with_capacity(body.len() / 2);
        let mut i = 0;
        while i + 1 < body.len() {
            units.push(u16::from_be_bytes([body[i], body[i + 1]]));
            i += 2;
        }
        return String::from_utf16_lossy(&units);
    }
    if bytes.len() >= 3 && bytes[0] == 0xef && bytes[1] == 0xbb && bytes[2] == 0xbf {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Parse a PDF date string: `D:YYYYMMDDHHmmSSOHH'mm'`. Everything past the
/// year is optional and gets zero-filled. Timezone block is parsed but
/// discarded — naive local-clock matches the rest of the codebase.
fn parse_pdf_date(s: String) -> Option<NaiveDateTime> {
    let s = s.trim();
    let body = s.strip_prefix("D:").unwrap_or(s);
    let bytes = body.as_bytes();

    let two = |at: usize, default: u32| -> Option<u32> {
        if at + 2 > bytes.len() {
            return Some(default);
        }
        let chunk = &bytes[at..at + 2];
        if chunk.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(chunk).ok()?.parse().ok()
        } else {
            None
        }
    };

    if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year: i32 = std::str::from_utf8(&bytes[..4]).ok()?.parse().ok()?;
    let month = two(4, 1)?;
    let day = two(6, 1)?;
    let hour = two(8, 0)?;
    let minute = two(10, 0)?;
    let second = two(12, 0)?;

    NaiveDate::from_ymd_opt(year, month.max(1), day.max(1))?.and_hms_opt(hour, minute, second)
}

fn is_name_char(b: u8) -> bool {
    // PDF names: any printable ASCII that isn't a delimiter or whitespace.
    matches!(b,
        b'0'..=b'9'
        | b'A'..=b'Z'
        | b'a'..=b'z'
        | b'_' | b'-' | b'.' | b'+'
    )
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00)
}

fn trim_left(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| !is_whitespace(b)).unwrap_or(s.len());
    &s[start..]
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| !is_whitespace(b)).unwrap_or(s.len());
    let inner = &s[start..];
    let end = inner
        .iter()
        .rposition(|&b| !is_whitespace(b))
        .map_or(0, |p| p + 1);
    &inner[..end]
}

fn parse_uint(s: &[u8]) -> Option<(u64, &[u8])> {
    let end = s
        .iter()
        .position(|&b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let n = std::str::from_utf8(&s[..end]).ok()?.parse().ok()?;
    Some((n, &s[end..]))
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .rev()
        .find(|&i| &hay[i..i + needle.len()] == needle)
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    //! Minimal synthetic PDF builder for tests.

    use std::fmt::Write as _;

    pub fn make_pdf(
        creation_date: Option<&str>,
        author: Option<&str>,
        title: Option<&str>,
        producer: Option<&str>,
    ) -> Vec<u8> {
        let mut info_body = String::new();
        if let Some(d) = creation_date {
            write!(info_body, "/CreationDate ({d}) ").unwrap();
        }
        if let Some(a) = author {
            write!(info_body, "/Author ({a}) ").unwrap();
        }
        if let Some(t) = title {
            write!(info_body, "/Title ({t}) ").unwrap();
        }
        if let Some(p) = producer {
            write!(info_body, "/Producer ({p}) ").unwrap();
        }

        let obj1 = "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let obj2 = "2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n";
        let obj3 = format!("3 0 obj\n<< {info_body}>>\nendobj\n");

        let header = "%PDF-1.4\n%\u{e2}\u{e3}\u{cf}\u{d3}\n";
        let mut body = String::new();
        body.push_str(header);
        let offset1 = body.len();
        body.push_str(obj1);
        let offset2 = body.len();
        body.push_str(obj2);
        let offset3 = body.len();
        body.push_str(&obj3);

        let xref_offset = body.len();
        write!(
            body,
            "xref\n0 4\n0000000000 65535 f \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n",
            offset1, offset2, offset3
        )
        .unwrap();
        write!(
            body,
            "trailer\n<< /Size 4 /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
        )
        .unwrap();

        body.into_bytes()
    }

    pub fn make_pdf_no_info() -> Vec<u8> {
        let obj1 = "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let obj2 = "2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n";

        let header = "%PDF-1.4\n%\u{e2}\u{e3}\u{cf}\u{d3}\n";
        let mut body = String::new();
        body.push_str(header);
        let offset1 = body.len();
        body.push_str(obj1);
        let offset2 = body.len();
        body.push_str(obj2);

        let xref_offset = body.len();
        write!(
            body,
            "xref\n0 3\n0000000000 65535 f \n{:010} 00000 n \n{:010} 00000 n \n",
            offset1, offset2
        )
        .unwrap();
        write!(
            body,
            "trailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
        )
        .unwrap();

        body.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{make_pdf, make_pdf_no_info};
    use super::*;
    use chrono::NaiveDate;
    use tempfile::NamedTempFile;

    fn write_temp(bytes: &[u8]) -> NamedTempFile {
        let mut tmp = NamedTempFile::new().unwrap();
        std::io::Write::write_all(tmp.as_file_mut(), bytes).unwrap();
        tmp
    }

    #[test]
    fn parse_pdf_date_full() {
        let dt = parse_pdf_date("D:20240315142210Z".to_string()).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn parse_pdf_date_with_offset() {
        let dt = parse_pdf_date("D:20240315142210-07'00'".to_string()).unwrap();
        // Offset is dropped; wall-clock components are kept.
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn parse_pdf_date_date_only() {
        let dt = parse_pdf_date("D:20240315".to_string()).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn parse_pdf_date_rejects_garbage() {
        assert!(parse_pdf_date("not a date".to_string()).is_none());
        assert!(parse_pdf_date("".to_string()).is_none());
    }

    #[test]
    fn try_date_reads_creation_date_from_fixture() {
        let bytes = make_pdf(
            Some("D:20240315142210Z"),
            Some("Jane Doe"),
            Some("Invoice March"),
            Some("LibreOffice 24.2"),
        );
        let tmp = write_temp(&bytes);
        let dt = PdfExtractor::new()
            .try_date(tmp.path(), "document")
            .unwrap()
            .unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn try_author_and_title_from_full_info() {
        let bytes = make_pdf(
            Some("D:20240315142210Z"),
            Some("Jane Doe"),
            Some("Invoice March"),
            Some("LibreOffice 24.2"),
        );
        let tmp = write_temp(&bytes);
        let ex = PdfExtractor::new();
        assert_eq!(
            ex.try_author(tmp.path(), "document").unwrap().as_deref(),
            Some("Jane Doe")
        );
        assert_eq!(
            ex.try_title(tmp.path(), "document").unwrap().as_deref(),
            Some("Invoice March")
        );
    }

    #[test]
    fn try_vendor_prefers_author_falls_back_to_producer() {
        let with_author = make_pdf(
            Some("D:20240315142210Z"),
            Some("Acme Corp"),
            None,
            Some("LibreOffice"),
        );
        let tmp = write_temp(&with_author);
        assert_eq!(
            PdfExtractor::new()
                .try_vendor(tmp.path(), "document")
                .unwrap()
                .as_deref(),
            Some("Acme Corp")
        );

        let no_author = make_pdf(Some("D:20240315142210Z"), None, None, Some("Scanner X"));
        let tmp = write_temp(&no_author);
        assert_eq!(
            PdfExtractor::new()
                .try_vendor(tmp.path(), "document")
                .unwrap()
                .as_deref(),
            Some("Scanner X")
        );
    }

    #[test]
    fn try_date_on_pdf_without_info_returns_none() {
        let bytes = make_pdf_no_info();
        let tmp = write_temp(&bytes);
        let out = PdfExtractor::new()
            .try_date(tmp.path(), "document")
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn try_date_on_non_pdf_returns_ok_none() {
        let tmp = write_temp(&[0xFF, 0xD8, 0xFF, 0xD9]);
        let out = PdfExtractor::new().try_date(tmp.path(), "photo").unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn try_author_on_non_pdf_returns_ok_none() {
        let tmp = write_temp(b"definitely not a pdf");
        let out = PdfExtractor::new()
            .try_author(tmp.path(), "document")
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn cache_returns_consistent_values_across_calls() {
        let bytes = make_pdf(
            Some("D:20240315142210Z"),
            Some("Jane Doe"),
            Some("Invoice March"),
            None,
        );
        let tmp = write_temp(&bytes);
        let ex = PdfExtractor::new();
        let dt = ex.try_date(tmp.path(), "document").unwrap().unwrap();
        let author = ex.try_author(tmp.path(), "document").unwrap();
        let title = ex.try_title(tmp.path(), "document").unwrap();
        assert_eq!(
            dt,
            NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(14, 22, 10)
                .unwrap()
        );
        assert_eq!(author.as_deref(), Some("Jane Doe"));
        assert_eq!(title.as_deref(), Some("Invoice March"));
    }

    #[test]
    fn decode_string_handles_utf16be_bom() {
        // (\xfe\xff\x00H\x00i) — UTF-16BE "Hi" inside a literal string.
        let raw = b"(\xfe\xff\x00H\x00i)";
        let out = decode_string(raw).unwrap();
        assert_eq!(out, "Hi");
    }

    #[test]
    fn decode_string_handles_hex_string() {
        let raw = b"<48 69>"; // "Hi"
        let out = decode_string(raw).unwrap();
        assert_eq!(out, "Hi");
    }
}
