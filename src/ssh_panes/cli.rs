//! Fork-owned CLI grammar and dispatch. Remote helpers bypass local feature config:
//! the payload has no dependency on the remote machine's Herdr installation.

#[cfg(unix)]
use super::sync;
use super::{
    connector::{self, ConnectOptions, RunOptions},
    remote,
};
use std::{collections::BTreeMap, io};

const USAGE: &str = "herdr ssh <host> [--cwd <dir>] [--no-sync] [-- <ssh args>]\nherdr ssh sync <host> [--force] [--dry-run] [--json]\nherdr ssh run <host> [--cwd <dir>] [--env K=V]... [--no-tty] -- <argv...>\nherdr ssh list [--json]\nherdr ssh doctor <host> [--json]";

enum Command {
    Help,
    Connect(ConnectOptions),
    Run(RunOptions),
    Sync {
        host: String,
        force: bool,
        dry_run: bool,
        json: bool,
    },
    List {
        json: bool,
    },
    Doctor {
        host: String,
        json: bool,
    },
    Attach(String),
    Watch(remote::WatchArgs),
    RemoteRun(String, Vec<String>),
}
fn usage(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn take(args: &[String], i: &mut usize) -> io::Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| usage("missing option value"))
}
fn parse(args: &[String]) -> io::Result<Command> {
    let first = args
        .first()
        .ok_or_else(|| usage("missing host or subcommand"))?;
    if matches!(first.as_str(), "help" | "--help" | "-h") && args.len() == 1 {
        return Ok(Command::Help);
    }
    if first == "remote-attach" && args.len() == 3 && args[1] == "--spec" {
        return Ok(Command::Attach(args[2].clone()));
    }
    if first == "remote-run" && args.len() >= 3 && args[1] == "--spec" {
        if args.len() > 3 && args[3] != "--" {
            return Err(usage("remote-run arguments require --"));
        }
        return Ok(Command::RemoteRun(
            args[2].clone(),
            args.get(4..).unwrap_or_default().to_vec(),
        ));
    }
    if first == "remote-watch" {
        let mut fields = BTreeMap::new();
        let mut i = 1;
        while i < args.len() {
            let key = &args[i];
            if !["--link", "--shell-pid", "--server-key", "--socket"].contains(&key.as_str())
                || fields.contains_key(key)
            {
                return Err(usage("invalid remote-watch option"));
            }
            fields.insert(key.clone(), take(args, &mut i)?);
            i += 1;
        }
        let get = |key: &str| {
            fields
                .get(key)
                .cloned()
                .ok_or_else(|| usage("missing remote-watch option"))
        };
        return Ok(Command::Watch(remote::WatchArgs {
            link: get("--link")?,
            shell_pid: get("--shell-pid")?
                .parse()
                .map_err(|_| usage("invalid shell pid"))?,
            server_key: get("--server-key")?,
            socket: get("--socket")?,
        }));
    }
    if first == "list" {
        return match &args[1..] {
            [] => Ok(Command::List { json: false }),
            [s] if s == "--json" => Ok(Command::List { json: true }),
            _ => Err(usage("invalid list arguments")),
        };
    }
    if matches!(first.as_str(), "sync" | "doctor" | "run") {
        let host = args
            .get(1)
            .filter(|h| !h.starts_with('-'))
            .ok_or_else(|| usage("missing host"))?
            .clone();
        let mut force = false;
        let mut dry_run = false;
        let mut json = false;
        let mut run = RunOptions {
            host: host.clone(),
            ..Default::default()
        };
        let mut i = 2;
        while i < args.len() {
            match (first.as_str(), args[i].as_str()) {
                ("sync", "--force") => force = true,
                ("sync", "--dry-run") => dry_run = true,
                ("sync" | "doctor", "--json") => json = true,
                ("run", "--cwd") => run.cwd = Some(take(args, &mut i)?),
                ("run", "--no-tty") => run.no_tty = true,
                ("run", "--env") => {
                    let pair = take(args, &mut i)?;
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| usage("--env requires K=V"))?;
                    if !valid_env_key(key) {
                        return Err(usage("invalid environment name"));
                    }
                    run.env.insert(key.into(), value.into());
                }
                ("run", "--") => {
                    run.argv = args[i + 1..].to_vec();
                    break;
                }
                _ => return Err(usage("unknown option")),
            }
            i += 1;
        }
        return match first.as_str() {
            "sync" => Ok(Command::Sync {
                host,
                force,
                dry_run,
                json,
            }),
            "doctor" => Ok(Command::Doctor { host, json }),
            _ if !run.argv.is_empty() => Ok(Command::Run(run)),
            _ => Err(usage("run requires -- <argv...>")),
        };
    }
    if ["remote-attach", "remote-watch", "remote-run", "help"].contains(&first.as_str())
        || first.starts_with('-')
    {
        return Err(usage("invalid command"));
    }
    let mut opts = ConnectOptions {
        host: first.clone(),
        ..Default::default()
    };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cwd" => opts.cwd = Some(take(args, &mut i)?),
            "--no-sync" => opts.no_sync = true,
            "--" => {
                opts.ssh_args = args[i + 1..].to_vec();
                break;
            }
            _ => return Err(usage("unknown connect option")),
        }
        i += 1;
    }
    Ok(Command::Connect(opts))
}
fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

