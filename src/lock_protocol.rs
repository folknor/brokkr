// Values shared by the lock holder and the standalone rustc guard. This file
// is included by the lock primitives, path resolver and guard binary so the
// no-lib-target boundary does not require a second spelling of the protocol.

pub const HOME_ENV: &str = "HOME";
pub const STATE_DIRECTORY: &str = ".brokkr";
pub const LOCK_FILE: &str = "brokkr.lock";
pub const COMPILE_LOCK_FILE: &str = "compile.lock";

pub const AUTH_KEY: &str = "auth";
pub const CAPABILITY_ENV: &str = "BROKKR_HOLD_NONCE";
pub const LEASE_MARKER_ENV: &str = "BROKKR_COMPILE_LEASE";
pub const CARGO_OVERRIDE_ENV: &str = "BROKKR_CARGO";

pub const GUARD_INFO_ARG: &str = "--brokkr-guard-info";
pub const GUARD_PROTOCOL: u32 = 1;
pub const GUARD_HANDSHAKE_PREFIX: &str = "brokkr-rustc-guard protocol=";
pub const GUARD_CAPABILITIES_KEY: &str = "capabilities";
pub const GUARD_CAPABILITIES: &str = "capability-lease-v1,query-bypass-v1";

pub fn guard_handshake() -> String {
    format!(
        "{GUARD_HANDSHAKE_PREFIX}{GUARD_PROTOCOL} {GUARD_CAPABILITIES_KEY}={GUARD_CAPABILITIES}"
    )
}
