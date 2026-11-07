// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! `worlds`: Infinity Worlds (contract DEDICATED_SERVER.md §2, WORLDS_OFFICIAL.md §1). One binary,
//! one role per process:
//!
//! ```text
//! worlds serve --profile public --control unix:<path> --bind <addr:port> [options]   # the engine
//! worlds serve --version                 # also `worlds --version`
//! worlds serve --health --control unix:<path>
//! worlds relay                           # not implemented yet: exit 7
//! worlds agent [options]                 # the world agent (Linux; Windows: exit 7)
//! ```

use std::{io::Write, time::Duration};

use worlds::{
    alloc::CountingAlloc,
    cli::{self, Command, Endpoint, Role},
    log, log_ids,
    runtime::{self, exit},
};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse_role(&all) {
        Ok(Role::Serve(args)) => args,
        Ok(Role::Version) => std::process::exit(version()),
        Ok(Role::Agent(args)) => std::process::exit(agent(&args)),
        Ok(Role::Unimplemented(role)) => {
            log::error(log_ids::DS_ROLE_UNIMPLEMENTED)
                .field("role", role)
                .emit();
            std::process::exit(exit::ROLE_UNIMPLEMENTED);
        }
        Err(field) => {
            log::error(log_ids::DS_CONFIG_INVALID)
                .field("field", field)
                .emit();
            std::process::exit(exit::CONFIG);
        }
    };
    let env = |name: &str| std::env::var(name).ok();
    let command = match cli::parse(&args, &env) {
        Ok(c) => c,
        Err(field) => {
            log::error(log_ids::DS_CONFIG_INVALID)
                .field("field", field)
                .emit();
            std::process::exit(exit::CONFIG);
        }
    };
    let code = match command {
        Command::Version => version(),
        Command::Health(endpoint) => health(&endpoint),
        Command::Run(rc) => {
            log::start(rc.log_level);
            install_panic_hook();
            let code = runtime::run(*rc);
            log::shutdown(Duration::from_secs(2));
            code
        }
    };
    std::process::exit(code);
}

/// Panics are logged (anonymised) as one line; the hook never exits: a world's fault boundary
/// may catch the panic, and a panic of the loop thread becomes exit 70 when it is joined.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let text = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_string(),
            None => info
                .payload()
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_default(),
        };
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        log::error(log_ids::DS_PANIC)
            .field("thread", thread)
            .msg(&text)
            .emit();
    }));
}

/// `worlds agent`: the world agent of a Worlds host (Linux only, DEDICATED_SERVER.md §16).
#[cfg(unix)]
fn agent(args: &[String]) -> i32 {
    log::start(log::Level::Info);
    let env = |name: &str| std::env::var(name).ok();
    let code = worlds::agent::main(args, &env);
    log::shutdown(Duration::from_secs(2));
    code
}

#[cfg(not(unix))]
fn agent(_args: &[String]) -> i32 {
    log::error(log_ids::DS_ROLE_UNIMPLEMENTED)
        .field("role", "agent")
        .emit();
    exit::ROLE_UNIMPLEMENTED
}

fn version() -> i32 {
    let line = serde_json::json!({
        "name": "worlds",
        "version": worlds::VERSION,
        "commit": worlds::commit(),
        "proto": [worlds::PROTO_MIN, worlds::PROTO_MAX],
    });
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    exit::OK
}

/// The VM's HEALTHCHECK: connects, `hello` (no controller) + `health`; 0 ready, 1 not.
#[cfg(unix)]
fn health(endpoint: &Endpoint) -> i32 {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let Endpoint::Unix(path) = endpoint;
    let Ok(mut stream) = UnixStream::connect(path) else {
        return 1;
    };
    let wait = Some(Duration::from_secs(5));
    if stream.set_read_timeout(wait).is_err() || stream.set_write_timeout(wait).is_err() {
        return 1;
    }
    // the contract's integer `proto` (§3.2)
    let request = format!(
        "{{\"id\":1,\"op\":\"hello\",\"proto\":{}}}\n{{\"id\":2,\"op\":\"health\"}}\n",
        worlds::PROTO_MAX
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return 1;
    }
    let reader = BufReader::new(stream);
    for line in reader.lines().take(16) {
        let Ok(line) = line else { return 1 };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("id").and_then(|i| i.as_u64()) == Some(2) {
            let ready = v.get("ok").and_then(|b| b.as_bool()) == Some(true)
                && v.get("ready").and_then(|b| b.as_bool()) == Some(true);
            return if ready { 0 } else { 1 };
        }
    }
    1
}

#[cfg(not(unix))]
fn health(_endpoint: &Endpoint) -> i32 {
    1
}
