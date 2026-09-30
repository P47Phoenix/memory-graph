//! S3 credentials (ADR 0006 E6): the environment (`AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) first, then
//! `--backup-credentials-file` (the AWS INI format) with `--backup-profile`
//! (default `default`). Never a flag or a TOML key; no IMDS. The secret and
//! the session token are redacted in every `Debug` and `Display`.
use std::path::Path;

/// A string that never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// The value, for signing only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where the credentials came from (for the start-up log line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Env,
    File { path: String, profile: String },
}

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: Secret,
    pub session_token: Option<Secret>,
    pub source: Source,
}

impl Credentials {
    /// Fixed credentials (tests and `FakeS3`).
    pub fn new(access_key_id: &str, secret: &str) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: Secret::new(secret),
            session_token: None,
            source: Source::Env,
        }
    }
}

/// The key id is shown by its first four characters only.
fn short_id(id: &str) -> String {
    let head: String = id.chars().take(4).collect();
    format!("{head}...")
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &short_id(&self.access_key_id))
            .field("secret_access_key", &self.secret_access_key)
            .field("session_token", &self.session_token)
            .field("source", &self.source)
            .finish()
    }
}

/// The environment variables read (in this order of precedence over the
/// file).
pub const ENV_KEY: &str = "AWS_ACCESS_KEY_ID";
pub const ENV_SECRET: &str = "AWS_SECRET_ACCESS_KEY";
pub const ENV_TOKEN: &str = "AWS_SESSION_TOKEN";

/// Resolve credentials: `env` (a lookup, `std::env::var` in production)
/// first, then `file` / `profile`.
pub fn resolve(
    env: &dyn Fn(&str) -> Option<String>,
    file: Option<&Path>,
    profile: Option<&str>,
) -> Result<Credentials, String> {
    let get = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    match (get(ENV_KEY), get(ENV_SECRET)) {
        (Some(id), Some(secret)) => {
            return Ok(Credentials {
                access_key_id: id.trim().into(),
                secret_access_key: Secret::new(secret.trim()),
                session_token: get(ENV_TOKEN).map(|t| Secret::new(t.trim())),
                source: Source::Env,
            })
        }
        (Some(_), None) => return Err(format!("{ENV_KEY} is set but {ENV_SECRET} is not")),
        (None, Some(_)) => return Err(format!("{ENV_SECRET} is set but {ENV_KEY} is not")),
        (None, None) => {}
    }
    let Some(file) = file else {
        return Err(format!(
            "no S3 credentials: set {ENV_KEY} and {ENV_SECRET} (and {ENV_TOKEN} if temporary), \
             or pass --backup-credentials-file (with --backup-profile); credentials are never \
             taken from flags or the config file, and instance roles (IMDS) are not supported"
        ));
    };
    let profile = profile.unwrap_or("default");
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("--backup-credentials-file `{}`: {e}", file.display()))?;
    let section = parse_ini(&text, profile).ok_or_else(|| {
        format!(
            "--backup-credentials-file `{}` has no [{profile}] profile",
            file.display()
        )
    })?;
    let field = |k: &str| {
        section
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let (Some(id), Some(secret)) = (field("aws_access_key_id"), field("aws_secret_access_key"))
    else {
        return Err(format!(
            "--backup-credentials-file `{}` [{profile}]: needs aws_access_key_id and \
             aws_secret_access_key",
            file.display()
        ));
    };
    Ok(Credentials {
        access_key_id: id,
        secret_access_key: Secret::new(secret),
        session_token: field("aws_session_token").map(Secret::new),
        source: Source::File {
            path: file.display().to_string(),
            profile: profile.into(),
        },
    })
}

