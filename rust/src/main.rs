//! lago: command-line client for the rofication notification daemon.
//!
//! Lists notifications (`ls`), deletes one by id or all of one application
//! (`rm`) and empties the queue (`clear`), talking to the daemon over a Unix
//! stream socket. Standard library only.

#![forbid(unsafe_code)]

mod args;
mod client;
mod json;
mod notification;
mod strtoll;

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;

use client::{Client, ClientError};

const DEFAULT_SOCKET: &[u8] = b"/tmp/rofi_notification_daemon";

const VERSION_TEXT: &[u8] = b"lago 0.1.0 (C) 2021 Ken Gilmer\n";

/// The help text, byte for byte: every line keeps its eight-space indent,
/// blank lines included, and a final newline follows the indented last line.
const USAGE_TEXT: &str = concat!(
    "\n",
    "        Usage:\n",
    "        lago [ ls | rm <id> | rm <app> | clear ]\n",
    "        \n",
    "        Help Options:\n",
    "        -h, --help                           Show help options\n",
    "        \n",
    "        Application Options:\n",
    "        -v, --version                        Display version number\n",
    "        -s <Socket URI>                      Socket path to connect to notification server\n",
    "        ",
    "\n",
);

fn main() {
    let raw_args: Vec<Vec<u8>> = std::env::args_os().map(OsString::into_vec).collect();
    let code = run(&raw_args);
    std::process::exit(code);
}

/// Writes `bytes` to stdout and flushes, ignoring failures (the daemon
/// traffic has already happened by then; there is nothing left to report).
fn print_stdout(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn print_stderr(bytes: &[u8]) {
    let mut err = io::stderr().lock();
    let _ = err.write_all(bytes);
    let _ = err.flush();
}

fn print_usage() {
    print_stdout(USAGE_TEXT.as_bytes());
}

fn run(raw_args: &[Vec<u8>]) -> i32 {
    let options = args::parse_args(raw_args);

    if options.contains(b"-v") || options.contains(b"--version") {
        print_stdout(VERSION_TEXT);
        return 0;
    }

    if options.contains(b"-h") || options.contains(b"--help") || raw_args.len() < 2 {
        print_usage();
        return 0;
    }

    if options.contains(b"-s") && options.lookup(b"-s").is_none() {
        let program = raw_args.first().map(Vec::as_slice).unwrap_or(b"");
        let mut msg = Vec::new();
        msg.extend_from_slice(b"error: option -s requires a value\n");
        msg.extend_from_slice(b"Run '");
        msg.extend_from_slice(program);
        msg.extend_from_slice(b" --help' to see a full list of available command line options.\n");
        print_stderr(&msg);
        return 1;
    }

    let socket_path: Vec<u8> = options
        .lookup(b"-s")
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| DEFAULT_SOCKET.to_vec());

    let positional = args::positional(raw_args);
    let command = match positional.command {
        Some(command) => command,
        None => {
            print_usage();
            return 1;
        }
    };

    let client = Client::new(&socket_path);
    match dispatch(&client, &command, positional.rm_operand.as_deref()) {
        Ok(code) => code,
        Err(ClientError::BadReply) => {
            let mut msg = b"lago: invalid reply from ".to_vec();
            msg.extend_from_slice(&socket_path);
            msg.push(b'\n');
            print_stderr(&msg);
            1
        }
        Err(ClientError::Io(reason)) => {
            let mut msg = b"lago: cannot connect to ".to_vec();
            msg.extend_from_slice(&socket_path);
            msg.extend_from_slice(b": ");
            msg.extend_from_slice(reason.as_bytes());
            msg.push(b'\n');
            print_stderr(&msg);
            1
        }
    }
}

fn dispatch(
    client: &Client,
    command: &[u8],
    rm_operand: Option<&[u8]>,
) -> Result<i32, ClientError> {
    match command {
        b"ls" => {
            let notifications = client.get_notifications()?;
            let mut listing = Vec::new();
            for notification in &notifications {
                notification.render(&mut listing);
            }
            print_stdout(&listing);
            Ok(0)
        }
        b"rm" => match rm_operand {
            Some(operand) => {
                let id = strtoll::parse_int64(operand);
                if id == 0 {
                    client.delete_notification_by_app(operand)?;
                } else {
                    client.delete_notification_by_id(id)?;
                }
                Ok(0)
            }
            None => {
                print_usage();
                Ok(0)
            }
        },
        b"clear" => {
            let notifications = client.get_notifications()?;
            for notification in &notifications {
                client.delete_notification_by_id(notification.id)?;
            }
            Ok(0)
        }
        _ => {
            print_usage();
            Ok(1)
        }
    }
}
