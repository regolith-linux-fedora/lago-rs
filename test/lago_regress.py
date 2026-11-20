#!/usr/bin/env python3
"""Regression suite for lago, run by `meson test`.

Usage: lago_regress.py <path-to-lago>

Every case runs the given binary (argv[0] pinned to "lago") against a fake
rofication daemon started here on a private Unix socket, then compares the
exit status, stdout, stderr and the daemon transcript (the request bytes of
every connection, in order) with the expected values. Expected values are
the behaviour of the released client; "{S}" stands for the per-case socket
path in argv, stderr and requests.

The broken-pipe cases run with stdout or stderr connected to a pipe whose
reader is gone, a reader that stops early, or /dev/full. A reader that went
away is not an error: lago stops writing, prints nothing about it and exits
with the command's own status, never by a signal. Any other failure to write
stdout is reported on stderr with status 1.

The fake daemon reads one request per connection. A `list` request gets the
scripted reply (whole, in chunks, or slowly) and the connection is closed;
`del`/`dela` requests are read to EOF and never answered, like the real
daemon.
"""

import os
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

LAGO = None  # set from argv in main()

USAGE = (
    b"\n"
    b"        Usage:\n"
    b"        lago [ ls | rm <id> | rm <app> | clear ]\n"
    b"        \n"
    b"        Help Options:\n"
    b"        -h, --help                           Show help options\n"
    b"        \n"
    b"        Application Options:\n"
    b"        -v, --version                        Display version number\n"
    b"        -s <Socket URI>                      Socket path to connect to"
    b" notification server\n"
    b"        \n"
)
VERSION = b"lago 0.1.0 (C) 2021 Ken Gilmer\n"
S_NO_VALUE = (
    b"error: option -s requires a value\n"
    b"Run 'lago --help' to see a full list of available command line"
    b" options.\n"
)
BAD_REPLY = b"lago: invalid reply from {S}\n"


def no_conn_error(reason):
    return b"lago: cannot connect to {S}: " + reason + b"\n"


def row(nid, app, urgency, summary, body):
    """One `ls` record."""
    return (b"[" + nid + b"] " + app + b" (" + urgency + b") - " + summary
            + b" \n\t" + body + b"\n")


NULL_ROW = row(b"0", b"(null)", b"Low", b"(null)", b"(null)")


def note(nid=None, summary=None, body=None, application=None, urgency=None,
         raw_id=None, raw_urgency=None):
    """A notification object; raw_* insert number text verbatim."""
    parts = []
    if raw_id is not None:
        parts.append(b'"id":' + raw_id)
    elif nid is not None:
        parts.append(b'"id":' + str(nid).encode())
    for key, value in (("summary", summary), ("body", body),
                       ("application", application)):
        if value is not None:
            parts.append(b'"' + key.encode() + b'":"' + value + b'"')
    if raw_urgency is not None:
        parts.append(b'"urgency":' + raw_urgency)
    elif urgency is not None:
        parts.append(b'"urgency":' + str(urgency).encode())
    return b"{" + b",".join(parts) + b"}"


def full(nid=42, urgency=1):
    return note(nid, b"hello", b"world", b"myapp", urgency)


FULL = b"[" + full() + b"]"
FULL_ROW = row(b"42", b"myapp", b"Normal", b"hello", b"world")
THREE = b"[" + b",".join(full(n) for n in (3, 1, 2)) + b"]"


# ---------------------------------------------------------------------------
# Fake daemon

def reply(data, chunk=0, delay=0.0):
    return {"kind": "reply", "reply": data, "chunk": chunk, "delay": delay}


NOREPLY = {"kind": "noreply"}
EARLY = {"kind": "early"}


