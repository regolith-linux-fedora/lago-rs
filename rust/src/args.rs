//! Command-line handling.
//!
//! Two passes over argv, as in the original client: an option table (every
//! token starting with `-` is a key; the next token not starting with `-`
//! is its value, a key without one maps to "no value"), then a positional
//! scan that finds the command and the `rm` operand while skipping the
//! value of `-s`.

/// Option table built from argv. Later occurrences of a key replace earlier
/// ones; `None` is a key present without a value.
pub struct Options {
    entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl Options {
    fn set(&mut self, key: &[u8], value: Option<Vec<u8>>) {
        if let Some(entry) = self.entries.iter_mut().find(|(k, _)| k == key) {
            entry.1 = value;
        } else {
            self.entries.push((key.to_vec(), value));
        }
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    /// The value stored for `key`; `None` both when absent and when present
    /// without a value.
    pub fn lookup(&self, key: &[u8]) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref())
    }
}

pub fn is_key(token: &[u8]) -> bool {
    token.first() == Some(&b'-')
}

/// Builds the option table. The first token (the program name) is stored
/// under the reserved key `cmd` and never treated as an option.
pub fn parse_args(raw_args: &[Vec<u8>]) -> Options {
    let mut options = Options {
        entries: Vec::new(),
    };
    let mut last_key: Option<&[u8]> = None;
    for token in raw_args {
        if !options.contains(b"cmd") {
            options.set(b"cmd", Some(token.clone()));
        } else if is_key(token) {
            if let Some(key) = last_key {
                options.set(key, None);
            }
            last_key = Some(token);
        } else if let Some(key) = last_key.take() {
            options.set(key, Some(token.clone()));
        }
    }
    if let Some(key) = last_key {
        options.set(key, None);
    }
    options
}

pub struct Positional {
    pub command: Option<Vec<u8>>,
    pub rm_operand: Option<Vec<u8>>,
}

/// Finds the command (the first token that is neither `-s` with its value
/// nor a help/version flag) and, for `rm`, the operand after it.
pub fn positional(raw_args: &[Vec<u8>]) -> Positional {
    let mut command: Option<Vec<u8>> = None;
    let mut rm_operand: Option<Vec<u8>> = None;
    let mut skip_value = false;
    for i in 1..raw_args.len() {
        let token = raw_args[i].as_slice();
        if skip_value {
            skip_value = false;
            continue;
        }
        if token == b"-s" {
            if i + 1 < raw_args.len() && !is_key(&raw_args[i + 1]) {
                skip_value = true;
            }
            continue;
        }
        if matches!(token, b"-h" | b"--help" | b"-v" | b"--version") {
            continue;
        }
        match command.as_deref() {
            None => command = Some(token.to_vec()),
            Some(b"rm") if rm_operand.is_none() => rm_operand = Some(token.to_vec()),
            _ => {}
        }
    }
    Positional {
        command,
        rm_operand,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<Vec<u8>> {
        tokens.iter().map(|t| t.as_bytes().to_vec()).collect()
    }

    #[test]
    fn trailing_key_has_no_value() {
        let o = parse_args(&argv(&["lago", "ls", "-s"]));
        assert!(o.contains(b"-s"));
        assert_eq!(o.lookup(b"-s"), None);
    }

    #[test]
    fn later_key_without_value_replaces_value() {
        let o = parse_args(&argv(&["lago", "-s", "P", "-s"]));
        assert!(o.contains(b"-s"));
        assert_eq!(o.lookup(b"-s"), None);
    }

    #[test]
    fn empty_value_is_a_value() {
        let o = parse_args(&argv(&["lago", "ls", "-s", ""]));
        assert_eq!(o.lookup(b"-s"), Some(&b""[..]));
    }

    #[test]
    fn options_anywhere() {
        let p = positional(&argv(&["lago", "-s", "P", "rm", "-s", "Q", "4", "x"]));
        assert_eq!(p.command.as_deref(), Some(&b"rm"[..]));
        assert_eq!(p.rm_operand.as_deref(), Some(&b"4"[..]));
    }

    #[test]
    fn unknown_option_is_the_command() {
        let p = positional(&argv(&["lago", "-x", "ls"]));
        assert_eq!(p.command.as_deref(), Some(&b"-x"[..]));
    }
}
