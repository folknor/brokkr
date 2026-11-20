//! `brokkr --version` (and `-V`): this brokkr's banner, its build stamp and
//! protocol, and the same for the results server the user-wide config names,
//! asked once over its identity route, so a stale server or a stale local
//! install shows next to the other. Always exits 0: it reports what it could
//! find, a server that does not answer included.

use crate::bench::store::wire;
use crate::output;

pub(crate) fn print_version() {
    output::data_stdout(&format!("brokkr {}", crate::cli::LONG_VERSION));
    let local = match crate::elf_section::own_stamp(crate::build_info::SECTION) {
        Ok(Some(stamp)) => Some(stamp),
        Ok(None) => None,
        Err(e) => {
            output::data_stdout(&format!("local:   stamp unreadable: {e}"));
            return server_line(None);
        }
    };
    output::data_stdout(&format!(
        "local:   {}, protocol {}",
        local.as_deref().unwrap_or("unstamped (not installed by brokkr install)"),
        wire::PROTOCOL_VERSION
    ));
    server_line(local.as_deref());
}

fn server_line(local: Option<&str>) {
    let server = match crate::config::compose_user_only() {
        Ok((_, sources)) => sources.user_layer().and_then(|u| u.results_server.clone()),
        Err(e) => {
            output::data_stdout(&format!("server:  user-wide config unreadable: {e}"));
            return;
        }
    };
    let Some(server) = server else {
        output::data_stdout("server:  none configured (results_server in the user-wide config)");
        return;
    };
    let identity = match crate::bench::remote::Client::identity_once(&server) {
        Ok(identity) => identity,
        Err(e) => {
            output::data_stdout(&format!("server:  {} unreachable: {e}", server.shown));
            return;
        }
    };
    let protocol = identity.get("protocol").and_then(serde_json::Value::as_u64);
    let stamp = identity.get("stamp").and_then(serde_json::Value::as_str);
    let mut line = format!(
        "server:  {} {}, protocol {}",
        server.shown,
        stamp.unwrap_or("unstamped (or older than stamps)"),
        protocol.map_or_else(|| "unknown".to_owned(), |p| p.to_string())
    );
    if protocol != Some(u64::from(wire::PROTOCOL_VERSION)) {
        line.push_str(" - protocol differs from this brokkr's: upgrade them together");
    } else if let (Some(local), Some(stamp)) = (local, stamp)
        && commit_of(local) != commit_of(stamp)
    {
        line.push_str(" - built from a different commit than this brokkr");
    }
    output::data_stdout(&line);
}

/// The commit id a stamp line names.
fn commit_of(stamp: &str) -> Option<&str> {
    stamp.strip_prefix("sha ")?.split(' ').next()
}