class FakeDaemon:
    def __init__(self, path, behaviors):
        self.path = path
        self.behaviors = list(behaviors)
        self.requests = []
        self.errors = []
        self._lock = threading.Lock()
        self._index = 0
        self._handlers = []
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(path)
        self.sock.listen(64)
        self.sock.setblocking(False)
        self._wake_r, self._wake_w = os.pipe()
        self._thread = threading.Thread(target=self._accept_loop, daemon=True)
        self._thread.start()

    def _accept_pending(self):
        """Accepts every queued connection; returns when none is left."""
        while True:
            try:
                conn, _ = self.sock.accept()
            except (BlockingIOError, InterruptedError):
                return
            except OSError:
                return
            conn.setblocking(True)
            with self._lock:
                index = self._index
                self._index += 1
                self.requests.append(None)
            behavior = (self.behaviors[index] if index < len(self.behaviors)
                        else NOREPLY)
            handler = threading.Thread(target=self._handle,
                                       args=(conn, index, behavior),
                                       daemon=True)
            self._handlers.append(handler)
            handler.start()

    def _accept_loop(self):
        while True:
            ready, _, _ = select.select([self.sock, self._wake_r], [], [])
            self._accept_pending()
            if self._wake_r in ready:
                # Stopped: the client has exited, so whatever it connected
                # is already queued and was taken just above.
                return

    def _handle(self, conn, index, behavior):
        request = b""
        try:
            conn.settimeout(10)
            if behavior["kind"] != "early":
                while not request.endswith(b"\n"):
                    piece = conn.recv(4096)
                    if not piece:
                        break
                    request += piece
                if request.startswith(b"del"):
                    while True:
                        piece = conn.recv(4096)
                        if not piece:
                            break
                        request += piece
            if behavior["kind"] == "reply":
                data, chunk = behavior["reply"], behavior["chunk"]
                pieces = ([data[i:i + chunk] for i in range(0, len(data), chunk)]
                          if chunk > 0 else [data])
                for piece in pieces:
                    conn.sendall(piece)
                    if behavior["delay"]:
                        time.sleep(behavior["delay"])
        except OSError as exc:
            self.errors.append(f"connection {index}: {exc!r}")
        finally:
            conn.close()
            with self._lock:
                self.requests[index] = request

    def close(self):
        os.write(self._wake_w, b"x")
        self._thread.join(10)
        for handler in self._handlers:
            handler.join(10)
        os.close(self._wake_r)
        os.close(self._wake_w)
        self.sock.close()
        try:
            os.unlink(self.path)
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Cases
#
# (name, argv, behaviors, rc, stdout, stderr, requests)
#   behaviors None: no daemon at all ({S} names a missing path).
#   requests: expected request bytes per connection, in order.

def ls(reply_bytes, out, **kw):
    return (["ls", "-s", "{S}"], [reply(reply_bytes, **kw)], 0, out, b"",
            [b"list\n"])


def rejected(reply_bytes, command="ls"):
    return ([command, "-s", "{S}"], [reply(reply_bytes)], 1, b"", BAD_REPLY,
            [b"list\n"])


def rm(operand, request):
    return (["rm", operand, "-s", "{S}"], [], 0, b"", b"", [request])


BIG = b"z" * (600 * 1024)
MANY = b"[" + b",".join(
    note(i, b"summary-%d padded to widen the reply" % i, b"body", b"app", i % 3)
    for i in range(200)) + b"]"
MANY_OUT = b"".join(
    row(b"%d" % i, b"app", (b"Low", b"Normal", b"Critical")[i % 3],
        b"summary-%d padded to widen the reply" % i, b"body")
    for i in range(200))

