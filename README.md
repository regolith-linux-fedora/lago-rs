# lago-rs

A Rust port of [lago](https://github.com/regolith-linux/lago), the command-line client for
Regolith's [rofication](https://github.com/regolith-linux/regolith-rofication) notification
daemon. lago lists notifications, dismisses one by id or all of one application, and clears the
queue, talking to `rofication-daemon` over its Unix socket.

## Status

Experimental port; not a drop-in replacement yet.

- **Snapshot.** The port reproduces lago as of
  [`3c9a70d`](https://github.com/regolith-linux/lago/commit/3c9a70dd2d3e3ead32d6e8c29e0d4d804fb09e52)
  (tag `v0.2.0-2` plus the packaging fixes merged on 2026-10-27), with the client fixes listed
  under "What is different". It is feature-complete for that snapshot and was verified against a
  build of the original (see "How it was verified").
- **Fixes still to port.** lago's `src/` has not changed upstream since that snapshot. These are
  known bugs of the original that the port still reproduces on purpose; they need porting once
  they are fixed upstream (this list may not be complete):
  - `rm <app>` with a name that starts with a digit (`rm 1Password`) deletes notification #1.
  - `rm "<app with ':'>"` deletes a different application when the daemon splits the request at
    every colon. The Python rofication does that up to `2e0908e`; rofication-rs does not.
  - No timeout: a stopped (not exited) daemon makes `ls` and `clear` wait forever.
  - Usage and exit codes: bare `rm` prints the usage and exits 0; `rm <unknown id>` exits 0
    without a message.
  - `--version` prints `0.1.0`; the package is 0.2.0.
  - `ls` prints multi-line bodies unindented.
- **Testing.** Very deep testing is needed before anyone considers it as a replacement for the
  original.
- **Reviews.** It has not yet had deep code reviews by the major AI coding agents and reviewers.
  Those reviews are planned before any replacement decision.
- **Upstreaming** into regolith-linux is undecided.

## What is identical

The command line (options anywhere, `-s <socket>`, `-h`, `-v`), the help and version text, the
request bytes sent to the daemon (`list`, `del:<id>`, `dela:<app>`), the `ls` output, the `rm`
operand parsing (C `strtoll` with base 0, like the original's `int64.parse`), the JSON parser's
acceptance rules (a port of json-glib's non-strict parser), the package name and the installed
file (`/usr/bin/lago`).

## What is different

Compared with lago `3c9a70d`, which aborts with a core dump in each of these cases:

- `ls` and `clear` read the daemon's reply until it closes the connection, however many reads it
  takes. The original made one receive into a fixed 512 KiB buffer, so a reply that arrived in
  several pieces (for example, with a notification carrying a 128x128 image) failed to parse.
- A daemon that is not running, or a reply that is not a JSON array, gives
  `lago: cannot connect to <socket>: <reason>` or `lago: invalid reply from <socket>` and exit
  status 1.
- `-s` works before or after the command; `-s` without a value is an error (exit status 1).
- `clear` with 8 or more notifications deletes all of them. The original's non-blocking connect
  failed once the daemon's listen backlog was full.

Output errors:

- A reader that goes away (`lago ls | head -1`, `lago -h | true`) is not an error. lago stops
  writing and exits with the status the command has anyway: 0 after a listing, the help or the
  version, 1 when it printed the usage for a wrong command line. It prints no message and is never
  ended by SIGPIPE. Under `set -o pipefail`, `lago ls | head -1` therefore succeeds. ripgrep and
  bat do the same. The original is ended by SIGPIPE (status 141 in a shell) for its help and
  version text. For `ls` it exits 0, because GLib's socket code has set SIGPIPE to ignored by then.
- Any other failure to write stdout, such as a full disk, gives
  `lago: cannot write to stdout: <reason>` and exit status 1. The original ignores it and exits 0.
- A failure to write stderr can't be reported anywhere. lago keeps status 1 for the failure it was
  trying to report.

Packaging: Build-Depends are `cargo, rustc, meson, ninja-build, python3` instead of valac and the
GLib/json-glib headers. The binary links only libc and libgcc.

## Building

The crate is in `rust/` and uses the standard library only (no crates.io dependencies).

```
cargo build --release --locked --manifest-path rust/Cargo.toml   # rust/target/release/lago
cargo test --locked --manifest-path rust/Cargo.toml              # unit tests
```

meson drives the same build and runs the regression suite:

```
meson setup build
ninja -C build                # build/src/lago
meson test -C build           # test/lago_regress.py: 174 cases against a fake daemon
```

Debian package (runs the meson build and the suite):

```
dpkg-buildpackage -b -us -uc
```

It builds on Ubuntu 26.04 and 24.04 with the distribution's `cargo` and `rustc` (Rust 1.75 or
newer).

## How it was verified

- `cargo test`: 25 unit tests, including a list reply split across reads, a 1.5 MB reply, and
  stdout and stderr writers that fail with EPIPE or ENOSPC.
- The regression suite: 174 cases checking exit status, stdout, stderr and the exact request
  bytes of every connection. 27 of them put stdout or stderr on a pipe with no reader, on a
  reader that stops after 4 KiB, or on `/dev/full`.
- A differential suite against a build of the original Vala lago with the fixes above (1078
  checks: 600 seeded command lines and replies, 434 recorded outputs, protocol, rendering and
  robustness cases, the real rofication daemon, packaging). Every behaviour check passes. The
  remaining differences are intended: the install payload (lago `3c9a70d` no longer installs two
  empty `CMakeLists.txt` files), the removed `po/` directory (lago has no translatable strings),
  and the version line, which names the author as the original does.
- `dpkg-buildpackage -b` on Ubuntu 26.04 and 24.04; the package installs on both.
- An installed-package session test in headless sway on Ubuntu 26.04 with rofication-rs: `ls`,
  `rm <id>`, `rm "Chat: Work"`, a list reply above 300 KB, and `clear` with 16 notifications.

## Usage

```
lago -h

        Usage:
        lago [ ls | rm <id> | rm <app> | clear ]

        Help Options:
        -h, --help                           Show help options

        Application Options:
        -v, --version                        Display version number
        -s <Socket URI>                      Socket path to connect to notification server
```

- `lago ls`: list notifications.
- `lago rm 4`: dismiss notification 4.
- `lago rm Slack`: dismiss all notifications from Slack.
- `lago clear`: dismiss all notifications.

## License

Apache-2.0, like the original lago. See `LICENSE`.
