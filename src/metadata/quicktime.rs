//! QuickTime / MP4 / MOV metadata extractor.
//!
//! Capture time comes from two distinct places:
//!
//! 1. **`com.apple.quicktime.creationdate`** — ISO 8601 string in the
//!    `mdta` block under `moov.udta.meta`. Apple writer pipeline, with
//!    explicit timezone offset.
//! 2. **`mvhd.creation_time`** — ISO/IEC 14496-12 field, seconds since
//!    1904-01-01 UTC ("Mac epoch"). By spec UTC, but older recorders
//!    sometimes write local-time-as-UTC.
//!
//! Priority is (1) → (2). When `creationdate` is present it's more
//! accurate. Both normalize to a [`NaiveDateTime`].
//!
//! Dimensions read from the first track whose `hdlr` is `vide`, via
//! `tkhd` width/height (16.16 fixed-point, truncated).
//!
//! Hand-rolled rather than backed by `mp4parse` or `mp4` because the
//! published versions don't expose `mvhd.creation_time` or hide
//! `udta.meta.mdta` behind `pub(crate)`. ISO BMFF boxes are simple
//! enough — `[u32 size][4-byte fourcc][payload]` — that a focused
//! reader fits in a few hundred lines.

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chrono::{DateTime, NaiveDateTime};

use super::Extractor;
use crate::error::{Error, Result};

/// Seconds between 1904-01-01 (Mac epoch) and 1970-01-01 (Unix epoch).
const MAC_EPOCH_TO_UNIX: i64 = 2_082_844_800;

/// Hard ceiling on the moov-region bytes we'll buffer before bailing.
const MAX_MOOV_BYTES: u64 = 64 * 1024 * 1024;

struct QtMetadata {
    creation_date_iso: Option<NaiveDateTime>,
    /// `mvhd.creation_time` as UTC-as-naive. `None` for spec sentinel `0`.
    mvhd_creation: Option<NaiveDateTime>,
    make: Option<String>,
    model: Option<String>,
    dimensions: Option<(u32, u32)>,
}

#[derive(Default)]
pub struct QuicktimeExtractor {
    cache: RefCell<Option<(PathBuf, Option<Rc<QtMetadata>>)>>,
}

impl QuicktimeExtractor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read_cached(&self, path: &Path) -> Result<Option<Rc<QtMetadata>>> {
        if let Some((cached_path, cached)) = self.cache.borrow().as_ref()
            && cached_path == path
        {
            return Ok(cached.clone());
        }
        let parsed = read_quicktime(path)?.map(Rc::new);
        *self.cache.borrow_mut() = Some((path.to_path_buf(), parsed.clone()));
        Ok(parsed)
    }
}

impl Extractor for QuicktimeExtractor {
    fn id(&self) -> &'static str {
        "quicktime"
    }

    fn try_date(&self, path: &Path, _kind: &str) -> Result<Option<NaiveDateTime>> {
        let Some(qt) = self.read_cached(path)? else {
            return Ok(None);
        };
        Ok(qt.creation_date_iso.or(qt.mvhd_creation))
    }

    fn try_camera(&self, path: &Path, _kind: &str) -> Result<Option<String>> {
        let Some(qt) = self.read_cached(path)? else {
            return Ok(None);
        };
        Ok(combine_make_model(qt.make.clone(), qt.model.clone()))
    }

    fn try_dimensions(&self, path: &Path, _kind: &str) -> Result<Option<(u32, u32)>> {
        let Some(qt) = self.read_cached(path)? else {
            return Ok(None);
        };
        Ok(qt.dimensions)
    }
}

