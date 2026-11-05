//! `brokkr-results-server`: the bench store with a network layer in front.
//!
//! `serve`: one process owns one SQLite file (`--db`): it takes an exclusive
//! lock beside it, opens and migrates it through the store's own open, and
//! serves the store's wire contract (`store::wire`) on `--listen` over
//! HTTP/1.1. Every request runs on one worker thread that alone holds the
//! connection, so every write is serial under `BEGIN IMMEDIATE`. Nothing
//! deployment-specific is compiled in: the file and the address are always
//! given, and who can reach the address is the network's business (no
//! application auth). `deploy` (`deploy.rs`) restarts the server a host
//! declares and confirms it runs this build, by the executable digest its
//! identity route reports (`identity.rs`).
//!
//! Self-contained like the rustc guard: the store is included by path, and
//! nothing else of the brokkr crate is linked. `reference/results-server.md`
//! is how it is built; `docs/results-server.md` how to run it.

// The store is shared with the brokkr binary, which uses parts of it this
// server does not (the read-only open, the local loader).
#[allow(dead_code, unused_imports)]
#[path = "../../bench/store/mod.rs"]
mod store;

mod deploy;
mod http;
mod identity;
mod owner;
mod worker;

#[cfg(test)]
mod deploy_tests;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// The bench store over HTTP.
#[derive(Debug, Parser)]
#[command(name = "brokkr-results-server", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Serve the bench store over HTTP.
    Serve(ServeArgs),
    /// Restart this host's server, if ~/.config/brokkr-results-server/deploy.toml
    /// declares one, and confirm it answers with this build; nothing, and
    /// success, on a host without that file.
    Deploy,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// The SQLite file to own: an absolute path, created with its directory
    /// if missing. It must be on a local filesystem and belong to this server
    /// alone.
    #[arg(long, value_name = "PATH")]
    db: PathBuf,
    /// The address to listen on, such as a loopback port a TLS proxy
    /// forwards to.
    #[arg(long, value_name = "ADDR")]
    listen: SocketAddr,
}

/// Every bound the server keeps, in one place. A client attempt outlasts
/// `connection` (`store::wire::CONNECTION_DEADLINE`, which brokkr's client
/// reads too) so it sees the server's own answer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Open connections; one past this is closed unanswered.
    pub(crate) connections: usize,
    /// Requests admitted to read their body, queued or running; one past
    /// this is answered `busy` without its body being read.
    pub(crate) in_flight: usize,
    /// Admitted request bytes across every admitted request, by declared
    /// length. Bounds what is admitted, not what decoding allocates.
    pub(crate) budget_bytes: u64,
    /// The largest body one request may declare.
    pub(crate) max_request_bytes: u64,
    /// Jobs waiting for the worker.
    pub(crate) queue: usize,
    /// From accepting a connection to its request head being read.
    pub(crate) head: Duration,
    /// From admission to the whole body being read.
    pub(crate) body: Duration,
    /// How long a request waits for the worker to answer it. Bounds the
    /// wait, not the work: a running job holds the worker until it ends.
    pub(crate) wait: Duration,
    /// The whole connection, head to the last byte written.
    pub(crate) connection: Duration,
}

impl Limits {
    pub(crate) const DEFAULT: Self = Self {
        connections: 256,
        in_flight: 64,
        budget_bytes: 1 << 30,
        max_request_bytes: store::wire::MAX_REQUEST_BYTES,
        queue: 64,
        head: Duration::from_secs(10),
        body: Duration::from_secs(120),
        wait: Duration::from_secs(60),
        connection: store::wire::CONNECTION_DEADLINE,
    };
}

// The phases of one request fit inside its connection's deadline, so a
// request that is not stuck gets its answer written before the deadline cuts
// the connection.
const _: () = assert!(
    Limits::DEFAULT.head.as_secs()
        + Limits::DEFAULT.body.as_secs()
        + Limits::DEFAULT.wait.as_secs()
        < Limits::DEFAULT.connection.as_secs()
);

/// One line on stderr, which a service manager's journal keeps.
#[allow(clippy::print_stderr)] // The server's log is its stderr.
pub(crate) fn log(message: &str) {
    eprintln!("brokkr-results-server: {message}");
}

fn main() -> ExitCode {
    let ran = match Cli::parse().command {
        Cmd::Serve(args) => serve(&args),
        Cmd::Deploy => deploy::run().map_err(|message| format!("deploy: {message}")),
    };
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            log(&message);
            ExitCode::FAILURE
        }
    }
}

fn serve(args: &ServeArgs) -> Result<(), String> {
    // Read before anything else, from the opened executable, so it names
    // the file this process runs even if an install replaces the path while
    // the store migrates. A digest that cannot be read is no reason not to
    // serve: the identity route omits it, and a deploy then fails.
    let build = identity::executable_digest()
        .map_err(|e| log(&format!("no build digest on the identity route: {e}")))
        .ok();
    let owned = owner::own(&args.db)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(args.listen)
            .await
            .map_err(|e| format!("cannot listen on {}: {e}", args.listen))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("cannot read the listening address: {e}"))?;
        let path = owned.path.display().to_string();
        let (queue, worker_gone) = worker::start(owned.store, Limits::DEFAULT.queue)?;
        log(&format!(
            "serving {path} on {bound}, protocol {}",
            store::wire::PROTOCOL_VERSION
        ));
        let shared = Arc::new(http::Shared::new(Limits::DEFAULT, queue, build));
        let ended = http::serve(listener, shared, worker_gone, std::future::pending()).await;
        // The lock is held until here, past the last request served.
        drop(owned.lock);
        ended
    })
}