/// The `key = value` pairs of `[profile]` (or `[profile <profile>]`, the
/// config-file spelling) in an AWS INI file; `None` if there is no such
/// section. `#` and `;` start comment lines.
fn parse_ini(text: &str, profile: &str) -> Option<Vec<(String, String)>> {
    let mut found = None;
    let mut current: Option<bool> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            let name = name.strip_prefix("profile ").map(str::trim).unwrap_or(name);
            let hit = name == profile;
            if hit && found.is_none() {
                found = Some(Vec::new());
            }
            current = Some(hit);
            continue;
        }
        if current == Some(true) {
            if let Some((k, v)) = line.split_once('=') {
                found
                    .as_mut()
                    .expect("in the section")
                    .push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn file(dir: &Path) -> std::path::PathBuf {
        let p = dir.join("credentials");
        std::fs::write(
            &p,
            "# comment\n[default]\naws_access_key_id = FILEKEY\naws_secret_access_key = \
             filesecret-DEFAULT\n\n[profile backup]\naws_access_key_id=BKEY1234\n\
             aws_secret_access_key=bsecret-XYZ\naws_session_token = btoken-XYZ\n",
        )
        .unwrap();
        p
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let d = tempfile::tempdir().unwrap();
        let f = file(d.path());
        let c = resolve(
            &env(&[(ENV_KEY, "ENVKEY"), (ENV_SECRET, "envsecret-SSS")]),
            Some(&f),
            None,
        )
        .unwrap();
        assert_eq!(
            (c.access_key_id.as_str(), c.source),
            ("ENVKEY", Source::Env)
        );
        let c = resolve(&env(&[]), Some(&f), None).unwrap();
        assert_eq!(c.access_key_id, "FILEKEY");
        assert_eq!(c.secret_access_key.expose(), "filesecret-DEFAULT");
        let c = resolve(&env(&[]), Some(&f), Some("backup")).unwrap();
        assert_eq!(c.access_key_id, "BKEY1234");
        assert_eq!(c.session_token.unwrap().expose(), "btoken-XYZ");
        assert!(resolve(&env(&[]), Some(&f), Some("nope"))
            .unwrap_err()
            .contains("[nope]"));
        assert!(resolve(&env(&[]), None, None)
            .unwrap_err()
            .contains("no S3 credentials"));
        assert!(resolve(&env(&[(ENV_KEY, "K")]), Some(&f), None)
            .unwrap_err()
            .contains(ENV_SECRET));
        // Empty variables do not count.
        let c = resolve(&env(&[(ENV_KEY, ""), (ENV_SECRET, " ")]), Some(&f), None).unwrap();
        assert_eq!(c.access_key_id, "FILEKEY");
    }

    #[test]
    fn secrets_never_print() {
        let d = tempfile::tempdir().unwrap();
        let f = file(d.path());
        let c = resolve(&env(&[]), Some(&f), Some("backup")).unwrap();
        let shown = format!("{c:?} {} {:?}", c.secret_access_key, c.session_token);
        for secret in ["bsecret-XYZ", "btoken-XYZ", "BKEY1234"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert!(shown.contains("<redacted>"));
        let c = resolve(
            &env(&[
                (ENV_KEY, "AKIAENV1234"),
                (ENV_SECRET, "envsecret-SSS"),
                (ENV_TOKEN, "envtoken-TTT"),
            ]),
            None,
            None,
        )
        .unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("envsecret-SSS") && !shown.contains("envtoken-TTT"));
        assert!(!shown.contains("AKIAENV1234") && shown.contains("AKIA..."));
    }

    proptest::proptest! {
        /// Whatever the secret, `Debug` never shows it.
        #[test]
        fn debug_redacts_any_secret(s in "[A-Za-z0-9/+]{12,40}") {
            let c = Credentials {
                access_key_id: "AKID".into(),
                secret_access_key: Secret::new(s.clone()),
                session_token: Some(Secret::new(s.clone())),
                source: Source::Env,
            };
            let shown = format!("{c:?}");
            proptest::prop_assert!(!shown.contains(&s));
        }
    }
}
