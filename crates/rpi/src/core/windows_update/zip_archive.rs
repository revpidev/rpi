//! A minimal ZIP reader for the Windows release asset.
//!
//! The Windows asset is `rpi-<version>-<target>.zip` with `rpi.exe` at its
//! root (build.yml `Compress-Archive`). The workspace has no ZIP crate and
//! ADR-0050 adds no dependency, so this reads the subset a release asset
//! uses: a classic (non-ZIP64) central directory, entries stored or
//! deflated (raw, `flate2`), sizes taken from the central directory (an
//! entry may carry a data descriptor), and the CRC-32 checked on
//! decompression. Encrypted entries, unsupported methods, unsafe names and
//! ZIP64 are rejected rather than guessed at.

use std::io::Read;

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const MAX_COMMENT: usize = 0xFFFF;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
const FLAG_ENCRYPTED: u16 = 0x0001;

fn little_endian(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn little_endian_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u16(bytes: &[u8], offset: usize, what: &str) -> Result<u16, String> {
    bytes
        .get(offset..offset + 2)
        .map(little_endian_u16)
        .ok_or_else(|| format!("truncated ZIP record: {what}"))
}

fn read_u32(bytes: &[u8], offset: usize, what: &str) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .map(little_endian)
        .ok_or_else(|| format!("truncated ZIP record: {what}"))
}

/// One central-directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    /// Entry name with `/` separators.
    pub name: String,
    method: u16,
    flags: u16,
    crc32: u32,
    compressed_size: usize,
    uncompressed_size: usize,
    local_header_offset: usize,
}

/// Parse the central directory.
/// Offset of the end-of-central-directory record: the last signature
/// whose comment length reaches exactly the end of the file.
fn find_eocd(archive: &[u8]) -> Option<usize> {
    if archive.len() < 22 {
        return None;
    }
    let lowest = archive.len().saturating_sub(22 + MAX_COMMENT);
    let mut index = archive.len() - 22;
    loop {
        if little_endian(&archive[index..index + 4]) == EOCD_SIGNATURE {
            let comment = usize::from(little_endian_u16(&archive[index + 20..index + 22]));
            if index + 22 + comment == archive.len() {
                return Some(index);
            }
        }
        if index <= lowest || index == 0 {
            return None;
        }
        index -= 1;
    }
}

/// Parse the central directory.
pub fn read_entries(archive: &[u8]) -> Result<Vec<ZipEntry>, String> {
    let eocd =
        find_eocd(archive).ok_or_else(|| "the release asset is not a ZIP archive".to_owned())?;
    if eocd >= 20 && little_endian(&archive[eocd - 20..eocd - 16]) == ZIP64_LOCATOR_SIGNATURE {
        return Err("ZIP64 archives are not supported".to_owned());
    }
    let entries = read_u16(archive, eocd + 10, "entry count")? as usize;
    let directory_size = read_u32(archive, eocd + 12, "directory size")? as usize;
    let directory_offset = read_u32(archive, eocd + 16, "directory offset")? as usize;
    if directory_offset == 0xFFFF_FFFF || directory_size == 0xFFFF_FFFF {
        return Err("ZIP64 archives are not supported".to_owned());
    }
    let directory = archive
        .get(directory_offset..directory_offset + directory_size)
        .ok_or("the ZIP central directory is truncated")?;
    let mut out = Vec::with_capacity(entries);
    let mut cursor = 0;
    for _ in 0..entries {
        if read_u32(directory, cursor, "central directory signature")? != CENTRAL_SIGNATURE {
            return Err("the ZIP central directory is corrupt".to_owned());
        }
        let flags = read_u16(directory, cursor + 8, "flags")?;
        let method = read_u16(directory, cursor + 10, "method")?;
        let crc32 = read_u32(directory, cursor + 16, "crc32")?;
        let compressed_size = read_u32(directory, cursor + 20, "compressed size")? as usize;
        let uncompressed_size = read_u32(directory, cursor + 24, "uncompressed size")? as usize;
        let name_length = read_u16(directory, cursor + 28, "name length")? as usize;
        let extra_length = read_u16(directory, cursor + 30, "extra length")? as usize;
        let comment_length = read_u16(directory, cursor + 32, "comment length")? as usize;
        let local_header_offset = read_u32(directory, cursor + 42, "local header offset")? as usize;
        if local_header_offset == 0xFFFF_FFFF {
            return Err("ZIP64 archives are not supported".to_owned());
        }
        let name_start = cursor + 46;
        let name = directory
            .get(name_start..name_start + name_length)
            .ok_or("the ZIP central directory is truncated")?;
        let name = String::from_utf8_lossy(name).replace('\\', "/");
        out.push(ZipEntry {
            name,
            method,
            flags,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        });
        cursor = name_start + name_length + extra_length + comment_length;
    }
    Ok(out)
}

