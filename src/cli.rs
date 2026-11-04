//! Shared command-line helpers.

use anyhow::{bail, Context, Result};

/// Resolve the shared secret from a single `--psk` value.
///
/// The value is either the secret itself or `@PATH`, in which case the secret is
/// read from that file (recommended for long-running services: it keeps the
/// secret out of `ps` output and shell history).  A trailing newline in the file
/// is ignored.
pub fn read_psk(value: &str) -> Result<String> {
    if let Some(path) = value.strip_prefix('@') {
        if path.is_empty() {
            bail!("--psk @PATH is missing the path");
        }
        let secret = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the PSK file {path}"))?;
        let secret = secret.trim_end_matches(['\r', '\n']);
        if secret.is_empty() {
            bail!("the PSK file {path} is empty");
        }
        Ok(secret.to_owned())
    } else if value.is_empty() {
        bail!("--psk must not be empty")
    } else {
        Ok(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_secret_is_returned_as_is() {
        assert_eq!(read_psk("s3cret").expect("literal"), "s3cret");
    }

    #[test]
    fn reads_secret_from_a_file() {
        let path = std::env::temp_dir().join(format!("psk-test-{}", std::process::id()));
        std::fs::write(&path, "file-secret\r\n").expect("write");
        let secret = read_psk(&format!("@{}", path.display())).expect("file");
        let _ = std::fs::remove_file(&path);
        assert_eq!(secret, "file-secret");
    }

    #[test]
    fn rejects_empty_values() {
        assert!(read_psk("").is_err());
        assert!(read_psk("@").is_err());
    }
}
