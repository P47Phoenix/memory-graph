//! `memory-graph auth new-token` and `auth new-secret` (ADR 0012 D2/D3):
//! create a credential, show it once, and record only its SHA-256.
//!
//! The token is printed once (stdout, or `--token-out`, a new file with
//! mode 0600 on Unix). The token file stanza (id, role, digest) goes to
//! stderr, or is appended to `--append <file>` (created with mode 0600). No
//! message, stanza or error ever contains the token.
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use zeroize::Zeroizing;

/// What `new-token` made.
pub struct NewToken {
    /// The token itself (zeroized on drop).
    pub token: Zeroizing<String>,
    /// The `[[token]]` stanza for the server's token file.
    pub stanza: String,
}

/// A token id: free text for logs, restricted so the stanza stays one
/// plain TOML string.
fn check_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@:/+".contains(&b))
    {
        bail!(
            "--id `{id}`: use 1 to 128 letters, digits and . _ - @ : / + (it names the token in \
             the server's logs)"
        );
    }
    Ok(())
}

/// Open `path` for writing with mode 0600 on Unix: a new file only
/// (`append` false), or appending to an existing one or a new one.
pub fn open_private(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    if append {
        o.append(true).create(true);
    } else {
        o.write(true).create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Create a token with this id and role (`read` or `write`) and an
/// optional expiry (RFC 3339). With `append`, the stanza is appended to
/// that token file after checking that it stays valid (no duplicate id).
pub fn new_token(
    id: &str,
    role: &str,
    expires: Option<&str>,
    append: Option<&Path>,
) -> Result<NewToken> {
    check_id(id)?;
    if graph_server::Role::parse(role).is_none() {
        bail!("--role `{role}`: expected read or write");
    }
    if let Some(e) = expires {
        if graph_server::auth::parse_rfc3339(e).is_none() {
            bail!(
                "--expires `{e}`: not an RFC 3339 date-time with an offset (2027-01-01T00:00:00Z)"
            );
        }
    }
    let token = graph_server::auth::new_credential(graph_proto::auth::TOKEN_PREFIX);
    let mut stanza = format!(
        "[[token]]\nid = \"{id}\"\nrole = \"{role}\"\nsha256 = \"{}\"\n",
        graph_proto::auth::digest_hex(&token)
    );
    if let Some(e) = expires {
        stanza.push_str(&format!("expires = \"{e}\"\n"));
    }
    if let Some(path) = append {
        let existing = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(e).with_context(|| format!("--append `{}`", path.display()));
            }
        };
        let sep = if existing.is_empty() || existing.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let combined = format!("{existing}{sep}{stanza}");
        if let Err(e) = graph_server::auth::parse_token_file(&combined) {
            bail!(
                "--append `{}`: the file would not be a valid token file ({e}); nothing was \
                 written",
                path.display()
            );
        }
        let mut f =
            open_private(path, true).with_context(|| format!("--append `{}`", path.display()))?;
        let text = if existing.is_empty() {
            stanza.clone()
        } else {
            format!("{sep}{stanza}")
        };
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .with_context(|| format!("--append `{}`", path.display()))?;
    }
    Ok(NewToken { token, stanza })
}

/// Write `secret` (one line) to a new file with mode 0600.
pub fn write_secret_file(path: &Path, secret: &str) -> Result<()> {
    let mut f = open_private(path, false).with_context(|| {
        format!(
            "`{}` (it must not exist: a credential file is never overwritten)",
            path.display()
        )
    })?;
    f.write_all(format!("{secret}\n").as_bytes())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("writing `{}`", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_builds_a_valid_file_and_never_writes_the_token() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tokens.toml");
        let a = new_token("ci", "write", None, Some(&p)).unwrap();
        let b = new_token("bot", "read", Some("2030-01-01T00:00:00Z"), Some(&p)).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains(&a.token[4..]) && !text.contains(&b.token[4..]));
        assert!(!a.stanza.contains(&a.token[4..]));
        let set = graph_server::auth::parse_token_file(&text).unwrap();
        assert_eq!(set.entries.len(), 2);
        let e = new_token("ci", "read", None, Some(&p))
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("duplicate"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text, "unchanged");
        assert!(new_token("a b", "read", None, None).is_err());
        assert!(new_token("a", "admin", None, None).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
            let out = d.path().join("t");
            write_secret_file(&out, &a.token).unwrap();
            let mode = std::fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        let out = d.path().join("t2");
        write_secret_file(&out, &a.token).unwrap();
        assert!(
            write_secret_file(&out, &a.token).is_err(),
            "never overwrites"
        );
    }
}
