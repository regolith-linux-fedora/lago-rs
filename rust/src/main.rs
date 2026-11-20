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
use std::process::ExitCode;

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

fn main() -> ExitCode {
    let raw_args: Vec<Vec<u8>> = std::env::args_os().map(OsString::into_vec).collect();
    let mut output = Output {
        out: io::stdout().lock(),
        err: io::stderr().lock(),
    };
    ExitCode::from(run(&raw_args, &mut output))
}

/// lago's stdout and stderr, generic so that tests can put failing writers
/// in their place.
///
/// Every command writes at most once, after its daemon traffic is over, so a
/// failed write only decides the exit status; there is nothing left to stop.
struct Output<O, E> {
    out: O,
    err: E,
}

impl<O: Write, E: Write> Output<O, E> {
    /// Prints `bytes` on stdout for a command that exits with `status`, and
    /// returns the status lago exits with.
    ///
    /// A reader that has gone away (`lago ls | head -1`) is not an error:
    /// lago stops writing and keeps `status`, without a message. Any other
    /// failure, such as a full disk, is reported on stderr and gives status 1.
    fn print(&mut self, bytes: &[u8], status: u8) -> u8 {
        match write_flushed(&mut self.out, bytes) {
            Ok(()) => status,
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => status,
            Err(err) => {
                let mut msg = b"lago: cannot write to stdout: ".to_vec();
                msg.extend_from_slice(client::reason(&err).as_bytes());
                msg.push(b'\n');
                self.fail(&msg)
            }
        }
    }

    fn print_usage(&mut self, status: u8) -> u8 {
        self.print(USAGE_TEXT.as_bytes(), status)
    }

    /// Reports a failure on stderr and returns exit status 1. When stderr
    /// cannot take the message either (no reader, a full disk), there is
    /// nowhere left to report that; the status still says lago failed.
    fn fail(&mut self, msg: &[u8]) -> u8 {
        let _ = write_flushed(&mut self.err, msg);
        1
    }
}

fn write_flushed(mut stream: impl Write, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes)?;
    stream.flush()
}

fn run(raw_args: &[Vec<u8>], output: &mut Output<impl Write, impl Write>) -> u8 {
    let options = args::parse_args(raw_args);

    if options.contains(b"-v") || options.contains(b"--version") {
        return output.print(VERSION_TEXT, 0);
    }

    if options.contains(b"-h") || options.contains(b"--help") || raw_args.len() < 2 {
        return output.print_usage(0);
    }

    if options.contains(b"-s") && options.lookup(b"-s").is_none() {
        let program = raw_args.first().map(Vec::as_slice).unwrap_or(b"");
        let mut msg = Vec::new();
        msg.extend_from_slice(b"error: option -s requires a value\n");
        msg.extend_from_slice(b"Run '");
        msg.extend_from_slice(program);
        msg.extend_from_slice(b" --help' to see a full list of available command line options.\n");
        return output.fail(&msg);
    }

    let socket_path: Vec<u8> = options
        .lookup(b"-s")
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| DEFAULT_SOCKET.to_vec());

    let positional = args::positional(raw_args);
    let command = match positional.command {
        Some(command) => command,
        None => return output.print_usage(1),
    };

    let client = Client::new(&socket_path);
    match dispatch(&client, &command, positional.rm_operand.as_deref(), output) {
        Ok(code) => code,
        Err(ClientError::BadReply) => {
            let mut msg = b"lago: invalid reply from ".to_vec();
            msg.extend_from_slice(&socket_path);
            msg.push(b'\n');
            output.fail(&msg)
        }
        Err(ClientError::Io(reason)) => {
            let mut msg = b"lago: cannot connect to ".to_vec();
            msg.extend_from_slice(&socket_path);
            msg.extend_from_slice(b": ");
            msg.extend_from_slice(reason.as_bytes());
            msg.push(b'\n');
            output.fail(&msg)
        }
    }
}