/// Probe whether `path` is a structurally complete MP4/MOV — has both a
/// top-level `ftyp` and `moov` box.
///
/// Returns `Ok(false)` for empty/short files, missing `ftyp`, or `moov`
/// declared size running past EOF (truncation). I/O errors propagate;
/// malformed-but-readable files are reported as `Ok(false)`.
pub fn container_ok(path: &Path) -> Result<bool> {
    let file = File::open(path).map_err(|source| Error::MetadataIo {
        path: path.to_path_buf(),
        source,
    })?;
    let size = file
        .metadata()
        .map_err(|source| Error::MetadataIo {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    let mut reader = BufReader::new(file);

    let mut saw_ftyp = false;
    let mut saw_moov = false;

    let mut pos: u64 = 0;
    while pos < size {
        let header = match read_box_header(&mut reader, size - pos) {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(_) => return Ok(false),
        };
        let (name, body_size, header_len) = header;
        pos += u64::from(header_len);
        if pos.saturating_add(body_size) > size {
            // Declared box body runs past EOF — truncation signature.
            return Ok(false);
        }
        match &name {
            b"ftyp" => saw_ftyp = true,
            b"moov" => saw_moov = true,
            _ => {}
        }
        if saw_ftyp && saw_moov {
            return Ok(true);
        }
        if let Err(e) = skip(&mut reader, body_size) {
            if e.kind() == std::io::ErrorKind::InvalidData {
                return Ok(false);
            }
            return Err(Error::MetadataIo {
                path: path.to_path_buf(),
                source: e,
            });
        }
        pos += body_size;
    }

    Ok(saw_ftyp && saw_moov)
}

fn read_quicktime(path: &Path) -> Result<Option<QtMetadata>> {
    let file = File::open(path).map_err(|source| Error::MetadataIo {
        path: path.to_path_buf(),
        source,
    })?;
    let size = file
        .metadata()
        .map_err(|source| Error::MetadataIo {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    let mut reader = BufReader::new(file);

    let mut saw_ftyp = false;
    let mut moov: Option<Vec<u8>> = None;

    let mut pos: u64 = 0;
    while pos < size {
        let Some((name, body_size, header_len)) = read_box_header(&mut reader, size - pos)
            .map_err(|source| Error::MetadataIo {
                path: path.to_path_buf(),
                source,
            })?
        else {
            break;
        };
        pos += u64::from(header_len);
        match &name {
            b"ftyp" => {
                saw_ftyp = true;
                skip(&mut reader, body_size).map_err(|source| Error::MetadataIo {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
            b"moov" => {
                if body_size > MAX_MOOV_BYTES {
                    return Err(Error::Mp4Parse {
                        path: path.to_path_buf(),
                        reason: format!("moov box size {body_size} exceeds {MAX_MOOV_BYTES}"),
                    });
                }
                let len = usize::try_from(body_size).map_err(|_| Error::Mp4Parse {
                    path: path.to_path_buf(),
                    reason: "moov box size doesn't fit in usize".into(),
                })?;
                let mut buf = vec![0u8; len];
                reader
                    .read_exact(&mut buf)
                    .map_err(|source| Error::MetadataIo {
                        path: path.to_path_buf(),
                        source,
                    })?;
                moov = Some(buf);
            }
            _ => {
                skip(&mut reader, body_size).map_err(|source| Error::MetadataIo {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
        }
        pos += body_size;
    }

    if !saw_ftyp {
        return Ok(None);
    }
    let Some(moov) = moov else {
        return Ok(None);
    };

    Ok(Some(decode_moov(&moov)))
}

/// Read a box header. Returns `Ok(None)` on EOF or fewer than 8 bytes
/// remaining. Returns fourcc, body-only size, and header length (8 or 16
/// — 16 for the largesize form with `size==1`).
fn read_box_header<R: Read>(
    reader: &mut R,
    remaining: u64,
) -> std::io::Result<Option<([u8; 4], u64, u8)>> {
    if remaining < 8 {
        return Ok(None);
    }
    let mut hdr = [0u8; 8];
    match reader.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let size = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
    let name: [u8; 4] = hdr[4..8].try_into().unwrap();

    match size {
        0 => {
            // "Box extends to EOF" — body = remaining - 8.
            Ok(Some((name, remaining - 8, 8)))
        }
        1 => {
            // 64-bit largesize follows.
            let mut large = [0u8; 8];
            reader.read_exact(&mut large)?;
            let size64 = u64::from_be_bytes(large);
            if size64 < 16 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "largesize < 16",
                ));
            }
            Ok(Some((name, size64 - 16, 16)))
        }
        n if (n as u64) < 8 => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "box size < 8",
        )),
        n => Ok(Some((name, u64::from(n) - 8, 8))),
    }
}

fn skip<R: Read + Seek>(reader: &mut R, n: u64) -> std::io::Result<()> {
    if n == 0 {
        return Ok(());
    }
    reader.seek(SeekFrom::Current(i64::try_from(n).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "skip too large")
    })?))?;
    Ok(())
}

/// Walk a buffered moov payload. Malformed sub-boxes are silently skipped
/// — bounded by the outer `moov` size, ignoring strays is friendlier to
/// real-world files than aborting.
fn decode_moov(moov: &[u8]) -> QtMetadata {
    let mut mvhd_creation = None;
    let mut creation_date_iso = None;
    let mut make = None;
    let mut model = None;
    let mut dimensions = None;

    for (name, body) in BoxIter::new(moov) {
        match &name {
            b"mvhd" => {
                if let Some(secs) = parse_mvhd_creation(body) {
                    mvhd_creation = mac_seconds_to_naive(secs);
                }
            }
            b"udta" => {
                let (d, mk, md) = decode_udta(body);
                if creation_date_iso.is_none() {
                    creation_date_iso = d;
                }
                if make.is_none() {
                    make = mk;
                }
                if model.is_none() {
                    model = md;
                }
            }
            b"trak" => {
                if dimensions.is_none()
                    && let Some(dims) = decode_trak_video_dims(body)
                {
                    dimensions = Some(dims);
                }
            }
            _ => {}
        }
    }

    QtMetadata {
        creation_date_iso,
        mvhd_creation,
        make,
        model,
        dimensions,
    }
}

/// Read `mvhd.creation_time` from an mvhd body. Handles version 0 (32-bit)
/// and version 1 (64-bit).
fn parse_mvhd_creation(body: &[u8]) -> Option<u64> {
    let version = *body.first()?;
    // version(1) + flags(3) = 4
    let rest = body.get(4..)?;
    match version {
        0 => {
            let bytes: [u8; 4] = rest.get(0..4)?.try_into().ok()?;
            Some(u64::from(u32::from_be_bytes(bytes)))
        }
        1 => {
            let bytes: [u8; 8] = rest.get(0..8)?.try_into().ok()?;
            Some(u64::from_be_bytes(bytes))
        }
        _ => None,
    }
}

fn decode_udta(body: &[u8]) -> (Option<NaiveDateTime>, Option<String>, Option<String>) {
    for (name, payload) in BoxIter::new(body) {
        if &name == b"meta" {
            return decode_meta(payload);
        }
    }
    (None, None, None)
}

/// Decode a `meta` box. Per ISO/IEC 14496-12, `meta` is a full-box: 4 bytes
/// version+flags. Some QuickTime writers skip the ext header and start
/// directly with `hdlr` — detect by peeking the first sub-box name.
/// `meta` with `mdir` (iTunes-style) handler is not useful for capture
/// metadata.
fn decode_meta(body: &[u8]) -> (Option<NaiveDateTime>, Option<String>, Option<String>) {
    let rest: &[u8] = if body.len() >= 8 && &body[4..8] == b"hdlr" {
        &body[4..]
    } else if body.len() >= 4 && &body[..4] == b"hdlr" {
        body
    } else if body.len() >= 4 {
        &body[4..]
    } else {
        return (None, None, None);
    };

    let mut handler_is_mdta = false;
    let mut keys_payload: Option<&[u8]> = None;
    let mut ilst_payload: Option<&[u8]> = None;

    for (name, payload) in BoxIter::new(rest) {
        match &name {
            b"hdlr" => {
                // hdlr: version+flags(4) + pre_defined(4) + handler_type(4)
                if let Some(ht) = payload.get(8..12)
                    && ht == b"mdta"
                {
                    handler_is_mdta = true;
                }
            }
            b"keys" => keys_payload = Some(payload),
            b"ilst" => ilst_payload = Some(payload),
            _ => {}
        }
    }

    if !handler_is_mdta {
        return (None, None, None);
    }
    match (keys_payload, ilst_payload) {
        (Some(k), Some(i)) => parse_mdta(k, i),
        _ => (None, None, None),
    }
}

fn decode_trak_video_dims(body: &[u8]) -> Option<(u32, u32)> {
    let mut handler_is_video = false;
    let mut dims: Option<(u32, u32)> = None;

    for (name, payload) in BoxIter::new(body) {
        match &name {
            b"tkhd" => {
                dims = parse_tkhd_dims(payload);
            }
            b"mdia" => {
                for (n, p) in BoxIter::new(payload) {
                    if &n == b"hdlr"
                        && let Some(ht) = p.get(8..12)
                        && ht == b"vide"
                    {
                        handler_is_video = true;
                    }
                }
            }
            _ => {}
        }
    }

    if handler_is_video { dims } else { None }
}

/// Read width/height from a `tkhd` body. tkhd version 0 is 84 bytes;
/// width/height are the last 8 bytes (16.16 fixed point, big-endian).
fn parse_tkhd_dims(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    let w_bytes: [u8; 4] = body[body.len() - 8..body.len() - 4].try_into().ok()?;
    let h_bytes: [u8; 4] = body[body.len() - 4..].try_into().ok()?;
    let w = u32::from_be_bytes(w_bytes) >> 16;
    let h = u32::from_be_bytes(h_bytes) >> 16;
    if w == 0 || h == 0 { None } else { Some((w, h)) }
}

/// Walk the `keys` table + `ilst` items and pull the QuickTime metadata
/// strings we care about. Encoding per Apple QTFF:
///
/// `keys` payload:
/// ```text
///   u32 version+flags
///   u32 entry_count
///   per entry:
///     u32 key_size      (whole entry incl. these 4 bytes)
///     u32 key_namespace (e.g. "mdta")
///     [key_size - 8] bytes of key name
/// ```
///
/// `ilst` payload: a sequence of boxes where the fourcc is a 1-based
/// index into the `keys` table. Each item contains a `data` sub-box:
/// ```text
///   u32 size
///   u32 "data"
///   u32 type_indicator   (1 = UTF-8, ...)
///   u32 locale
///   [rest] payload
/// ```
fn parse_mdta(
    keys_payload: &[u8],
    ilst_payload: &[u8],
) -> (Option<NaiveDateTime>, Option<String>, Option<String>) {
    let key_names = parse_keys(keys_payload).unwrap_or_default();

    let mut creation_date = None;
    let mut make = None;
    let mut model = None;

    for (key_idx, data) in parse_ilst(ilst_payload) {
        let Some(name_idx) = (key_idx as usize).checked_sub(1) else {
            continue;
        };
        let Some(name) = key_names.get(name_idx) else {
            continue;
        };
        let Ok(s) = std::str::from_utf8(&data) else {
            continue;
        };
        let s = s.trim().trim_end_matches('\0').trim();
        if s.is_empty() {
            continue;
        }
        match name.as_str() {
            "com.apple.quicktime.creationdate" => creation_date = parse_iso8601_local(s),
            "com.apple.quicktime.make" => make = Some(s.to_string()),
            "com.apple.quicktime.model" => model = Some(s.to_string()),
            _ => {}
        }
    }

    (creation_date, make, model)
}

fn parse_keys(payload: &[u8]) -> Option<Vec<String>> {
    let payload = payload.get(4..)?; // skip version+flags
    let (count_bytes, mut rest) = payload.split_at_checked(4)?;
    let count = u32::from_be_bytes(count_bytes.try_into().ok()?);

    let mut keys = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if rest.len() < 8 {
            break;
        }
        let key_size = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
        if key_size < 8 || rest.len() < key_size {
            break;
        }
        // rest[4..8] = key_namespace (e.g. "mdta"); foreign namespaces just
        // won't match our expected names.
        let name_bytes = &rest[8..key_size];
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        keys.push(name);
        rest = &rest[key_size..];
    }
    Some(keys)
}

fn parse_ilst(payload: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut items = Vec::new();
    for (fourcc, body) in BoxIter::new(payload) {
        let key_idx = u32::from_be_bytes(fourcc);
        if let Some(data) = find_data_payload(body) {
            items.push((key_idx, data));
        }
    }
    items
}

/// Find the `data` sub-box's post-header payload (after type_indicator + locale).
fn find_data_payload(item_body: &[u8]) -> Option<Vec<u8>> {
    for (name, payload) in BoxIter::new(item_body) {
        if &name == b"data" && payload.len() >= 8 {
            // skip type_indicator(4) + locale(4)
            return Some(payload[8..].to_vec());
        }
    }
    None
}

struct BoxIter<'a> {
    rest: &'a [u8],
}

