//! Sluggrs visual snapshot testing module.

pub(crate) mod cmd;
pub(crate) mod db;

use std::io::Read;

use crate::error::DevError;

pub(crate) fn generate_run_id() -> Result<String, DevError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex)
}
