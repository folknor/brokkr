//! Shared local-server addresses and timing policy for nidhogg commands.

use std::time::Duration;

pub(crate) const LOCAL_HOST: &str = "localhost";

pub(crate) const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const HEALTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(6);
pub(crate) const SERVER_READY_POLL_INTERVAL: Duration = Duration::from_millis(200);
pub(crate) const SERVER_TERM_GRACE: Duration = Duration::from_secs(5);
pub(crate) const SERVER_KILL_GRACE: Duration = Duration::from_secs(2);
pub(crate) const TILE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const TILE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const TILE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const API_BENCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const TILE_ITERATIONS: usize = 5;

/// The address of a local nidhogg server instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerAddress {
    port: u16,
}

impl ServerAddress {
    pub(crate) const fn localhost(port: u16) -> Self {
        Self { port }
    }

    pub(crate) fn url(self, path: &str) -> String {
        format!("http://{}:{}/{path}", LOCAL_HOST, self.port)
    }
}

/// Render whole-second curl timeout values from the shared duration policy.
pub(crate) fn curl_seconds(timeout: Duration) -> String {
    timeout.as_secs().to_string()
}
