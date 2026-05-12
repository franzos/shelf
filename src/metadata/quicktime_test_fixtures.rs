//! Synthetic MP4 builder for tests.
//!
//! Generates a minimal but spec-valid MP4 containing exactly the boxes the
//! [`super::quicktime`] extractor reads: `ftyp`, `moov` (with `mvhd` and
//! optionally `udta.meta` with `hdlr=mdta`, `keys`, `ilst`). No `trak` —
//! dimension coverage is a separate unit test against a hand-crafted
//! tkhd body.

use std::io::Write;

const MAC_EPOCH_TO_UNIX: i64 = 2_082_844_800;

/// Build an iPhone-style MP4: `mvhd.creation_time` set to Unix epoch (so
/// it's distinguishable from `creationdate` results) plus an `mdta` block
/// carrying `com.apple.quicktime.creationdate`, `.make`, `.model`.
pub fn iphone_like_mp4(
    creation_date_iso: &str,
    make: Option<&str>,
    model: Option<&str>,
) -> Vec<u8> {
    let mvhd_seconds: u64 = u64::try_from(MAC_EPOCH_TO_UNIX).unwrap();
    build_mp4(mvhd_seconds, Some(creation_date_iso), make, model)
}

/// Build a barebones MP4 with no mdta metadata, just an `mvhd.creation_time`
/// set to the supplied Unix timestamp (converted to Mac epoch internally).
pub fn mvhd_only_mp4(unix_secs: i64) -> Vec<u8> {
    let mac_secs = u64::try_from(unix_secs + MAC_EPOCH_TO_UNIX).unwrap();
    build_mp4(mac_secs, None, None, None)
}

fn build_mp4(
    mvhd_mac_secs: u64,
    creation_date_iso: Option<&str>,
    make: Option<&str>,
    model: Option<&str>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(ftyp());
    out.extend(moov(mvhd_mac_secs, creation_date_iso, make, model));
    out
}

fn ftyp() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"mp42"); // major_brand
    body.extend_from_slice(&0u32.to_be_bytes()); // minor_version
    body.extend_from_slice(b"isom");
    body.extend_from_slice(b"mp42");
    wrap_box(b"ftyp", &body)
}

fn moov(
    mvhd_mac_secs: u64,
    creation_date_iso: Option<&str>,
    make: Option<&str>,
    model: Option<&str>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(mvhd(mvhd_mac_secs));
    if creation_date_iso.is_some() || make.is_some() || model.is_some() {
        body.extend(udta_with_mdta(creation_date_iso, make, model));
    }
    wrap_box(b"moov", &body)
}

fn mvhd(creation_secs: u64) -> Vec<u8> {
    // version 0: 32-bit times. Layout per ISO/IEC 14496-12 §8.2.2.
    let mut body = Vec::new();
    body.push(0); // version
    body.extend_from_slice(&[0, 0, 0]); // flags
    let secs32 = u32::try_from(creation_secs).expect("fits in u32");
    body.extend_from_slice(&secs32.to_be_bytes()); // creation_time
    body.extend_from_slice(&secs32.to_be_bytes()); // modification_time
    body.extend_from_slice(&1000u32.to_be_bytes()); // timescale
    body.extend_from_slice(&0u32.to_be_bytes()); // duration
    body.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate (1.0)
    body.extend_from_slice(&0x0100u16.to_be_bytes()); // volume (1.0)
    body.extend_from_slice(&[0u8; 2]); // reserved
    body.extend_from_slice(&[0u8; 8]); // reserved
    // 3x3 identity matrix in 16.16 / 2.30 fixed point
    let matrix: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
    for v in matrix {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&[0u8; 24]); // pre_defined
    body.extend_from_slice(&1u32.to_be_bytes()); // next_track_id
    wrap_box(b"mvhd", &body)
}

fn udta_with_mdta(
    creation_date_iso: Option<&str>,
    make: Option<&str>,
    model: Option<&str>,
) -> Vec<u8> {
    let meta = meta_mdta(creation_date_iso, make, model);
    wrap_box(b"udta", &meta)
}

fn meta_mdta(creation_date_iso: Option<&str>, make: Option<&str>, model: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // meta version+flags
    body.extend(hdlr_mdta());

    let mut keys: Vec<&str> = Vec::new();
    let mut values: Vec<&str> = Vec::new();
    if let Some(s) = creation_date_iso {
        keys.push("com.apple.quicktime.creationdate");
        values.push(s);
    }
    if let Some(s) = make {
        keys.push("com.apple.quicktime.make");
        values.push(s);
    }
    if let Some(s) = model {
        keys.push("com.apple.quicktime.model");
        values.push(s);
    }

    body.extend(keys_box(&keys));
    body.extend(ilst_box(&values));
    wrap_box(b"meta", &body)
}

fn hdlr_mdta() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // version+flags
    body.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    body.extend_from_slice(b"mdta"); // handler_type
    body.extend_from_slice(&[0u8; 12]); // reserved
    body.push(0); // empty name + trailing NUL
    wrap_box(b"hdlr", &body)
}

fn keys_box(keys: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // version+flags
    body.extend_from_slice(&u32::try_from(keys.len()).unwrap().to_be_bytes());
    for k in keys {
        let key_size = 8 + k.len();
        body.extend_from_slice(&u32::try_from(key_size).unwrap().to_be_bytes());
        body.extend_from_slice(b"mdta"); // key_namespace
        body.extend_from_slice(k.as_bytes());
    }
    wrap_box(b"keys", &body)
}

fn ilst_box(values: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    for (idx, v) in values.iter().enumerate() {
        // 1-based key index, encoded as the item box's "fourcc"
        let key_idx = u32::try_from(idx + 1).unwrap();
        let data = data_box_utf8(v);
        let item_size = 8 + data.len();
        body.extend_from_slice(&u32::try_from(item_size).unwrap().to_be_bytes());
        body.extend_from_slice(&key_idx.to_be_bytes());
        body.extend_from_slice(&data);
    }
    wrap_box(b"ilst", &body)
}

fn data_box_utf8(s: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_be_bytes()); // type_indicator = 1 (UTF-8)
    body.extend_from_slice(&0u32.to_be_bytes()); // locale
    body.extend_from_slice(s.as_bytes());
    wrap_box(b"data", &body)
}

fn wrap_box(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let size = 8 + body.len();
    let mut out = Vec::with_capacity(size);
    out.write_all(&u32::try_from(size).unwrap().to_be_bytes())
        .unwrap();
    out.write_all(name).unwrap();
    out.write_all(body).unwrap();
    out
}