CASES = [
    # --- command line --------------------------------------------------
    ("no-args", [], [], 0, USAGE, b"", []),
    ("help", ["-h"], [], 0, USAGE, b"", []),
    ("help-long", ["--help"], [], 0, USAGE, b"", []),
    ("version", ["-v"], [], 0, VERSION, b"", []),
    ("version-long", ["--version"], [], 0, VERSION, b"", []),
    ("version-over-help", ["-h", "-v"], [], 0, VERSION, b"", []),
    ("version-over-command", ["ls", "-s", "{S}", "--version"], [], 0,
     VERSION, b"", []),
    ("help-over-command", ["clear", "-s", "{S}", "-h"], [], 0, USAGE, b"",
     []),
    ("unknown-command", ["foo", "-s", "{S}"], [], 1, USAGE, b"", []),
    ("unknown-option", ["-x", "ls", "-s", "{S}"], [], 1, USAGE, b"", []),
    ("socket-only", ["-s", "{S}"], [], 1, USAGE, b"", []),
    ("bare-rm", ["rm", "-s", "{S}"], [], 0, USAGE, b"", []),
    ("bare-rm-presock", ["-s", "{S}", "rm"], [], 0, USAGE, b"", []),
    ("s-no-value", ["ls", "-s"], [], 1, b"", S_NO_VALUE, []),
    ("s-no-value-bare", ["-s"], [], 1, b"", S_NO_VALUE, []),
    ("s-no-value-later", ["-s", "{S}", "clear", "-s"], [], 1, b"",
     S_NO_VALUE, []),
    ("s-followed-by-option", ["-s", "-x", "ls"], [], 1, b"", S_NO_VALUE,
     []),
    ("rm-s-no-value", ["rm", "4", "-s"], [], 1, b"", S_NO_VALUE, []),

    # --- option placement and requests ---------------------------------
    ("ls", *ls(FULL, FULL_ROW)),
    ("ls-presock", ["-s", "{S}", "ls"], [reply(FULL)], 0, FULL_ROW, b"",
     [b"list\n"]),
    ("ls-extra-args", ["ls", "extra", "-s", "{S}", "more"], [reply(FULL)],
     0, FULL_ROW, b"", [b"list\n"]),
    ("ls-double-dash", ["ls", "--", "-s", "{S}"], [reply(FULL)], 0,
     FULL_ROW, b"", [b"list\n"]),
    ("ls-repeated-s", ["-s", "/nonexistent/x.sock", "ls", "-s", "{S}"],
     [reply(FULL)], 0, FULL_ROW, b"", [b"list\n"]),
    ("rm-id", ["rm", "4", "-s", "{S}"], [], 0, b"", b"", [b"del:4\n"]),
    ("rm-presock-operand", ["rm", "-s", "{S}", "4"], [], 0, b"", b"",
     [b"del:4\n"]),
    ("rm-sock-first", ["-s", "{S}", "rm", "7", "extra"], [], 0, b"", b"",
     [b"del:7\n"]),
    ("clear", ["clear", "-s", "{S}"], [reply(THREE)], 0, b"", b"",
     [b"list\n", b"del:3\n", b"del:1\n", b"del:2\n"]),
    ("clear-presock", ["-s", "{S}", "clear"], [reply(THREE)], 0, b"", b"",
     [b"list\n", b"del:3\n", b"del:1\n", b"del:2\n"]),
    ("clear-empty", ["clear", "-s", "{S}"], [reply(b"[]")], 0, b"", b"",
     [b"list\n"]),
    ("clear-defaults", ["clear", "-s", "{S}"],
     [reply(b'[{},1,"s",{"id":5}]')], 0, b"", b"",
     [b"list\n", b"del:0\n", b"del:0\n", b"del:0\n", b"del:5\n"]),
    ("clear-float-ids", ["clear", "-s", "{S}"],
     [reply(b'[{"id":1.9},{"id":-2.5},{"id":1e19}]')], 0, b"", b"",
     [b"list\n", b"del:1\n", b"del:-2\n", b"del:-9223372036854775808\n"]),
    ("clear-multi-root", ["clear", "-s", "{S}"],
     [reply(b'{"id":7} [] {"id":8}')], 0, b"", b"",
     [b"list\n", b"del:7\n", b"del:0\n", b"del:8\n"]),
    ("clear-rejected", *rejected(b'{"id":1}', "clear")),
    ("clear-rejected-garbage", *rejected(b"[] x", "clear")),

    # --- rm operand classification -------------------------------------
    ("rm-hex", *rm("0x10", b"del:16\n")),
    ("rm-hex-upper", *rm("0X1F", b"del:31\n")),
    ("rm-octal", *rm("010", b"del:8\n")),
    ("rm-octal-bad", *rm("08", b"dela:08\n")),
    ("rm-binary", *rm("0b101", b"del:5\n")),
    ("rm-binary-bare", *rm("0b", b"dela:0b\n")),
    ("rm-hex-bare", *rm("0x", b"dela:0x\n")),
    ("rm-partial", *rm("4abc", b"del:4\n")),
    ("rm-leading-digit-app", *rm("1Password", b"del:1\n")),
    ("rm-spaces", *rm("  7  ", b"del:7\n")),
    ("rm-tab", *rm("\t5", b"del:5\n")),
    ("rm-vt", *rm("\x0b6", b"del:6\n")),
    ("rm-nbsp", *rm("\xa06", "dela:\xa06\n".encode())),
    ("rm-plus", *rm("+5", b"del:5\n")),
    ("rm-negative", *rm("-5", b"del:-5\n")),
    ("rm-negative-hex", *rm("-0x10", b"del:-16\n")),
    ("rm-sign-space", *rm("- 5", b"dela:- 5\n")),
    ("rm-overflow", *rm("99999999999999999999999",
                        b"del:9223372036854775807\n")),
    ("rm-neg-overflow", *rm("-99999999999999999999999",
                            b"del:-9223372036854775808\n")),
    ("rm-hex-min", *rm("-0x8000000000000000",
                       b"del:-9223372036854775808\n")),
    ("rm-zero", *rm("0", b"dela:0\n")),
    ("rm-double-zero", *rm("00", b"dela:00\n")),
    ("rm-negative-zero", *rm("-0", b"dela:-0\n")),
    ("rm-empty", *rm("", b"dela:\n")),
    ("rm-app", *rm("firefox", b"dela:firefox\n")),
    ("rm-app-space", *rm("my app", b"dela:my app\n")),
    ("rm-app-colon", *rm("Foo: Bar", b"dela:Foo: Bar\n")),
    ("rm-app-unicode", *rm("通知app", "dela:通知app\n".encode())),
    ("rm-app-newline", *rm("a\nb", b"dela:a\nb\n")),
    ("rm-dash", *rm("-", b"dela:-\n")),

    # --- listing -------------------------------------------------------
    ("ls-empty", *ls(b"[]", b"")),
    ("ls-order", *ls(THREE, b"".join(
        row(n, b"myapp", b"Normal", b"hello", b"world")
        for n in (b"3", b"1", b"2")))),
    ("ls-urgencies", *ls(
        b"[" + b",".join(note(1, b"s", b"b", b"a", u)
                         for u in (0, 1, 2, 3, -1, 100)) + b"]",
        b"".join(row(b"1", b"a", u, b"s", b"b") for u in
                 (b"Low", b"Normal", b"Critical", b"Unknown", b"Unknown",
                  b"Unknown")))),
    ("ls-urgency-kinds", *ls(
        b"[" + b",".join(note(1, b"s", b"b", b"a", raw_urgency=u) for u in
                         (b"null", b"true", b"false", b'"2"', b"[2]",
                          b"1.9", b"2.5", b"1e0")) + b"]",
        b"".join(row(b"1", b"a", u, b"s", b"b") for u in
                 (b"Low", b"Normal", b"Low", b"Low", b"Low", b"Normal",
                  b"Critical", b"Normal")))),
    ("ls-urgency-missing", *ls(b"[" + note(1, b"s", b"b", b"a") + b"]",
                               row(b"1", b"a", b"Low", b"s", b"b"))),
    ("ls-missing-fields", *ls(b"[{}]", NULL_ROW)),
    ("ls-wrong-types", *ls(
        b'[{"id":"7","summary":1,"body":null,"application":[],'
        b'"urgency":{}}]',
        row(b"0", b"(null)", b"Low", b"(null)", b"(null)"))),
    ("ls-non-objects", *ls(b'[1,"s",null,true,[],[{}]]', NULL_ROW * 6)),
    ("ls-id-kinds", *ls(
        b"[" + b",".join(note(summary=b"s", raw_id=i) for i in
                         (b"true", b"false", b"-3", b"1.5", b"-1.9",
                          b"9223372036854775808", b"-9223372036854775809",
                          b"1e19", b"-1e19", b"9.223372036854776e18",
                          b"-9.223372036854776e18", b"1e999999", b"-0.0",
                          b"1e3")) + b"]",
        b"".join(row(i, b"(null)", b"Low", b"s", b"(null)") for i in
                 (b"1", b"0", b"-3", b"1", b"-1", b"9223372036854775807",
                  b"-9223372036854775808", b"-9223372036854775808",
                  b"-9223372036854775808", b"-9223372036854775808",
                  b"-9223372036854775808", b"-9223372036854775808", b"0",
                  b"1000")))),
    ("ls-duplicate-keys", *ls(
        b'[{"id":1,"id":2,"summary":"x","summary":"y"}]',
        row(b"2", b"(null)", b"Low", b"y", b"(null)"))),
    ("ls-extra-keys", *ls(
        b'[{"id":1,"summary":"s","body":"b","application":"a","urgency":2,'
        b'"actions":["x"],"hints":{"k":"v"},"ID":9}]',
        row(b"1", b"a", b"Critical", b"s", b"b"))),
    ("ls-unicode", *ls(
        '[{"id":1,"summary":"héllo 😀","body":"世界","application":"äpp"}]'
        .encode(),
        row(b"1", "äpp".encode(), b"Low", "héllo 😀".encode(),
            "世界".encode()))),
    ("ls-escapes", *ls(
        b'[{"summary":"a\\"b\\\\c\\/d\\be\\ff\\ng\\rh\\ti",'
        b'"body":"\\u0041\\ud83d\\ude00"}]',
        row(b"0", b"(null)", b"Low", b'a"b\\c/d\x08e\x0cf\ng\rh\ti',
            "A😀".encode()))),
    ("ls-loose-escapes", *ls(
        b'[{"summary":"a\\x41\\q\\123\\778\\400","body":"\\U0041"}]',
        row(b"0", b"(null)", b"Low", "ax41qS?8Ā".encode(),
            b"U0041"))),
    ("ls-nul-escape", *ls(
        b'[{"summary":"ab\\u0000cd","body":"x\\0y","application":"a\\000"}]',
        row(b"0", b"a", b"Low", b"ab", b"x"))),
    ("ls-raw-controls", *ls(b'[{"summary":"a\x01b\x7fc\x1fd"}]',
                            row(b"0", b"(null)", b"Low", b"a\x01b\x7fc\x1fd",
                                b"(null)"))),
    ("ls-single-quotes", *ls(b"[{'summary':'it\\s'}]",
                             row(b"0", b"(null)", b"Low", b"it\\s",
                                 b"(null)"))),
    ("ls-whitespace-bom", *ls(b"\xef\xbb\xbf \n\t[ ] \r\n", b"")),
    ("ls-pretty", *ls(b'[\n  {\n    "id" : 7 ,\n    "urgency" : 2\n  }\n]',
                      row(b"7", b"(null)", b"Critical", b"(null)",
                          b"(null)"))),
    ("ls-comments", *ls(b'/*a*/[/*b*/{"id"/*c*/:/*d*/3}/*e*/]/**/',
                        row(b"3", b"(null)", b"Low", b"(null)", b"(null)"))),
    ("ls-slash-comment", *ls(b"/x/[1]", NULL_ROW)),
    ("ls-multi-root", *ls(b'[] [1] {"id":6}',
                          NULL_ROW * 2
                          + row(b"6", b"(null)", b"Low", b"(null)",
                                b"(null)"))),
    ("ls-scalar-first", *ls(b"3 [1,2]", NULL_ROW * 2)),
    ("ls-assignment", *ls(b"var notes = [{}];", NULL_ROW)),
    ("ls-deep", *ls(b"[" * 600 + b"]" * 600, NULL_ROW)),
    ("ls-nul-truncates", *ls(b"[]\x00garbage(((", b"")),
    ("ls-nul-truncates-row", *ls(FULL + b"\x00" + b"[{}]", FULL_ROW)),
    ("ls-nul-before-bad-utf8", *ls(b"[1]\x00\xff", NULL_ROW)),
    ("ls-nul-in-string", *rejected(b'[{"summary":"a\x00b"}]')),

    # --- rejected replies ----------------------------------------------
    ("bad-empty", *rejected(b"")),
    ("bad-whitespace", *rejected(b"  \n ")),
    ("bad-comment-only", *rejected(b"/**/")),
    ("bad-text", *rejected(b"not json")),
    ("bad-trailing-garbage", *rejected(b"[] garbage")),
    ("bad-trailing-scalar", *rejected(b"[1] 2")),
    ("bad-two-scalars", *rejected(b"3 4")),
    ("bad-comma-roots", *rejected(b"[1],[2]")),
    ("bad-object-root", *rejected(b'{"a":1}')),
    ("bad-string-root", *rejected(b'"hi"')),
    ("bad-number-root", *rejected(b"42")),
    ("bad-null-root", *rejected(b"null")),
    ("bad-true-root", *rejected(b"true")),
    ("bad-utf8", *rejected(b"[\xff]")),
    ("bad-utf8-string", *rejected(b'[{"summary":"\xc3"}]')),
    ("bad-leading-zero", *rejected(b"[01]")),
    ("bad-plus", *rejected(b"[+1]")),
    ("bad-dot", *rejected(b"[.5]")),
    ("bad-dot-end", *rejected(b"[5.]")),
    ("bad-exponent", *rejected(b"[1e]")),
    ("bad-trailing-comma", *rejected(b"[1,]")),
    ("bad-object-trailing-comma", *rejected(b'[{"a":1,}]')),
    ("bad-missing-comma", *rejected(b"[1 2]")),
    ("bad-unclosed", *rejected(b'[{"id":1')),
    ("bad-nan", *rejected(b"[NaN]")),
    ("bad-true-case", *rejected(b"[True]")),
    ("bad-bareword", *rejected(b"[abc]")),
    ("bad-line-comment", *rejected(b"//c\n[]")),
    ("bad-unterminated-comment", *rejected(b"[1]/*")),
    ("bad-slash", *rejected(b"[1] / [2]")),
    ("bad-lone-surrogate", *rejected(b'[{"summary":"\\ud800"}]')),
    ("bad-low-surrogate", *rejected(b'["\\udc00"]')),
    ("bad-unicode-escape", *rejected(b'["\\uzzzz"]')),
    ("bad-raw-tab", *rejected(b'[{"summary":"a\tb"}]')),
    ("bad-raw-newline", *rejected(b'[{"summary":"a\nb"}]')),
    ("bad-vt-between", *rejected(b"[]\x0b[]")),
    ("bad-too-deep", *rejected(b"[" * 1025 + b"]" * 1025)),
    ("bad-assignment", *rejected(b"var x = [1]")),
    ("bad-member-name", *rejected(b"[{id:1}]")),

    # --- reply delivery ------------------------------------------------
    ("one-byte-chunks", *ls(FULL, FULL_ROW, chunk=1)),
    ("slow-chunks", *ls(FULL, FULL_ROW, chunk=3, delay=0.005)),
    ("many-in-8k-writes", *ls(MANY, MANY_OUT, chunk=8192)),
    ("large-reply", *ls(
        b'[{"id":1,"summary":"' + BIG
        + b'","body":"","application":"","urgency":0}]',
        row(b"1", b"", b"Low", BIG, b""), chunk=8192)),
    ("clear-chunked", ["clear", "-s", "{S}"], [reply(THREE, chunk=5)], 0,
     b"", b"", [b"list\n", b"del:3\n", b"del:1\n", b"del:2\n"]),
    ("no-reply", ["ls", "-s", "{S}"], [NOREPLY], 1, b"", BAD_REPLY,
     [b"list\n"]),
    ("early-close", ["ls", "-s", "{S}"], [EARLY], 1, b"", BAD_REPLY,
     [b""]),
    ("clear-no-reply", ["clear", "-s", "{S}"], [NOREPLY], 1, b"",
     BAD_REPLY, [b"list\n"]),

    # --- unreachable daemon --------------------------------------------
    ("missing-ls", ["ls", "-s", "{S}"], None, 1, b"",
     no_conn_error(b"No such file or directory"), []),
    ("missing-rm", ["rm", "4", "-s", "{S}"], None, 1, b"",
     no_conn_error(b"No such file or directory"), []),
    ("missing-rm-app", ["rm", "app", "-s", "{S}"], None, 1, b"",
     no_conn_error(b"No such file or directory"), []),
    ("missing-clear", ["-s", "{S}", "clear"], None, 1, b"",
     no_conn_error(b"No such file or directory"), []),
    ("missing-unknown-command", ["foo", "-s", "{S}"], None, 1, USAGE, b"",
     []),
    ("refused", ["ls", "-s", "{S}"], "refused", 1, b"",
     no_conn_error(b"Connection refused"), []),
    ("regular-file", ["ls", "-s", "{S}"], "file", 1, b"",
     no_conn_error(b"Connection refused"), []),
]


