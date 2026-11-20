//! The rofication socket protocol.
//!
//! One connection per request: `list\n` is answered with a JSON document
//! and the daemon closes; `del:<id>\n` and `dela:<app>\n` get no answer.

use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixStream};

use crate::json;
use crate::notification::Notification;

/// Longest path that fits `sun_path` with its terminating zero; longer
/// paths are cut to this length, as the original address object does.
const MAX_PATH_LEN: usize = 107;

const READ_CHUNK: usize = 8192;

pub enum ClientError {
    /// Connecting or sending a delete request failed; the text is the
    /// system's reason.
    Io(String),
    /// The `list` exchange failed or its reply is unusable.
    BadReply,
}

/// The system error text without Rust's ` (os error N)` suffix.
pub fn reason(err: &io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(idx) if text.ends_with(')') => text[..idx].to_string(),
        _ => text,
    }
}

pub struct Client {
    path: Vec<u8>,
}

impl Client {
    pub fn new(socket_path: &[u8]) -> Client {
        let len = socket_path.len().min(MAX_PATH_LEN);
        Client {
            path: socket_path[..len].to_vec(),
        }
    }

    fn connect(&self) -> Result<UnixStream, ClientError> {
        let result = if self.path.is_empty() {
            // An empty path is an all-zero sun_path: the abstract name made
            // of zero bytes.
            SocketAddr::from_abstract_name([0u8; MAX_PATH_LEN])
                .and_then(|addr| UnixStream::connect_addr(&addr))
        } else {
            UnixStream::connect(std::ffi::OsStr::from_bytes(&self.path))
        };
        result.map_err(|err| ClientError::Io(reason(&err)))
    }

    fn send(stream: &mut UnixStream, request: &[u8]) -> io::Result<()> {
        stream.write_all(request)
    }

    /// Sends `list` and reads the reply until the daemon closes.
    fn list_reply(&self) -> Result<Vec<u8>, ClientError> {
        let mut stream = self.connect()?;
        Self::send(&mut stream, b"list\n").map_err(|_| ClientError::BadReply)?;
        let mut reply = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => reply.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(ClientError::BadReply),
            }
        }
        drop(stream);
        Ok(reply)
    }

    pub fn get_notifications(&self) -> Result<Vec<Notification>, ClientError> {
        let mut reply = self.list_reply()?;
        // The reply is handled as a C string: it ends at the first NUL.
        if let Some(nul) = reply.iter().position(|&b| b == 0) {
            reply.truncate(nul);
        }
        let root = json::parse(&reply).ok_or(ClientError::BadReply)?;
        match root {
            json::Node::Array(elements) => {
                Ok(elements.iter().map(Notification::from_node).collect())
            }
            _ => Err(ClientError::BadReply),
        }
    }

    fn fire_and_forget(&self, request: &[u8]) -> Result<(), ClientError> {
        let mut stream = self.connect()?;
        Self::send(&mut stream, request).map_err(|err| {
            let mut text = String::from("Error sending data: ");
            text.push_str(&reason(&err));
            ClientError::Io(text)
        })?;
        drop(stream);
        Ok(())
    }

    pub fn delete_notification_by_id(&self, id: i64) -> Result<(), ClientError> {
        self.fire_and_forget(format!("del:{id}\n").as_bytes())
    }

    pub fn delete_notification_by_app(&self, app: &[u8]) -> Result<(), ClientError> {
        let mut request = b"dela:".to_vec();
        request.extend_from_slice(app);
        request.push(b'\n');
        self.fire_and_forget(&request)
    }
}

#[cfg(test)]
mod tests {
    use super::{reason, Client};
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    /// Serves one `list` request on a fresh socket, writing `pieces` with a
    /// pause between them, then closing; returns what the client read.
    fn list_with_reply_in(name: &str, pieces: Vec<Vec<u8>>) -> Vec<(i64, Vec<u8>)> {
        let dir = std::env::temp_dir().join(format!("lago-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut request = [0u8; 5];
            conn.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"list\n");
            for piece in pieces {
                conn.write_all(&piece).unwrap();
                conn.flush().unwrap();
                thread::sleep(Duration::from_millis(50));
            }
        });
        let client = Client::new(path.to_str().unwrap().as_bytes());
        let listed = match client.get_notifications() {
            Ok(list) => list
                .into_iter()
                .map(|n| (n.id, n.application.unwrap_or_default()))
                .collect(),
            Err(_) => panic!("the list reply was not read"),
        };
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        listed
    }

    #[test]
    fn reply_in_several_reads() {
        let reply = br#"[{"id": 1, "application": "Chat", "urgency": 1}, {"id": 2, "application": "Mail", "urgency": 2}]"#;
        let (first, second) = reply.split_at(30);
        let listed = list_with_reply_in("split", vec![first.to_vec(), second.to_vec()]);
        assert_eq!(listed, [(1, b"Chat".to_vec()), (2, b"Mail".to_vec())]);
    }

    #[test]
    fn reply_larger_than_any_single_read() {
        // About 1.5 MB, written in 64 KiB pieces.
        let body = "x".repeat(1_500_000);
        let reply =
            format!(r#"[{{"id": 7, "application": "Big", "body": "{body}"}}]"#).into_bytes();
        let pieces = reply.chunks(65536).map(<[u8]>::to_vec).collect();
        let listed = list_with_reply_in("large", pieces);
        assert_eq!(listed, [(7, b"Big".to_vec())]);
    }

    #[test]
    fn os_error_suffix_is_dropped() {
        let err = io::Error::from_raw_os_error(2);
        assert_eq!(reason(&err), "No such file or directory");
        let err = io::Error::from_raw_os_error(111);
        assert_eq!(reason(&err), "Connection refused");
    }
}
