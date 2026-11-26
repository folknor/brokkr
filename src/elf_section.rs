//! Read one named section out of an ELF file, std only, so `brokkr` and
//! `brokkr-results-server` (which includes this file by path) can each read
//! the build stamp `brokkr install` wrote into it (`crate::build_info`).
//!
//! Only what a stamp needs: 64-bit little-endian ELF, the section header
//! table and its string table, including the extended numbering a file with
//! 0xff00 or more sections uses. Anything else is an error naming why.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Every section named `name` in `path`, in table order: none, one, or
/// (for a file someone stamped twice) several.
pub(crate) fn read_sections(path: &Path, name: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let shown = path.display().to_string();
    let mut header = [0u8; 64];
    file.read_exact(&mut header)
        .map_err(|e| format!("{shown}: cannot read an ELF header: {e}"))?;
    if &header[..4] != b"\x7fELF" {
        return Err(format!("{shown} is not an ELF file"));
    }
    if header[4] != 2 || header[5] != 1 {
        return Err(format!("{shown} is not a 64-bit little-endian ELF file"));
    }
    let shoff = u64_at(&header, 0x28);
    let shentsize = u64::from(u16_at(&header, 0x3a));
    let mut shnum = u64::from(u16_at(&header, 0x3c));
    let mut shstrndx = u64::from(u16_at(&header, 0x3e));
    if shoff == 0 {
        return Err(format!("{shown} has no section headers"));
    }
    if shentsize < 64 {
        return Err(format!(
            "{shown}: section header entries of {shentsize} bytes"
        ));
    }
    if shnum == 0 || shstrndx == 0xffff {
        // Extended numbering: the real values live in section header 0.
        let first = read_at(&mut file, shoff, 64, &shown)?;
        if shnum == 0 {
            shnum = u64_at(&first, 0x20);
        }
        if shstrndx == 0xffff {
            shstrndx = u64::from(u32_at(&first, 0x28));
        }
    }
    if shnum > 1 << 20 || shstrndx >= shnum {
        return Err(format!(
            "{shown}: implausible section table ({shnum} entries, names at {shstrndx})"
        ));
    }
    let len = usize::try_from(shnum * shentsize)
        .map_err(|_| format!("{shown}: section table too large"))?;
    let table = read_at(&mut file, shoff, len, &shown)?;
    let entry = |i: u64| {
        let start = usize::try_from(i * shentsize).unwrap_or(usize::MAX);
        table.get(start..start.saturating_add(64))
    };
    let names = entry(shstrndx).ok_or_else(|| format!("{shown}: no section name table"))?;
    let names = read_at(
        &mut file,
        u64_at(names, 0x18),
        usize::try_from(u64_at(names, 0x20))
            .map_err(|_| format!("{shown}: name table too large"))?,
        &shown,
    )?;
    let mut found = Vec::new();
    for i in 0..shnum {
        let Some(sh) = entry(i) else { break };
        let offset = usize::try_from(u32_at(sh, 0)).unwrap_or(usize::MAX);
        let Some(rest) = names.get(offset..) else {
            continue;
        };
        let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        if &rest[..end] != name.as_bytes() {
            continue;
        }
        // SHT_NOBITS occupies no file bytes.
        if u32_at(sh, 4) == 8 {
            found.push(Vec::new());
            continue;
        }
        let size =
            usize::try_from(u64_at(sh, 0x20)).map_err(|_| format!("{shown}: section too large"))?;
        if size > 1 << 20 {
            return Err(format!(
                "{shown}: {name} is {size} bytes, more than a stamp"
            ));
        }
        found.push(read_at(&mut file, u64_at(sh, 0x18), size, &shown)?);
    }
    Ok(found)
}

fn read_at(
    file: &mut std::fs::File,
    offset: u64,
    len: usize,
    shown: &str,
) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("{shown}: cannot seek to {offset}: {e}"))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)
        .map_err(|e| format!("{shown}: cannot read {len} bytes at {offset}: {e}"))?;
    Ok(buf)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(a)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// The stamp line a running executable carries, from `/proc/self/exe`:
/// `Ok(None)` when it has none (a plain cargo build, or one from before
/// stamping), an error when the section is unreadable or not exactly one
/// well-formed line.
pub(crate) fn own_stamp(section: &str) -> Result<Option<String>, String> {
    file_stamp(Path::new("/proc/self/exe"), section)
}

/// [`own_stamp`] for any file: read, never executed, so it is safe on a
/// privileged helper such as `brokkr-fstrim`.
pub(crate) fn file_stamp(path: &Path, section: &str) -> Result<Option<String>, String> {
    let found = read_sections(path, section)?;
    match found.as_slice() {
        [] => Ok(None),
        [one] => {
            let text = std::str::from_utf8(one).map_err(|_| format!("{section} is not UTF-8"))?;
            let line = text.strip_suffix('\n').unwrap_or(text);
            if line.is_empty() || line.contains('\n') || !line.starts_with("sha ") {
                return Err(format!("{section} does not hold one stamp line"));
            }
            Ok(Some(line.to_owned()))
        }
        _ => Err(format!("{section} appears {} times", found.len())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn the_running_test_binary_reads_as_elf() {
        // A test binary is never stamped; reading it must still parse.
        let found =
            super::read_sections(std::path::Path::new("/proc/self/exe"), ".brokkr.build_info")
                .unwrap();
        assert!(found.is_empty());
        let names =
            super::read_sections(std::path::Path::new("/proc/self/exe"), ".shstrtab").unwrap();
        assert_eq!(names.len(), 1);
        assert!(names[0].windows(9).any(|w| w == b".shstrtab"));
    }
}