# ---------------------------------------------------------------------------
# Broken pipes
#
# (name, argv, behaviors, stream, rc, other, requests)
#   stream: "stdout" or "stderr", connected to a pipe whose read end is closed
#     before lago starts; "stdout-4k": stdout is read for 4096 bytes and then
#     closed while lago still has output to write; "stdout-full" and
#     "stderr-full": /dev/full.
#   rc: the expected exit status (a death by signal never matches).
#   other: expected bytes on the other stream.

LISTING = b"[" + b",".join(
    note(i, b"x" * 200, b"body", b"app", 1) for i in range(3000)) + b"]"
NO_SPACE = b"lago: cannot write to stdout: No space left on device\n"

PIPE_CASES = [
    # A reader that has gone away: lago stops writing, says nothing and
    # keeps the command's exit status ...
    ("pipe-version", ["-v"], [], "stdout", 0, b"", []),
    ("pipe-help", ["-h"], [], "stdout", 0, b"", []),
    ("pipe-no-args", [], [], "stdout", 0, b"", []),
    ("pipe-unknown-command", ["foo", "-s", "{S}"], [], "stdout", 1, b"", []),
    ("pipe-bare-rm", ["rm", "-s", "{S}"], [], "stdout", 0, b"", []),
    ("pipe-ls", ["ls", "-s", "{S}"], [reply(FULL)], "stdout", 0, b"",
     [b"list\n"]),
    ("pipe-ls-reader-stops", ["ls", "-s", "{S}"], [reply(LISTING)],
     "stdout-4k", 0, b"", [b"list\n"]),
    ("pipe-s-no-value", ["ls", "-s"], [], "stderr", 1, b"", []),
    ("pipe-missing-ls", ["ls", "-s", "{S}"], None, "stderr", 1, b"", []),
    ("pipe-missing-rm", ["rm", "4", "-s", "{S}"], None, "stderr", 1, b"",
     []),
    ("pipe-bad-reply", ["ls", "-s", "{S}"], [reply(b"not json")], "stderr",
     1, b"", [b"list\n"]),
    ("pipe-clear-bad-reply", ["clear", "-s", "{S}"], [reply(b"{}")],
     "stderr", 1, b"", [b"list\n"]),
    # ... and a stream lago does not write to changes nothing.
    ("pipe-ls-empty", ["ls", "-s", "{S}"], [reply(b"[]")], "stdout", 0, b"",
     [b"list\n"]),
    ("pipe-rm", ["rm", "4", "-s", "{S}"], [], "stdout", 0, b"",
     [b"del:4\n"]),
    ("pipe-clear", ["clear", "-s", "{S}"], [reply(THREE)], "stdout", 0, b"",
     [b"list\n", b"del:3\n", b"del:1\n", b"del:2\n"]),
    ("pipe-s-no-value-stdout", ["ls", "-s"], [], "stdout", 1, S_NO_VALUE,
     []),
    ("pipe-missing-stdout", ["ls", "-s", "{S}"], None, "stdout", 1,
     no_conn_error(b"No such file or directory"), []),
    ("pipe-version-stderr", ["-v"], [], "stderr", 0, VERSION, []),
    ("pipe-ls-stderr", ["ls", "-s", "{S}"], [reply(FULL)], "stderr", 0,
     FULL_ROW, [b"list\n"]),
    # Any other failure to write stdout is reported, with status 1 ...
    ("full-device-version", ["-v"], [], "stdout-full", 1, NO_SPACE, []),
    ("full-device-help", ["-h"], [], "stdout-full", 1, NO_SPACE, []),
    ("full-device-unknown-command", ["foo", "-s", "{S}"], [], "stdout-full",
     1, NO_SPACE, []),
    ("full-device-ls", ["ls", "-s", "{S}"], [reply(FULL)], "stdout-full", 1,
     NO_SPACE, [b"list\n"]),
    ("full-device-ls-empty", ["ls", "-s", "{S}"], [reply(b"[]")],
     "stdout-full", 0, b"", [b"list\n"]),
    # ... but a failure to write stderr has nowhere to go: the status of the
    # failure lago was reporting stays.
    ("full-device-s-no-value", ["ls", "-s"], [], "stderr-full", 1, b"", []),
    ("full-device-missing", ["ls", "-s", "{S}"], None, "stderr-full", 1,
     b"", []),
    ("full-device-bad-reply", ["ls", "-s", "{S}"], [reply(b"not json")],
     "stderr-full", 1, b"", [b"list\n"]),
]


