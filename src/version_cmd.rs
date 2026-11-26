//! `brokkr --version` (and `-V`): this brokkr's banner, its build stamp and
//! protocol, the build stamp of each installed helper binary (read from the
//! file, never run), and the same for the results server the user-wide config names,
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
            helper_lines(None);
            return server_line(None);
        }
    };
    output::data_stdout(&format!(
        "local:   {}, protocol {}",
        local
            .as_deref()
            .unwrap_or("unstamped (not installed by brokkr install)"),
        wire::PROTOCOL_VERSION
    ));
    helper_lines(local.as_deref());
    server_line(local.as_deref());
}

/// The helper binaries brokkr runs, and where each is found: the two
/// `cargo install` puts beside this executable, and the root-owned fstrim
/// helper the operator copies to its fixed path.
fn helpers() -> Vec<(&'static str, Option<std::path::PathBuf>)> {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    vec![
        ("brokkr-rustc-guard", dir.as_ref().map(|d| d.join("brokkr-rustc-guard"))),
        ("brokkr-results-server", dir.as_ref().map(|d| d.join("brokkr-results-server"))),
        ("brokkr-fstrim", Some(crate::fstrim::HELPER_PATH.into())),
    ]
}

/// One line per helper: its stamp, read from the file and never executed,
/// and whether it was built from a different commit than this brokkr.
fn helper_lines(local: Option<&str>) {
    for (name, path) in helpers() {
        let Some(path) = path else {
            output::data_stdout(&format!("helper:  {name}: this brokkr's directory is unknown"));
            continue;
        };
        let shown = path.display();
        if !path.exists() {
            output::data_stdout(&format!("helper:  {name}: not installed at {shown}"));
            continue;
        }
        let mut line = match crate::elf_section::file_stamp(&path, crate::build_info::SECTION) {
            Ok(Some(stamp)) => {
                let mut line = format!("helper:  {name} {}", short_stamp(&stamp));
                if let Some(local) = local
                    && commit_of(local) != commit_of(&stamp)
                {
                    line.push_str(" - built from a different commit than this brokkr");
                }
                line
            }
            Ok(None) => format!("helper:  {name}: unstamped"),
            Err(e) => format!("helper:  {name}: stamp unreadable: {e}"),
        };
        if name == "brokkr-fstrim" {
            line.push_str(&format!(" ({shown})"));
        }
        output::data_stdout(&line);
    }
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
        stamp.map_or_else(|| "unstamped (or older than stamps)".to_owned(), short_stamp),
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

/// A stamp line as a short commit id, `-dirty` when the tree was not clean;
/// the line itself when it is not the `sha HEX clean Y|N` shape.
fn short_stamp(stamp: &str) -> String {
    let Some(sha) = commit_of(stamp) else {
        return stamp.to_owned();
    };
    let short = sha.get(..8).unwrap_or(sha);
    if stamp.ends_with(" clean y") {
        short.to_owned()
    } else {
        format!("{short}-dirty")
    }
}

/// The commit id a stamp line names.
fn commit_of(stamp: &str) -> Option<&str> {
    stamp.strip_prefix("sha ")?.split(' ').next()
}
