//! `deploy` against a real server on a loopback port: the confirmation by
//! the executable digest, each way it fails, the restart command's bounds,
//! and the declaration's refusals.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Instant;

use crate::Limits;
use crate::deploy::{self, Declaration};
use crate::tests::{BUILD, scratch, server};

fn declared(url: String, restart: &[&str], timeout_secs: u64) -> Declaration {
    Declaration {
        url,
        restart: restart.iter().map(|&s| s.to_owned()).collect(),
        timeout_secs,
    }
}

/// `converge` under the declaration's own deadline, from now.
fn converge(declaration: &Declaration, digest: &str, cwd: &Path) -> Result<(), String> {
    let deadline = declaration.deadline(Instant::now())?;
    deploy::converge(declaration, digest, cwd, deadline)
}

/// A loopback responder writing `answer` to every connection, for answers
/// a real server never gives.
fn responder(answer: Vec<u8>) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut head = [0u8; 4096];
            drop(stream.read(&mut head));
            drop(stream.write_all(&answer));
        }
    });
    addr
}

#[test]
fn a_server_answering_with_this_build_confirms_the_deploy() {
    let server = server("deploy-ok", Limits::DEFAULT);
    let cwd = scratch("deploy-ok-home");
    let declaration = declared(
        format!("http://{}/bench", server.addr),
        &["touch", "ran"],
        10,
    );
    converge(&declaration, BUILD, &cwd).unwrap();
    assert!(
        cwd.join("ran").exists(),
        "the restart runs from the home directory"
    );
}

#[test]
fn another_build_answering_fails_at_the_deadline_naming_it() {
    let server = server("deploy-other", Limits::DEFAULT);
    let declaration = declared(format!("http://{}/bench", server.addr), &["true"], 1);
    let error = converge(&declaration, &"f".repeat(32), &scratch("deploy-other-home")).unwrap_err();
    assert!(
        error.contains(&format!("answered with build {BUILD}")),
        "{error}"
    );
}

#[test]
fn nothing_listening_fails_at_the_deadline() {
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let declaration = declared(format!("http://{addr}/bench"), &["true"], 1);
    let error = converge(&declaration, BUILD, &scratch("deploy-none-home")).unwrap_err();
    assert!(error.contains("did not answer"), "{error}");
}

#[test]
fn a_failed_status_or_an_oversized_answer_never_counts() {
    let cwd = scratch("deploy-scripted-home");
    let identity =
        format!(r#"{{"server":"brokkr-results-server","protocol":1,"build":"{BUILD}"}}"#);
    let busy = responder(
        format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{identity}",
            identity.len()
        )
        .into_bytes(),
    );
    let error = converge(
        &declared(format!("http://{busy}/bench"), &["true"], 1),
        BUILD,
        &cwd,
    )
    .unwrap_err();
    assert!(error.contains("status 503"), "{error}");

    let padding = " ".repeat(70 * 1024);
    let large = responder(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{padding}",
            padding.len()
        )
        .into_bytes(),
    );
    let error = converge(
        &declared(format!("http://{large}/bench"), &["true"], 1),
        BUILD,
        &cwd,
    )
    .unwrap_err();
    assert!(error.contains("could not be read"), "{error}");
}