def run_pipe_case(workdir, case):
    name, argv, behaviors, stream, rc, other, requests = case
    sock = os.path.join(workdir, name[:40] + ".sock")
    argv = [a.replace("{S}", sock) for a in argv]
    sub = sock.encode()
    daemon = FakeDaemon(sock, behaviors) if behaviors is not None else None
    target = stream.split("-")[0]
    files = {"stdout": subprocess.PIPE, "stderr": subprocess.PIPE}
    if stream.endswith("-full"):
        files[target] = os.open("/dev/full", os.O_WRONLY)
    elif stream in ("stdout", "stderr"):
        read_end, write_end = os.pipe()
        os.close(read_end)
        files[target] = write_end
    proc = subprocess.Popen(["lago"] + argv, executable=LAGO,
                            stdin=subprocess.DEVNULL, cwd=workdir, **files)
    for f in files.values():
        if isinstance(f, int) and f >= 0:
            os.close(f)
    other_stream = proc.stderr if target == "stdout" else proc.stdout
    if stream == "stdout-4k":
        proc.stdout.read(4096)
        proc.stdout.close()
    problems = []
    try:
        got_other = other_stream.read()
        proc.wait(10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        problems.append("timed out after 10 s")
        got_other = b""
    for f in (proc.stdout, proc.stderr):
        if f is not None and not f.closed:
            f.close()
    got_requests = []
    if daemon:
        daemon.close()
        got_requests = daemon.requests
    if proc.returncode < 0:
        problems.append(f"killed by {signal.Signals(-proc.returncode).name}, "
                        f"expected exit status {rc}")
    elif proc.returncode != rc:
        problems.append(f"exit status {proc.returncode}, expected {rc}")
    expected_other = other.replace(b"{S}", sub)
    if got_other != expected_other:
        problems.append(f"other stream {shorten(got_other)} != "
                        f"{shorten(expected_other)}")
    expected_requests = [r.replace(b"{S}", sub) for r in requests]
    if got_requests != expected_requests:
        problems.append(f"requests {got_requests!r} != {expected_requests!r}")
    return problems


# ---------------------------------------------------------------------------
# Runner

def run_case(workdir, case):
    name, argv, behaviors, rc, out, err, requests = case
    sock = os.path.join(workdir, name[:40] + ".sock")
    argv = [a.replace("{S}", sock) for a in argv]
    sub = sock.encode()
    daemon = None
    if behaviors == "refused":
        held = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        held.bind(sock)
        held.close()
    elif behaviors == "file":
        with open(sock, "w") as f:
            f.write("x")
    elif behaviors is not None:
        daemon = FakeDaemon(sock, behaviors)
    start = time.monotonic()
    try:
        proc = subprocess.run(["lago"] + argv, executable=LAGO,
                              capture_output=True, timeout=10, cwd=workdir)
    except subprocess.TimeoutExpired:
        if daemon:
            daemon.close()
        return [f"timed out after 10 s"]
    finally:
        elapsed = time.monotonic() - start
    got_requests = []
    if daemon:
        daemon.close()
        got_requests = daemon.requests
    problems = []
    if proc.returncode != rc:
        problems.append(f"exit status {proc.returncode}, expected {rc}")
    expected_out = out.replace(b"{S}", sub)
    if proc.stdout != expected_out:
        problems.append(f"stdout {shorten(proc.stdout)} != "
                        f"{shorten(expected_out)}")
    expected_err = err.replace(b"{S}", sub)
    if proc.stderr != expected_err:
        problems.append(f"stderr {shorten(proc.stderr)} != "
                        f"{shorten(expected_err)}")
    expected_requests = [r.replace(b"{S}", sub) for r in requests]
    if got_requests != expected_requests:
        problems.append(f"requests {got_requests!r} != {expected_requests!r}")
    if elapsed > 5.0:
        problems.append(f"took {elapsed:.1f} s after the daemon closed")
    return problems


def shorten(data, limit=160):
    text = repr(data)
    if len(text) > limit:
        return text[:limit] + f"... ({len(data)} bytes)"
    return text


def main():
    global LAGO
    if len(sys.argv) != 2:
        print("usage: lago_regress.py <lago>", file=sys.stderr)
        return 2
    LAGO = os.path.abspath(sys.argv[1])
    names = [case[0] for case in CASES + PIPE_CASES]
    assert len(names) == len(set(names)), "duplicate case names"
    failures = 0
    workdir = tempfile.mkdtemp(prefix="lago-regress-")
    runs = ([(run_case, case) for case in CASES]
            + [(run_pipe_case, case) for case in PIPE_CASES])
    try:
        for runner, case in runs:
            problems = runner(workdir, case)
            if problems:
                failures += 1
                print(f"FAIL {case[0]}")
                for problem in problems:
                    print(f"    {problem}")
            else:
                print(f"ok   {case[0]}")
    finally:
        for entry in os.listdir(workdir):
            try:
                os.unlink(os.path.join(workdir, entry))
            except OSError:
                pass
        os.rmdir(workdir)
    print(f"{len(runs) - failures}/{len(runs)} cases passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
