//! [`AuthFixture`]: a token file and a cluster secret for tests of ADR
//! 0012 (`--auth-tokens`, `--cluster-secret-file`).
use crate::auth::{new_credential, AuthConfig};
use graph_proto::auth::{digest_hex, SECRET_PREFIX, TOKEN_PREFIX};
use std::path::PathBuf;

/// Fresh tokens of each kind, a token file listing them and a secret file.
pub struct AuthFixture {
    pub dir: tempfile::TempDir,
    /// The token file (`write`, `write2`, `read`, and `expired`, which
    /// expired in 2000).
    pub tokens: PathBuf,
    pub secret_file: PathBuf,
    pub write: String,
    pub write2: String,
    pub read: String,
    pub expired: String,
    pub secret: String,
}

/// One `[[token]]` stanza for `token`.
pub fn stanza(id: &str, role: &str, token: &str, expires: Option<&str>) -> String {
    let mut s = format!(
        "[[token]]\nid = \"{id}\"\nrole = \"{role}\"\nsha256 = \"{}\"\n",
        digest_hex(token)
    );
    if let Some(e) = expires {
        s.push_str(&format!("expires = \"{e}\"\n"));
    }
    s
}

impl AuthFixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("auth fixture dir");
        let t = || new_credential(TOKEN_PREFIX).to_string();
        let (write, write2, read, expired) = (t(), t(), t(), t());
        let secret = new_credential(SECRET_PREFIX).to_string();
        let tokens = dir.path().join("tokens.toml");
        let secret_file = dir.path().join("cluster.secret");
        let f = Self {
            dir,
            tokens,
            secret_file,
            write,
            write2,
            read,
            expired,
            secret,
        };
        f.write_tokens(&f.default_tokens());
        std::fs::write(&f.secret_file, format!("{}\n", f.secret)).expect("secret file");
        f
    }

    /// The token file's default content.
    pub fn default_tokens(&self) -> String {
        [
            stanza("w", "write", &self.write, None),
            stanza("w2", "write", &self.write2, None),
            stanza("r", "read", &self.read, None),
            stanza("old", "read", &self.expired, Some("2000-01-01T00:00:00Z")),
        ]
        .join("\n")
    }

    /// Replace the token file's content.
    pub fn write_tokens(&self, text: &str) {
        std::fs::write(&self.tokens, text).expect("token file");
    }

    /// `--auth-tokens` and `--cluster-secret-file` for these files.
    pub fn config(&self) -> AuthConfig {
        AuthConfig {
            tokens: Some(self.tokens.clone()),
            cluster_secret: Some(self.secret_file.clone()),
        }
    }

    /// Client credentials for `token`.
    pub fn creds(&self, token: &str) -> graph_client::Credentials {
        graph_client::Credentials::from_token("test", token).expect("a client token")
    }
}

impl Default for AuthFixture {
    fn default() -> Self {
        Self::new()
    }
}
