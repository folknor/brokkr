//! The embedded `brokkr-launch` executable and its copy on disk.
//!
//! `build.rs` compiles `helper_main.rs` with no ELF interpreter and hands the
//! bytes to this crate. brokkr writes them, once, to a path named by their
//! digest under the per-user launch directory, so the helper a brokkr runs is
//! always the one it was built with, and no install step can leave a stale
//! one behind.

use std::path::PathBuf;

use crate::error::DevError;

#[cfg(launch_helper)]
static BYTES: &[u8] = include_bytes!(env!("BROKKR_LAUNCH_HELPER"));

/// The helper's file name. It switches mode on it: under this name it takes
/// `--plan` and `--probe`; a copy under any other name is a peer launcher.
pub(crate) const NAME: &str = "brokkr-launch";

/// The path of this brokkr's helper, written there first if absent.
pub(crate) fn path() -> Result<PathBuf, DevError> {
    #[cfg(not(launch_helper))]
    {
        Err(DevError::Refused(
            "brokkr-launch is not built for this platform (it needs Linux on x86_64 or \
             aarch64), so brokkr cannot launch programs through cargo here"
                .into(),
        ))
    }
    #[cfg(launch_helper)]
    {
        let base = crate::user_dirs::launch_dir().ok_or_else(|| {
            DevError::Config("cannot place brokkr-launch: $HOME is not set".into())
        })?;
        install_at(&base, BYTES)
    }
}

/// Write `bytes` to `base/helper/DIGEST/brokkr-launch`, executable, unless an
/// identical file is already there. A file of the right name holding other
/// bytes (a write cut short) is replaced, never trusted.
#[cfg_attr(not(launch_helper), allow(dead_code))]
fn install_at(base: &std::path::Path, bytes: &[u8]) -> Result<PathBuf, DevError> {
    use std::os::unix::fs::PermissionsExt;

    let digest = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes));
    let dir = base.join("helper").join(digest);
    let file = dir.join(NAME);
    if std::fs::read(&file).is_ok_and(|on_disk| on_disk == bytes) {
        return Ok(file);
    }
    std::fs::create_dir_all(&dir)?;
    let staging = dir.join(format!(".{NAME}.{}", std::process::id()));
    std::fs::write(&staging, bytes)?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staging, &file)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_copy_is_content_addressed_and_repaired() {
        let base = crate::test_scratch::scratch("launch", "helper");
        let first = install_at(&base, b"helper bytes").unwrap();
        assert_eq!(first.file_name().unwrap(), NAME);
        assert_eq!(install_at(&base, b"helper bytes").unwrap(), first);
        // A truncated copy is rewritten rather than run.
        std::fs::write(&first, b"helper").unwrap();
        assert_eq!(install_at(&base, b"helper bytes").unwrap(), first);
        assert_eq!(std::fs::read(&first).unwrap(), b"helper bytes");
        // Other bytes get their own directory.
        assert_ne!(install_at(&base, b"other").unwrap(), first);
    }

    /// The embedded helper has no ELF interpreter and no dynamic section: a
    /// target's LD_PRELOAD or loader path cannot run code inside it.
    #[cfg(launch_helper)]
    #[test]
    fn the_embedded_helper_is_static() {
        let elf = BYTES;
        assert_eq!(&elf[..4], b"\x7fELF");
        assert_eq!(elf[4], 2, "64-bit");
        let u16_at = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(elf[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(elf[o..o + 8].try_into().unwrap());
        let phoff = usize::try_from(u64_at(0x20)).unwrap();
        let phentsize = usize::from(u16_at(0x36));
        let phnum = usize::from(u16_at(0x38));
        const PT_DYNAMIC: u32 = 2;
        const PT_INTERP: u32 = 3;
        for i in 0..phnum {
            let kind = u32_at(phoff + i * phentsize);
            assert_ne!(kind, PT_INTERP, "the helper names an ELF interpreter");
            assert_ne!(kind, PT_DYNAMIC, "the helper has a dynamic section");
        }
    }
}