/// Decompress one entry, verifying its CRC-32.
pub fn extract_entry(archive: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, String> {
    if entry.flags & FLAG_ENCRYPTED != 0 {
        return Err(format!("{} is encrypted", entry.name));
    }
    let header = archive
        .get(entry.local_header_offset..entry.local_header_offset + 30)
        .ok_or("the ZIP local header is truncated")?;
    if read_u32(header, 0, "local header signature")? != LOCAL_SIGNATURE {
        return Err("the ZIP local header is corrupt".to_owned());
    }
    let name_length = read_u16(header, 26, "local name length")? as usize;
    let extra_length = read_u16(header, 28, "local extra length")? as usize;
    let data_start = entry.local_header_offset + 30 + name_length + extra_length;
    let data = archive
        .get(data_start..data_start + entry.compressed_size)
        .ok_or_else(|| format!("{} is truncated", entry.name))?;
    let bytes = match entry.method {
        METHOD_STORED => data.to_vec(),
        METHOD_DEFLATE => {
            let mut decoder = flate2::read::DeflateDecoder::new(data);
            let mut out = Vec::with_capacity(entry.uncompressed_size.min(64 * 1024 * 1024));
            decoder
                .read_to_end(&mut out)
                .map_err(|error| format!("{} does not decompress: {error}", entry.name))?;
            out
        }
        other => {
            return Err(format!(
                "{} uses an unsupported compression method ({other})",
                entry.name
            ));
        }
    };
    if bytes.len() != entry.uncompressed_size {
        return Err(format!(
            "{} decompresses to {} byte(s), the archive says {}",
            entry.name,
            bytes.len(),
            entry.uncompressed_size
        ));
    }
    let mut crc = flate2::Crc::new();
    crc.update(&bytes);
    if crc.sum() != entry.crc32 {
        return Err(format!("{} fails its CRC-32 check", entry.name));
    }
    Ok(bytes)
}

/// A safe member name: relative, no `..`, no drive letter, no NUL.
pub fn is_safe_member_name(name: &str) -> bool {
    if name.is_empty() || name.contains('\0') {
        return false;
    }
    let normalized = name.trim_start_matches("./");
    if normalized.starts_with('/') || normalized.len() >= 2 && normalized.as_bytes()[1] == b':' {
        return false;
    }
    !normalized.split('/').any(|segment| segment == "..")
}

/// The bytes of `member` (e.g. `rpi.exe`) in `archive`.
pub fn extract_member(archive: &[u8], member: &str) -> Result<Vec<u8>, String> {
    let entries = read_entries(archive)?;
    let entry = entries
        .iter()
        .find(|entry| entry.name.trim_start_matches("./") == member)
        .ok_or_else(|| format!("the release archive does not contain {member}"))?;
    if !is_safe_member_name(&entry.name) {
        return Err(format!("unsafe archive member: {}", entry.name));
    }
    extract_entry(archive, entry)
}

/// Member names, for diagnostics and tests.
pub fn member_names(archive: &[u8]) -> Result<Vec<String>, String> {
    Ok(read_entries(archive)?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = flate2::Crc::new();
        crc.update(bytes);
        crc.sum()
    }

    /// Build a small ZIP with the given `(name, bytes, deflate)` members.
    fn build_zip(members: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, bytes, deflate) in members {
            let offset = out.len() as u32;
            let (method, data): (u16, Vec<u8>) = if *deflate {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(bytes).unwrap();
                (METHOD_DEFLATE, encoder.finish().unwrap())
            } else {
                (METHOD_STORED, bytes.to_vec())
            };
            let name = name.as_bytes();
            out.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // time + date
            out.extend_from_slice(&crc32(bytes).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name);
            out.extend_from_slice(&data);
            central.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // version made
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&crc32(bytes).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
        }
        let directory_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&directory_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn reads_stored_and_deflated_members() {
        let exe = vec![0u8; 100_000];
        let archive = build_zip(&[
            ("rpi.exe", &exe, false),
            (
                "extensions\\rpi-todo-0.1.9-rc.4-t.rpix",
                b"rpix bytes",
                true,
            ),
        ]);
        assert_eq!(
            member_names(&archive).unwrap(),
            vec!["rpi.exe", "extensions/rpi-todo-0.1.9-rc.4-t.rpix"]
        );
        assert_eq!(extract_member(&archive, "rpi.exe").unwrap(), exe);
        assert_eq!(
            extract_member(&archive, "extensions/rpi-todo-0.1.9-rc.4-t.rpix").unwrap(),
            b"rpix bytes"
        );
    }

    #[test]
    fn a_member_with_a_data_descriptor_uses_the_central_sizes() {
        // Rebuild `rpi.exe` with bit 3 set and zeroed local sizes.
        let exe = b"MZ-something";
        let mut archive = build_zip(&[("rpi.exe", exe, true)]);
        let entries = read_entries(&archive).unwrap();
        let local = entries[0].local_header_offset;
        archive[local + 6..local + 8].copy_from_slice(&0x0008u16.to_le_bytes());
        archive[local + 14..local + 22].copy_from_slice(&[0; 8]);
        assert_eq!(extract_member(&archive, "rpi.exe").unwrap(), exe);
    }

    #[test]
    fn corrupt_truncated_and_unsafe_archives_are_rejected() {
        let archive = build_zip(&[("rpi.exe", b"payload", false)]);
        assert!(member_names(b"not a zip at all").is_err());
        assert!(member_names(&archive[..archive.len() - 5]).is_err());
        // A corrupted payload fails the CRC check.
        let mut corrupt = archive.clone();
        let entries = read_entries(&archive).unwrap();
        let local = entries[0].local_header_offset;
        let data = local + 30 + "rpi.exe".len();
        corrupt[data] ^= 0xFF;
        let error = extract_member(&corrupt, "rpi.exe").unwrap_err();
        assert!(error.contains("CRC-32"), "{error}");
        // Missing member.
        assert!(
            extract_member(&archive, "other.exe")
                .unwrap_err()
                .contains("does not contain")
        );
        // Unsafe names never load even when listed.
        let unsafe_archive =
            build_zip(&[("../rpi.exe", b"x", false), ("/abs/rpi.exe", b"x", false)]);
        assert!(
            extract_member(&unsafe_archive, "../rpi.exe")
                .unwrap_err()
                .contains("unsafe")
        );
        assert!(
            extract_member(&unsafe_archive, "/abs/rpi.exe")
                .unwrap_err()
                .contains("unsafe")
        );
        assert!(!is_safe_member_name("C:/x"));
        assert!(!is_safe_member_name("a/../../b"));
        assert!(is_safe_member_name("./rpi.exe"));
        assert!(is_safe_member_name("extensions/x.rpix"));
        // An unsupported method.
        let mut other = archive.clone();
        let directory_offset = little_endian(&other[other.len() - 6..other.len() - 2]) as usize;
        other[directory_offset + 10..directory_offset + 12].copy_from_slice(&99u16.to_le_bytes());
        assert!(
            extract_member(&other, "rpi.exe")
                .unwrap_err()
                .contains("unsupported compression")
        );
        // An encrypted entry.
        let mut encrypted = archive;
        let directory_offset =
            little_endian(&encrypted[encrypted.len() - 6..encrypted.len() - 2]) as usize;
        encrypted[directory_offset + 8..directory_offset + 10].copy_from_slice(&1u16.to_le_bytes());
        assert!(
            extract_member(&encrypted, "rpi.exe")
                .unwrap_err()
                .contains("encrypted")
        );
        // ZIP64 is refused instead of misread.
        let zip64 = build_zip(&[("rpi.exe", b"x", false)]);
        let eocd = zip64.len() - 22;
        let mut with_locator = zip64[..eocd].to_vec();
        with_locator.extend_from_slice(&ZIP64_LOCATOR_SIGNATURE.to_le_bytes());
        with_locator.extend_from_slice(&0u32.to_le_bytes());
        with_locator.extend_from_slice(&0u64.to_le_bytes());
        with_locator.extend_from_slice(&1u32.to_le_bytes());
        with_locator.extend_from_slice(&zip64[eocd..]);
        assert_eq!(
            member_names(&with_locator).unwrap_err(),
            "ZIP64 archives are not supported"
        );
    }

    #[test]
    fn an_archive_with_a_comment_is_read() {
        let mut archive = build_zip(&[("rpi.exe", b"x", false)]);
        let comment = b"release notes";
        let eocd = archive.len() - 22;
        archive[eocd + 20..eocd + 22].copy_from_slice(&(comment.len() as u16).to_le_bytes());
        archive.extend_from_slice(comment);
        assert_eq!(member_names(&archive).unwrap(), vec!["rpi.exe"]);
    }

    /// A real release asset, when `RPI_UPDATE_TEST_ASSET` names a ZIP and
    /// `RPI_UPDATE_TEST_MEMBER` the member to extract (default `rpi.exe`);
    /// Python's `zipfile` is the byte-level oracle when `python3` is on the
    /// PATH. Ignored by default: it reads whatever path the variable names.
    #[test]
    #[ignore = "reads the asset named by RPI_UPDATE_TEST_ASSET"]
    fn extracts_a_real_release_asset_against_the_zipfile_oracle() {
        let Ok(path) = std::env::var("RPI_UPDATE_TEST_ASSET") else {
            return;
        };
        let member =
            std::env::var("RPI_UPDATE_TEST_MEMBER").unwrap_or_else(|_| "rpi.exe".to_owned());
        let archive = std::fs::read(&path).unwrap();
        let names = member_names(&archive).unwrap();
        let bytes = extract_member(&archive, &member).unwrap();
        let oracle = std::process::Command::new("python3")
            .args([
                "-c",
                "import zipfile,sys; sys.stdout.buffer.write(zipfile.ZipFile(sys.argv[1]).read(sys.argv[2]))",
                &path,
                &member,
            ])
            .output()
            .expect("python3");
        assert!(
            oracle.status.success(),
            "{}",
            String::from_utf8_lossy(&oracle.stderr)
        );
        assert_eq!(
            bytes, oracle.stdout,
            "byte-identical to the oracle extraction"
        );
        eprintln!(
            "asset {path}: members {names:?}, {member} is {} bytes",
            bytes.len()
        );
    }
}