impl<'a> BoxIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { rest: buf }
    }
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes(self.rest[0..4].try_into().ok()?);
        let name: [u8; 4] = self.rest[4..8].try_into().ok()?;
        let (header_len, body_len) = match size {
            0 => (8usize, self.rest.len() - 8),
            1 => {
                if self.rest.len() < 16 {
                    return None;
                }
                let size64 = u64::from_be_bytes(self.rest[8..16].try_into().ok()?);
                let total = usize::try_from(size64).ok()?;
                if total < 16 || total > self.rest.len() {
                    return None;
                }
                (16usize, total - 16)
            }
            n if (n as usize) < 8 || (n as usize) > self.rest.len() => return None,
            n => (8usize, n as usize - 8),
        };
        let body = &self.rest[header_len..header_len + body_len];
        self.rest = &self.rest[header_len + body_len..];
        Some((name, body))
    }
}

/// Parse Apple's `creationdate` ISO 8601 with optional offset
/// (`2024-03-15T14:22:10-0700` / `...-07:00` / `...123-07:00`); return
/// local wall-clock at that offset as a `NaiveDateTime`.
fn parse_iso8601_local(s: &str) -> Option<NaiveDateTime> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.naive_local());
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Some(dt.naive_local());
        }
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive);
        }
    }
    None
}

