//! One listed notification and its `ls` rendering.

use crate::json::Node;

/// A notification as read from a `list` reply element. Text fields are
/// `None` when the member is missing or is not a string; numeric fields are
/// 0 when missing or not a number. Elements that are not objects read as
/// all-missing.
pub struct Notification {
    pub id: i64,
    pub summary: Option<Vec<u8>>,
    pub body: Option<Vec<u8>>,
    pub application: Option<Vec<u8>>,
    pub urgency: i64,
}

impl Notification {
    pub fn from_node(node: &Node) -> Notification {
        match node {
            Node::Object(_) => Notification {
                id: node.int_member(b"id"),
                summary: node.string_member(b"summary").map(<[u8]>::to_vec),
                body: node.string_member(b"body").map(<[u8]>::to_vec),
                application: node.string_member(b"application").map(<[u8]>::to_vec),
                urgency: node.int_member(b"urgency"),
            },
            _ => Notification {
                id: 0,
                summary: None,
                body: None,
                application: None,
                urgency: 0,
            },
        }
    }

    /// Appends `[<id>] <application> (<urgency>) - <summary> \n\t<body>\n`;
    /// absent text prints as `(null)`.
    pub fn render(&self, out: &mut Vec<u8>) {
        out.push(b'[');
        out.extend_from_slice(self.id.to_string().as_bytes());
        out.extend_from_slice(b"] ");
        push_text(out, self.application.as_deref());
        out.extend_from_slice(b" (");
        out.extend_from_slice(urgency_text(self.urgency).as_bytes());
        out.extend_from_slice(b") - ");
        push_text(out, self.summary.as_deref());
        out.extend_from_slice(b" \n\t");
        push_text(out, self.body.as_deref());
        out.push(b'\n');
    }
}

fn push_text(out: &mut Vec<u8>, text: Option<&[u8]>) {
    out.extend_from_slice(text.unwrap_or(b"(null)"));
}

/// Freedesktop notification urgency levels.
fn urgency_text(urgency: i64) -> &'static str {
    match urgency {
        0 => "Low",
        1 => "Normal",
        2 => "Critical",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(reply: &[u8]) -> Vec<u8> {
        let root = crate::json::parse(reply).expect("accepted");
        let mut out = Vec::new();
        if let Node::Array(elements) = root {
            for element in &elements {
                Notification::from_node(element).render(&mut out);
            }
        }
        out
    }

    #[test]
    fn full_record() {
        let out = render(
            br#"[{"id":42,"summary":"hello","body":"world","application":"myapp","urgency":1}]"#,
        );
        assert_eq!(out, b"[42] myapp (Normal) - hello \n\tworld\n");
    }

    #[test]
    fn defaults() {
        assert_eq!(render(b"[{}]"), b"[0] (null) (Low) - (null) \n\t(null)\n");
        assert_eq!(render(b"[1]"), b"[0] (null) (Low) - (null) \n\t(null)\n");
        assert_eq!(
            render(br#"[{"id":true,"urgency":7,"summary":5}]"#),
            b"[1] (null) (Unknown) - (null) \n\t(null)\n"
        );
    }

    #[test]
    fn doubles_truncate_and_out_of_range_is_min() {
        assert_eq!(render(br#"[{"id":-1.9}]"#)[..4], *b"[-1]");
        let out = render(br#"[{"id":1e19}]"#);
        assert!(out.starts_with(b"[-9223372036854775808]"));
    }
}
