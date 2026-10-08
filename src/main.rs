mod chicken;
mod commands;
mod resp;
mod server;
mod store;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
chicken-kv: a Redis-compatible key-value server, partly written in Chicken

USAGE:
    chicken-kv [OPTIONS]                  start the server
    chicken-kv run <file.ch> [input]      run a Chicken program and print its output

OPTIONS:
    --port <N>             port to listen on (default 6379)
    --bind <ADDR>          address to listen on (default 127.0.0.1)
    --snapshot <PATH>      snapshot file (default dump.ckv)
    --save-interval <S>    seconds between automatic snapshots, 0 = off (default 60)
    -h, --help             show this message";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    if args.first().map(String::as_str) == Some("run") {
        return run_script(&args[1..]);
    }

    match parse_config(&args) {
        Ok(config) => match server::run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn parse_config(args: &[String]) -> Result<server::Config, String> {
    let mut config = server::Config {
        bind: "127.0.0.1".into(),
        port: 6379,
        snapshot_path: PathBuf::from("dump.ckv"),
        save_interval: 60,
    };

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        // Accept both `--port 7000` and `--port=7000`.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--port" => config.port = value()?.parse().map_err(|_| "invalid --port".to_string())?,
            "--bind" => config.bind = value()?,
            "--snapshot" => config.snapshot_path = PathBuf::from(value()?),
            "--save-interval" => {
                config.save_interval = value()?.parse().map_err(|_| "invalid --save-interval".to_string())?
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(config)
}

/// Runs a standalone Chicken program in the original interpreter's
/// JS-compatible mode, for experimenting with Chicken outside the server.
fn run_script(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: chicken-kv run <file.ch> [input]");
        return ExitCode::from(2);
    };
    let input = args.get(1).map(String::as_str).unwrap_or("");
    let code = match std::fs::read_to_string(path) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("failed to read '{path}': {e}");
            return ExitCode::FAILURE;
        }
    };
    match chicken::run(&code, input, true) {
        Ok(output) => {
            println!("{}", chicken::decode_entities(&output));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
