//! `lafdctl` - command line client for `lafd`.

use lafd::config::{default_socket_path, Config};
use lafd::protocol::{Request, Response, Status};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let mut socket: Option<String> = None;
    let mut json = false;
    let mut args = std::env::args().skip(1).peekable();

    while let Some(arg) = args.peek() {
        match arg.as_str() {
            "-s" | "--socket" => {
                args.next();
                socket = args.next();
            }
            "-j" | "--json" => {
                args.next();
                json = true;
            }
            "-V" | "--version" => {
                println!("lafdctl {VERSION}");
                return 0;
            }
            "-h" | "--help" => {
                usage();
                return 0;
            }
            _ => break,
        }
    }

    let command: Vec<String> = args.collect();
    if command.is_empty() {
        usage();
        return 2;
    }

    let socket = socket.unwrap_or_else(|| {
        Config::load(None)
            .map(|cfg| cfg.socket_path)
            .unwrap_or_else(|_| default_socket_path())
    });

    let request = match build_request(&command) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("lafdctl: {message}");
            return 2;
        }
    };

    let response = match send(&socket, &request) {
        Ok(response) => response,
        Err(message) => {
            eprintln!("lafdctl: {message}");
            return 1;
        }
    };

    if json {
        match serde_json::to_string_pretty(&response) {
            Ok(data) => println!("{data}"),
            Err(err) => eprintln!("lafdctl: {err}"),
        }
        return if response.ok { 0 } else { 1 };
    }

    if !response.ok {
        eprintln!("lafdctl: {}", response.error.unwrap_or_default());
        return 1;
    }

    match response.result {
        Some(value) if value.get("pong").is_some() => println!("pong"),
        Some(value) => match serde_json::from_value::<Status>(value.clone()) {
            Ok(status) => print_status(&status),
            Err(_) => println!("{value}"),
        },
        None => {}
    }
    0
}

fn build_request(command: &[String]) -> Result<Request, String> {
    match command[0].as_str() {
        "status" => Ok(Request {
            cmd: "status".to_string(),
            ..Request::default()
        }),
        "ping" => Ok(Request {
            cmd: "ping".to_string(),
            ..Request::default()
        }),
        "trigger" => match command.get(1).map(String::as_str) {
            Some("start") => {
                let mut timeout_ms = None;
                let mut name_parts = Vec::new();
                let mut index = 2;
                while index < command.len() {
                    let arg = &command[index];
                    if arg == "--timeout" || arg == "-t" {
                        index += 1;
                        let value = command
                            .get(index)
                            .ok_or("--timeout needs a value, for example 30s")?;
                        timeout_ms = Some(parse_duration_ms(value)?);
                    } else if let Some(value) = arg.strip_prefix("--timeout=") {
                        timeout_ms = Some(parse_duration_ms(value)?);
                    } else {
                        name_parts.push(arg.clone());
                    }
                    index += 1;
                }
                if name_parts.is_empty() {
                    return Err("usage: lafdctl trigger start [--timeout 30s] <name>".to_string());
                }
                Ok(Request {
                    cmd: "trigger_start".to_string(),
                    name: Some(name_parts.join(" ")),
                    timeout_ms,
                })
            }
            Some("end") => {
                let name = command[2..].join(" ");
                Ok(Request {
                    cmd: "trigger_end".to_string(),
                    name: if name.is_empty() { None } else { Some(name) },
                    timeout_ms: None,
                })
            }
            _ => Err("usage: lafdctl trigger start|end ...".to_string()),
        },
        other => Err(format!(
            "unknown command {other:?} (want status, trigger start|end or ping)"
        )),
    }
}

fn parse_duration_ms(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(rest) = value.strip_suffix("ms") {
        (rest, 1.0)
    } else if let Some(rest) = value.strip_suffix('s') {
        (rest, 1000.0)
    } else if let Some(rest) = value.strip_suffix('m') {
        (rest, 60_000.0)
    } else {
        (value, 1000.0)
    };

    let number: f64 = number
        .parse()
        .map_err(|_| format!("invalid duration {value:?} (use 30s, 500ms or 2m)"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("invalid duration {value:?}"));
    }
    Ok((number * multiplier) as u64)
}