/// Convert seconds-since-1904-01-01-UTC to a [`NaiveDateTime`] (UTC).
/// Sentinel `0` ("not set") returns `None`.
fn mac_seconds_to_naive(mac_secs: u64) -> Option<NaiveDateTime> {
    if mac_secs == 0 {
        return None;
    }
    let unix_secs = i64::try_from(mac_secs)
        .ok()?
        .checked_sub(MAC_EPOCH_TO_UNIX)?;
    DateTime::from_timestamp(unix_secs, 0).map(|dt| dt.naive_utc())
}

/// Mirror of `exif::combine_make_model` — inlined because the EXIF helper
/// is private and the rule applies identically to QuickTime tags.
fn combine_make_model(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(mk), Some(md)) => {
            let mk_lc = mk.to_lowercase();
            let md_lc = md.to_lowercase();
            if md_lc.starts_with(&mk_lc) {
                return Some(md);
            }
            if let Some(first_word) = mk_lc.split_whitespace().next()
                && md_lc.starts_with(first_word)
            {
                return Some(md);
            }
            Some(format!("{mk} {md}"))
        }
        (Some(mk), None) => Some(mk),
        (None, Some(md)) => Some(md),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::quicktime_test_fixtures::{iphone_like_mp4, mvhd_only_mp4};
    use super::*;
    use chrono::NaiveDate;
    use tempfile::NamedTempFile;

    fn write_temp(bytes: &[u8]) -> NamedTempFile {
        let mut tmp = NamedTempFile::new().unwrap();
        std::io::Write::write_all(tmp.as_file_mut(), bytes).unwrap();
        tmp
    }

    #[test]
    fn mac_epoch_zero_returns_none() {
        assert!(mac_seconds_to_naive(0).is_none());
    }

    #[test]
    fn mac_epoch_converts_to_utc_naive() {
        // 2024-03-15 14:22:10 UTC = unix 1_710_512_530
        // mac seconds = unix + 2_082_844_800 = 3_793_357_330
        let dt = mac_seconds_to_naive(3_793_357_330).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn parses_apple_creationdate_with_colon_offset() {
        let dt = parse_iso8601_local("2024-03-15T14:22:10-07:00").unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn parses_apple_creationdate_without_colon_offset() {
        let dt = parse_iso8601_local("2024-03-15T14:22:10-0700").unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn combine_make_model_iphone() {
        assert_eq!(
            combine_make_model(Some("Apple".into()), Some("iPhone 13 Pro".into())),
            Some("Apple iPhone 13 Pro".into())
        );
    }

    #[test]
    fn try_date_reads_creationdate_from_fixture() {
        let bytes = iphone_like_mp4("2024-03-15T14:22:10-07:00", None, None);
        let tmp = write_temp(&bytes);

        let dt = QuicktimeExtractor::new()
            .try_date(tmp.path(), "video")
            .unwrap()
            .unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn try_date_falls_back_to_mvhd_when_no_mdta() {
        // 2024-03-15 14:22:10 UTC
        let unix_secs: i64 = 1_710_512_530;
        let bytes = mvhd_only_mp4(unix_secs);
        let tmp = write_temp(&bytes);

        let dt = QuicktimeExtractor::new()
            .try_date(tmp.path(), "video")
            .unwrap()
            .unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap();
        assert_eq!(dt, expected);
    }

    #[test]
    fn try_camera_reads_iphone_make_model() {
        let bytes = iphone_like_mp4(
            "2024-03-15T14:22:10-07:00",
            Some("Apple"),
            Some("iPhone 13 Pro"),
        );
        let tmp = write_temp(&bytes);

        let cam = QuicktimeExtractor::new()
            .try_camera(tmp.path(), "video")
            .unwrap();
        assert_eq!(cam.as_deref(), Some("Apple iPhone 13 Pro"));
    }

    #[test]
    fn try_date_on_non_mp4_returns_ok_none() {
        let tmp = write_temp(&[0xFF, 0xD8, 0xFF, 0xD9]);
        let out = QuicktimeExtractor::new()
            .try_date(tmp.path(), "photo")
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn try_camera_on_non_mp4_returns_ok_none() {
        let tmp = write_temp(b"definitely not an mp4");
        let out = QuicktimeExtractor::new()
            .try_camera(tmp.path(), "video")
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn cache_returns_same_metadata_for_repeated_calls() {
        let bytes = iphone_like_mp4(
            "2024-03-15T14:22:10-07:00",
            Some("Apple"),
            Some("iPhone 13 Pro"),
        );
        let tmp = write_temp(&bytes);

        let ex = QuicktimeExtractor::new();
        let dt = ex.try_date(tmp.path(), "video").unwrap().unwrap();
        let cam = ex.try_camera(tmp.path(), "video").unwrap();
        assert_eq!(
            dt,
            NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(14, 22, 10)
                .unwrap()
        );
        assert_eq!(cam.as_deref(), Some("Apple iPhone 13 Pro"));
    }

    #[test]
    fn container_ok_accepts_well_formed_mp4() {
        let bytes = iphone_like_mp4("2024-03-15T14:22:10-07:00", None, None);
        let tmp = write_temp(&bytes);
        assert!(container_ok(tmp.path()).unwrap());
    }

    #[test]
    fn container_ok_rejects_empty_file() {
        let tmp = write_temp(&[]);
        assert!(!container_ok(tmp.path()).unwrap());
    }

    #[test]
    fn container_ok_rejects_non_mp4() {
        let tmp = write_temp(&[0xFF, 0xD8, 0xFF, 0xD9]);
        assert!(!container_ok(tmp.path()).unwrap());
    }

    #[test]
    fn container_ok_rejects_truncated_mp4() {
        let mut bytes = iphone_like_mp4("2024-03-15T14:22:10-07:00", None, None);
        // Lop off the tail — the moov declared size overruns EOF.
        let cut = bytes.len().saturating_sub(64);
        bytes.truncate(cut);
        let tmp = write_temp(&bytes);
        assert!(!container_ok(tmp.path()).unwrap());
    }
}