#[test]
fn a_failing_or_hanging_restart_fails_before_any_probe() {
    let server = server("deploy-restart", Limits::DEFAULT);
    let url = format!("http://{}/bench", server.addr);
    let cwd = scratch("deploy-restart-home");
    let failed = converge(&declared(url.clone(), &["false"], 10), BUILD, &cwd).unwrap_err();
    assert!(
        failed.contains("the restart command `false` failed"),
        "{failed}"
    );

    let started = Instant::now();
    let hung = converge(&declared(url, &["sleep", "30"], 1), BUILD, &cwd).unwrap_err();
    assert!(
        hung.contains("did not finish within the deadline"),
        "{hung}"
    );
    assert!(hung.contains("was killed"), "{hung}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn the_watchdog_stops_waiting_for_work_blocked_past_the_deadline() {
    let started = Instant::now();
    let error = deploy::watched(started, 0, || {
        std::thread::sleep(std::time::Duration::from_secs(30));
        Ok(())
    })
    .unwrap_err();
    assert!(error.contains("did not finish within 0 s"), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(deploy::watched(started, 0, || Ok(7)).unwrap(), 7);
}

#[test]
fn a_deadline_already_passed_runs_no_restart() {
    let cwd = scratch("deploy-late-home");
    let declaration = declared("http://127.0.0.1:1/bench".into(), &["touch", "ran"], 1);
    let error = deploy::converge(&declaration, BUILD, &cwd, Instant::now()).unwrap_err();
    assert!(error.contains("before the restart command"), "{error}");
    assert!(!cwd.join("ran").exists());
}

#[test]
fn only_an_answer_made_for_the_probe_by_this_protocol_counts() {
    let good = format!(
        r#"{{"server":"brokkr-results-server","protocol":1,"probe":"ab","build":"{BUILD}"}}"#
    );
    assert_eq!(deploy::judge(good.as_bytes(), "ab").unwrap(), BUILD);
    assert!(
        deploy::judge(good.as_bytes(), "cd")
            .unwrap_err()
            .contains("not made for this probe")
    );
    let other = good.replace("brokkr-results-server", "nginx");
    assert!(deploy::judge(other.as_bytes(), "ab").is_err());
    let protocol = good.replace(r#""protocol":1"#, r#""protocol":2"#);
    assert!(
        deploy::judge(protocol.as_bytes(), "ab")
            .unwrap_err()
            .contains("protocol 2")
    );
    let no_build = r#"{"server":"brokkr-results-server","protocol":1,"probe":"ab"}"#;
    assert!(
        deploy::judge(no_build.as_bytes(), "ab")
            .unwrap_err()
            .contains("no build")
    );
    assert!(deploy::judge(b"<html>", "ab").is_err());
}

#[test]
fn a_missing_declaration_is_no_server_and_a_bad_one_is_refused() {
    let dir = scratch("deploy-declaration");
    let path = dir.join("deploy.toml");
    assert!(deploy::load(&path).unwrap().is_none());

    std::fs::write(
        &path,
        "url = \"http://127.0.0.1:8650/bench\"\nrestart = [\"true\"]\n",
    )
    .unwrap();
    let loaded = deploy::load(&path).unwrap().unwrap();
    assert_eq!(loaded.timeout_secs, 300);

    let huge = format!(
        "url = \"http://h/bench\"\nrestart = [\"true\"]\ntimeout_secs = {}\n",
        i64::MAX
    );
    std::fs::write(&path, huge).unwrap();
    let loaded = deploy::load(&path).unwrap().unwrap();
    assert!(
        loaded
            .deadline(Instant::now())
            .unwrap_err()
            .contains("too large")
    );

    for (text, expect) in [
        (
            "url = \"ftp://h/bench\"\nrestart = [\"true\"]\n",
            "not http",
        ),
        ("url = \"http://\"\nrestart = [\"true\"]\n", "url"),
        (
            "url = \"http://h/bench?x=1\"\nrestart = [\"true\"]\n",
            "query",
        ),
        ("url = \"http://h/bench\"\nrestart = []\n", "no command"),
        (
            "url = \"http://h/bench\"\nrestart = [\"true\"]\ntimeout_secs = 0\n",
            "is 0",
        ),
        (
            "url = \"http://h/bench\"\nrestart = [\"true\"]\nhost = \"x\"\n",
            "unknown field",
        ),
    ] {
        std::fs::write(&path, text).unwrap();
        let error = deploy::load(&path).unwrap_err();
        assert!(error.contains(expect), "{text}: {error}");
    }

    let dangling = dir.join("dangling.toml");
    std::os::unix::fs::symlink(dir.join("gone.toml"), &dangling).unwrap();
    let error = deploy::load(&dangling).unwrap_err();
    assert!(error.contains("cannot read"), "{error}");
}