fn send(socket: &str, request: &Request) -> Result<Response, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|err| format!("connect to {socket}: {err} (is lafd.service running?)"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

    let data = serde_json::to_string(request).map_err(|err| err.to_string())?;
    writeln!(stream, "{data}").map_err(|err| format!("send request: {err}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|err| format!("read response: {err}"))?;
    serde_json::from_str(&line).map_err(|err| format!("invalid response: {err}"))
}

fn print_status(status: &Status) {
    match &status.focus {
        None => println!("focus: none"),
        Some(focus) => {
            let state = if focus.locked { "locked" } else { "auto" };
            println!(
                "focus: {} [{}, {}] for {}s ({})",
                focus.owner, focus.mode, state, focus.since_secs, focus.reason
            );
        }
    }

    if !status.paused_by_service.is_empty() {
        println!("paused by service: {}", status.paused_by_service.join(", "));
    }

    if status.players.is_empty() {
        println!("no MPRIS players");
        return;
    }

    let width = status
        .players
        .iter()
        .map(|player| player.identity.chars().count())
        .max()
        .unwrap_or(0)
        .min(28);

    for player in &status.players {
        let marker = if player.is_focus { "*" } else { " " };
        let mut line = format!(
            "{marker} {:<8} {:<width$} {}",
            player.status,
            player.identity,
            player.name,
            width = width
        );
        if player.paused_by_service {
            line.push_str(" [paused by service]");
        }
        println!("{}", line.trim_end());
    }
}

fn usage() {
    println!(
        "lafdctl {VERSION} - control the Linux audio focus daemon

Usage:
  lafdctl [options] <command> [args]

Commands:
  status                              Show focus owner and MPRIS players
  trigger start [--timeout 30s] NAME  Pause everything until trigger end
  trigger end [NAME]                  End the trigger and resume paused players
  ping                                Check that the daemon is alive

Options:
  -s, --socket PATH   Override the daemon socket path
  -j, --json          Print the raw JSON response
  -V, --version       Print version and exit
  -h, --help          Print this help and exit

Examples:
  lafdctl status
  lafdctl trigger start --timeout 45s call
  lafdctl trigger end call
  lafdctl trigger start --timeout 30s screen-recording"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds() {
        assert_eq!(parse_duration_ms("30s"), Ok(30_000));
        assert_eq!(parse_duration_ms("1.5s"), Ok(1500));
        assert_eq!(parse_duration_ms("500ms"), Ok(500));
        assert_eq!(parse_duration_ms("2m"), Ok(120_000));
        assert_eq!(parse_duration_ms("7"), Ok(7000));
    }

    #[test]
    fn rejects_bad_duration() {
        assert!(parse_duration_ms("soon").is_err());
        assert!(parse_duration_ms("-5s").is_err());
    }

    #[test]
    fn builds_trigger_start() {
        let command = vec![
            "trigger".to_string(),
            "start".to_string(),
            "--timeout".to_string(),
            "45s".to_string(),
            "phone".to_string(),
            "call".to_string(),
        ];
        let request = build_request(&command).expect("request");
        assert_eq!(request.cmd, "trigger_start");
        assert_eq!(request.name.as_deref(), Some("phone call"));
        assert_eq!(request.timeout_ms, Some(45_000));
    }

    #[test]
    fn builds_trigger_end_without_name() {
        let command = vec!["trigger".to_string(), "end".to_string()];
        let request = build_request(&command).expect("request");
        assert_eq!(request.cmd, "trigger_end");
        assert_eq!(request.name, None);
    }

    #[test]
    fn rejects_trigger_start_without_name() {
        let command = vec![
            "trigger".to_string(),
            "start".to_string(),
            "--timeout".to_string(),
            "30s".to_string(),
        ];
        assert!(build_request(&command).is_err());
    }
}
