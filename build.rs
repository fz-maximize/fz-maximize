//! Embed the application manifest (themed common controls, per-monitor DPI awareness)
//! and the version resource (product name and version, shown in the file's Properties
//! and required for code signing), using the MSVC linker directly, so no resource
//! compiler is needed.

use std::path::Path;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("fz-maximize.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env != "msvc" {
        return;
    }
    // Only the binary gets these; test harnesses are unaffected.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());

    let version = env!("CARGO_PKG_VERSION");
    let res = Path::new(&std::env::var("OUT_DIR").unwrap()).join("version.res");
    std::fs::write(&res, version_res(version)).expect("write version.res");
    // link.exe accepts compiled resource (.res) files as inputs.
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

/// A compiled resource file holding one `VS_VERSIONINFO` resource for `version`
/// ("major.minor.patch").
fn version_res(version: &str) -> Vec<u8> {
    let mut parts = version.split(['.', '-', '+']).map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    let four = format!("{major}.{minor}.{patch}.0");
    let ms = (u32::from(major) << 16) | u32::from(minor);
    let ls = u32::from(patch) << 16;

    // VS_FIXEDFILEINFO
    let mut fixed = Vec::new();
    for v in [
        0xFEEF_04BD, // signature
        0x0001_0000, // structure version
        ms,          // file version
        ls,
        ms, // product version
        ls,
        0x3F,        // flags mask
        0,           // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,           // VFT_APP
        0,           // subtype
        0,           // date
        0,
    ] {
        fixed.extend_from_slice(&u32::to_le_bytes(v));
    }

    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "fz-maximize"),
        ("FileDescription", "fz-maximize"),
        ("FileVersion", four.as_str()),
        ("InternalName", "fz-maximize"),
        (
            "LegalCopyright",
            "Copyright (c) 2026 fz-maximize contributors. MIT License.",
        ),
        ("OriginalFilename", "fz-maximize.exe"),
        ("ProductName", "fz-maximize"),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(k, v)| {
        let value = utf16z(v);
        block(k, &value, (value.len() / 2) as u16, 1, &[])
    })
    .collect();
    // U.S. English, Unicode.
    let table = block("040904B0", &[], 0, 1, &strings);
    let string_info = block("StringFileInfo", &[], 0, 1, &[table]);
    let translation = [0x09u8, 0x04, 0xB0, 0x04];
    let var = block("Translation", &translation, 4, 0, &[]);
    let var_info = block("VarFileInfo", &[], 0, 1, &[var]);
    let info = block(
        "VS_VERSION_INFO",
        &fixed,
        fixed.len() as u16,
        0,
        &[string_info, var_info],
    );

    let mut res = Vec::new();
    // A .res file starts with an empty entry.
    res.extend_from_slice(&res_header(0, 0, 0, 0));
    res.extend_from_slice(&res_header(info.len() as u32, 16, 1, 0x0409)); // RT_VERSION, id 1
    res.extend_from_slice(&info);
    pad4(&mut res);
    res
}

/// A version-resource block: length, value length, type, key, value, children; each part
/// aligned to 4 bytes.
fn block(key: &str, value: &[u8], value_len: u16, kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0u8; 6];
    b.extend_from_slice(&utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    for c in children {
        pad4(&mut b);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b[2..4].copy_from_slice(&value_len.to_le_bytes());
    b[4..6].copy_from_slice(&kind.to_le_bytes());
    b
}

/// A 32-byte .res entry header with a numeric type and name.
fn res_header(data_size: u32, type_id: u16, name_id: u16, language: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(32);
    h.extend_from_slice(&data_size.to_le_bytes());
    h.extend_from_slice(&32u32.to_le_bytes());
    h.extend_from_slice(&[0xFF, 0xFF]);
    h.extend_from_slice(&type_id.to_le_bytes());
    h.extend_from_slice(&[0xFF, 0xFF]);
    h.extend_from_slice(&name_id.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes()); // data version
    let flags: u16 = if data_size == 0 { 0 } else { 0x0030 }; // MOVEABLE | PURE
    h.extend_from_slice(&flags.to_le_bytes());
    h.extend_from_slice(&language.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes()); // version
    h.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    h
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn pad4(b: &mut Vec<u8>) {
    while b.len() % 4 != 0 {
        b.push(0);
    }
}