fn dispatch(
    client: &Client,
    command: &[u8],
    rm_operand: Option<&[u8]>,
    output: &mut Output<impl Write, impl Write>,
) -> Result<u8, ClientError> {
    match command {
        b"ls" => {
            let notifications = client.get_notifications()?;
            let mut listing = Vec::new();
            for notification in &notifications {
                notification.render(&mut listing);
            }
            Ok(output.print(&listing, 0))
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
            None => Ok(output.print_usage(0)),
        },
        b"clear" => {
            let notifications = client.get_notifications()?;
            for notification in &notifications {
                client.delete_notification_by_id(notification.id)?;
            }
            Ok(0)
        }
        _ => Ok(output.print_usage(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::{run, Output, USAGE_TEXT, VERSION_TEXT};
    use std::io::{self, Write};

    const EPIPE: i32 = 32;
    const ENOSPC: i32 = 28;

    /// A stream that takes `room` bytes and then fails every write with
    /// the system error `errno`.
    struct Stream {
        taken: Vec<u8>,
        room: usize,
        errno: i32,
    }

    impl Stream {
        fn working() -> Stream {
            Stream::failing(usize::MAX, 0)
        }

        fn failing(room: usize, errno: i32) -> Stream {
            Stream {
                taken: Vec::new(),
                room,
                errno,
            }
        }
    }

    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = buf.len().min(self.room - self.taken.len());
            if n == 0 && !buf.is_empty() {
                return Err(io::Error::from_raw_os_error(self.errno));
            }
            self.taken.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Runs lago with `args` on the given streams; returns the exit status
    /// and what each stream took.
    fn lago(args: &[&str], out: Stream, err: Stream) -> (u8, Vec<u8>, Vec<u8>) {
        let raw_args: Vec<Vec<u8>> = std::iter::once("lago")
            .chain(args.iter().copied())
            .map(|arg| arg.as_bytes().to_vec())
            .collect();
        let mut output = Output { out, err };
        let status = run(&raw_args, &mut output);
        (status, output.out.taken, output.err.taken)
    }

    #[test]
    fn working_streams() {
        let (status, out, err) = lago(&["-v"], Stream::working(), Stream::working());
        assert_eq!(
            (status, out.as_slice(), err.as_slice()),
            (0, VERSION_TEXT, &b""[..])
        );
    }

    #[test]
    fn stdout_reader_gone_keeps_the_status_quietly() {
        for (args, expected) in [(&["-v"][..], 0), (&["-h"], 0), (&["rm"], 0), (&["foo"], 1)] {
            let (status, out, err) = lago(args, Stream::failing(0, EPIPE), Stream::working());
            assert_eq!((status, out, err), (expected, vec![], vec![]), "{args:?}");
        }
    }

    #[test]
    fn stdout_reader_stops_partway() {
        let (status, out, err) = lago(&["-h"], Stream::failing(10, EPIPE), Stream::working());
        assert_eq!(status, 0);
        assert_eq!(out, USAGE_TEXT.as_bytes()[..10]);
        assert_eq!(err, b"");
    }

    #[test]
    fn other_stdout_failures_are_reported() {
        for (args, room) in [(&["-v"][..], 0), (&["-h"], 10), (&["foo"], 0)] {
            let (status, _, err) = lago(args, Stream::failing(room, ENOSPC), Stream::working());
            assert_eq!(status, 1, "{args:?}");
            assert_eq!(
                err, b"lago: cannot write to stdout: No space left on device\n",
                "{args:?}"
            );
        }
    }

    #[test]
    fn stderr_failures_keep_status_1() {
        let no_daemon = "/nonexistent/lago-test-socket";
        for errno in [EPIPE, ENOSPC] {
            for args in [&["ls", "-s"][..], &["ls", "-s", no_daemon]] {
                let (status, out, err) = lago(args, Stream::working(), Stream::failing(0, errno));
                assert_eq!((status, out, err), (1, vec![], vec![]), "{args:?} {errno}");
            }
            // A failed report of a failed stdout write changes nothing either.
            let (status, _, _) = lago(
                &["-v"],
                Stream::failing(0, ENOSPC),
                Stream::failing(0, errno),
            );
            assert_eq!(status, 1);
        }
    }
}
