//! Shell quoting helpers for building commands that run on a remote host.

/// Quote a string so it can be passed as a single POSIX shell argument.
///
/// The value is wrapped in single quotes and embedded single quotes are
/// escaped with the classic `'\''` sequence, which is safe in every
/// POSIX-compatible shell.
#[must_use]
pub fn shell_escape(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::shell_escape;

    #[test]
    fn plain_path_is_single_quoted() {
        assert_eq!(shell_escape("/mnt/data"), "'/mnt/data'");
    }

    #[test]
    fn spaces_are_preserved() {
        assert_eq!(shell_escape("/mnt/my data"), "'/mnt/my data'");
    }

    #[test]
    fn single_quotes_are_escaped() {
        assert_eq!(shell_escape("/mnt/it's"), "'/mnt/it'\\''s'");
    }

    #[test]
    fn shell_metacharacters_cannot_escape_the_quoting() {
        let quoted = shell_escape("/tmp; rm -rf /");
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
        assert!(!quoted.contains("';'"));
    }
}
