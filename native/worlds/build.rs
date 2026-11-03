// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! Windows VERSIONINFO for `worlds.exe` (contract DEDICATED_SERVER.md §2.4, WORLDS_OFFICIAL.md §1: the
//! firewall prompt and rule list show `FileDescription` "Infinity Worlds").
//!
//! No dependency and no resource compiler: the script writes the compiled resource (`.res`) itself
//! and hands it to the MSVC linker, which accepts `.res` inputs directly (link.exe and lld-link).
//! Other targets (Linux, the windows-gnu cross build) get no resource.
//!
//! `DS_PRODUCT_VERSION` (the launcher's `-Version`, e.g. `2.0.5` or `2.0.5-test.6`) sets
//! `ProductVersion` and, from its `major.minor.patch`, the numeric file and product versions;
//! the crate version otherwise. CompanyName and LegalCopyright follow `infinity.exe`'s.

use std::io::Write;

const FILE_DESCRIPTION: &str = "Infinity Worlds";
const PRODUCT_NAME: &str = "infinity";
const COMPANY_NAME: &str = "infinity";
const LEGAL_COPYRIGHT: &str = " ";
const EXE_NAME: &str = "worlds.exe";
/// en-US, Unicode (the StringTable key "040904B0" and the Translation entry).
const LANGUAGE: u16 = 0x0409;
const CODEPAGE: u16 = 1200;

fn main() {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "cargo:rerun-if-changed=build.rs");
    let _ = writeln!(out, "cargo:rerun-if-env-changed=DS_PRODUCT_VERSION");
    let _ = writeln!(out, "cargo:rerun-if-env-changed=DS_COMMIT");
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !windows_msvc {
        return;
    }
    let version = std::env::var("DS_PRODUCT_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION"));
    let numeric = numeric_version(&version).unwrap_or_else(|| {
        panic!("DS_PRODUCT_VERSION must start with major.minor.patch (u16 each): {version:?}")
    });
    let res = resource_file(&version_info(&version, numeric));
    let path = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("worlds-version.res");
    std::fs::write(&path, res).expect("write the version resource");
    let _ = writeln!(out, "cargo:rustc-link-arg-bins={}", path.display());
}

/// `2.0.5-test.6+meta` -> [2, 0, 5, 0].
fn numeric_version(version: &str) -> Option<[u16; 4]> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut numbers = [0u16; 4];
    for slot in numbers.iter_mut().take(3) {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn pad4(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

/// One version-resource block: wLength, wValueLength, wType, szKey, padding, value, children
/// (each 32-bit aligned). `value_length` is in words for text values, in bytes for binary ones.
fn block(key: &str, value: &[u8], value_length: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&value_length.to_le_bytes());
    bytes.extend_from_slice(&u16::from(text).to_le_bytes());
    bytes.extend_from_slice(&utf16z(key));
    pad4(&mut bytes);
    bytes.extend_from_slice(value);
    for child in children {
        pad4(&mut bytes);
        bytes.extend_from_slice(child);
    }
    let length = u16::try_from(bytes.len()).expect("version resource fits in 64 KiB");
    bytes[0..2].copy_from_slice(&length.to_le_bytes());
    bytes
}

fn string(key: &str, value: &str) -> Vec<u8> {
    let encoded = utf16z(value);
    let words = u16::try_from(encoded.len() / 2).expect("short string");
    block(key, &encoded, words, true, &[])
}

/// The VS_VERSIONINFO tree: VS_FIXEDFILEINFO, StringFileInfo (one en-US Unicode table) and
/// VarFileInfo.
fn version_info(version: &str, [major, minor, patch, build]: [u16; 4]) -> Vec<u8> {
    let ms = (u32::from(major) << 16) | u32::from(minor);
    let ls = (u32::from(patch) << 16) | u32::from(build);
    let numeric = format!("{major}.{minor}.{patch}.{build}");
    let fixed: Vec<u8> = [
        0xFEEF_04BD, // dwSignature
        0x0001_0000, // dwStrucVersion
        ms,          // dwFileVersionMS
        ls,          // dwFileVersionLS
        ms,          // dwProductVersionMS
        ls,          // dwProductVersionLS
        0x0000_003F, // dwFileFlagsMask
        0,           // dwFileFlags
        0x0004_0004, // dwFileOS: VOS_NT_WINDOWS32
        0x0000_0001, // dwFileType: VFT_APP
        0,           // dwFileSubtype
        0,           // dwFileDateMS
        0,           // dwFileDateLS
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    let strings = [
        string("CompanyName", COMPANY_NAME),
        string("FileDescription", FILE_DESCRIPTION),
        string("FileVersion", &numeric),
        string("InternalName", EXE_NAME),
        string("LegalCopyright", LEGAL_COPYRIGHT),
        string("OriginalFilename", EXE_NAME),
        string("ProductName", PRODUCT_NAME),
        string("ProductVersion", version),
    ];
    let table = block(
        &format!("{LANGUAGE:04X}{CODEPAGE:04X}"),
        &[],
        0,
        true,
        &strings,
    );
    let string_file_info = block("StringFileInfo", &[], 0, true, &[table]);
    let translation = [LANGUAGE.to_le_bytes(), CODEPAGE.to_le_bytes()].concat();
    let var = block("Translation", &translation, 4, false, &[]);
    let var_file_info = block("VarFileInfo", &[], 0, true, &[var]);
    block(
        "VS_VERSION_INFO",
        &fixed,
        52,
        false,
        &[string_file_info, var_file_info],
    )
}

/// A `.res` file: the empty leading entry, then RT_VERSION (16) #1 with `data`.
fn resource_file(data: &[u8]) -> Vec<u8> {
    let entry = |data: &[u8], kind: u16, name: u16, flags: u16, language: u16| -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::try_from(data.len()).expect("small").to_le_bytes()); // DataSize
        bytes.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
        bytes.extend_from_slice(&[0xFF, 0xFF]);
        bytes.extend_from_slice(&kind.to_le_bytes()); // TYPE (ordinal)
        bytes.extend_from_slice(&[0xFF, 0xFF]);
        bytes.extend_from_slice(&name.to_le_bytes()); // NAME (ordinal)
        bytes.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
        bytes.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
        bytes.extend_from_slice(&language.to_le_bytes()); // LanguageId
        bytes.extend_from_slice(&0u32.to_le_bytes()); // Version
        bytes.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
        bytes.extend_from_slice(data);
        pad4(&mut bytes);
        bytes
    };
    let mut res = entry(&[], 0, 0, 0, 0);
    // MOVEABLE | PURE, like rc.exe writes for a VERSIONINFO.
    res.extend(entry(data, 16, 1, 0x0030, LANGUAGE));
    res
}