pub fn run(args: &[String]) -> io::Result<i32> {
    let result = run_inner(args);
    match result {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!("{error}");
            if error.kind() == io::ErrorKind::InvalidInput {
                eprintln!("{USAGE}");
                Ok(2)
            } else {
                Ok(1)
            }
        }
    }
}
fn run_inner(args: &[String]) -> io::Result<i32> {
    // Keep the stock disabled diagnostic even for an empty invocation. Helpers
    // and help are parsed first, because they must work without remote config.
    let hidden = args.first().is_some_and(|s| {
        matches!(
            s.as_str(),
            "remote-attach" | "remote-watch" | "remote-run" | "help" | "--help" | "-h"
        )
    });
    let cfg = if hidden {
        Default::default()
    } else {
        crate::config::Config::load().config.ssh_panes
    };
    if !hidden && !cfg.enabled && args.first().is_some_and(|s| s == "list") {
        let Command::List { json } = parse(args)? else {
            unreachable!()
        };
        println!(
            "{}",
            if json {
                r#"{"enabled":false,"links":[]}"#
            } else {
                "SSH panes are disabled"
            }
        );
        return Ok(0);
    }
    if !hidden && !cfg.enabled {
        eprintln!("{}", super::disabled_message());
        return Ok(2);
    }
    let command = parse(args)?;
    match command {
        Command::Help => {
            println!("{USAGE}");
            Ok(0)
        }
        Command::Attach(spec) => remote::remote_attach(&spec),
        Command::Watch(args) => remote::remote_watch(args),
        Command::RemoteRun(spec, argv) => remote::remote_run(&spec, &argv),
        Command::Connect(opts) => connector::connect(opts, &cfg),
        Command::Run(opts) => connector::run_remote(opts, &cfg),
        #[cfg(unix)]
        Command::Sync {
            host,
            force,
            dry_run,
            json,
        } => {
            connector::check_host(&host, &cfg)?;
            let report = sync::sync_payload(&host, &cfg, force, dry_run)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&report).map_err(io::Error::other)?
                );
            } else {
                println!(
                    "{}",
                    if !report.changed {
                        "unchanged"
                    } else if dry_run {
                        "would sync"
                    } else {
                        "synced"
                    }
                );
            }
            Ok(0)
        }
        #[cfg(unix)]
        Command::Doctor { host, json } => {
            connector::check_host(&host, &cfg)?;
            let report = sync::doctor(&host, &cfg)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&report).map_err(io::Error::other)?
                );
            } else {
                for line in &report.diagnostics {
                    println!("{line}");
                }
            }
            Ok(if report.ok { 0 } else { 1 })
        }
        #[cfg(unix)]
        Command::List { json } => {
            let links = connector::rpc(
                &crate::api::socket_path(),
                super::api::SshPanesRequest::List(super::api::ListParams::default()),
            )?;
            if json {
                println!("{links}");
            } else if let Some(items) = links["links"].as_array() {
                for link in items {
                    println!(
                        "{} {} {} {}",
                        link["pane_id"].as_str().unwrap_or(""),
                        link["host"].as_str().unwrap_or(""),
                        link["state"].as_str().unwrap_or(""),
                        link["cwd"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        #[cfg(not(unix))]
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ssh-panes: not supported on this platform",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: &[&str]) -> Vec<String> {
        value.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ssh_panes_cli_preserves_remote_arguments_and_rejects_usage_errors() {
        let Command::Connect(opts) = parse(&args(&[
            "studio",
            "--cwd",
            "/a ' b",
            "--no-sync",
            "--",
            "-p",
            "2222",
        ]))
        .unwrap() else {
            panic!("expected connect")
        };
        assert_eq!(opts.cwd.as_deref(), Some("/a ' b"));
        assert!(opts.no_sync);
        assert_eq!(opts.ssh_args, ["-p", "2222"]);
        let Command::Run(opts) = parse(&args(&[
            "run",
            "studio",
            "--env",
            "A=@root/a b",
            "--no-tty",
            "--",
            "herdr",
            "--version",
        ]))
        .unwrap() else {
            panic!("expected run")
        };
        assert_eq!(opts.env["A"], "@root/a b");
        assert_eq!(opts.argv, ["herdr", "--version"]);
        assert!(opts.no_tty);
        for input in [
            vec![],
            vec!["sync"],
            vec!["studio", "--cwd"],
            vec!["list", "studio"],
            vec!["run", "studio", "--"],
            vec!["run", "studio", "--env", "1BAD=x", "--", "true"],
            vec!["-bad"],
            vec!["studio", "unexpected"],
        ] {
            assert!(parse(&args(&input)).is_err(), "{input:?}");
        }
        assert!(matches!(parse(&args(&["help"])).unwrap(), Command::Help));
    }
}
