//! Client credentials (ADR 0012 D1/D2): a bearer token held in zeroized
//! memory, never printed, sent as a sensitive `authorization` header on
//! every call through [`AuthChannel`].
use graph_proto::auth::{self as wire, CredKind};
use graph_store::StoreError;
use std::path::Path;
use std::task::{Context, Poll};
use tonic::codegen::http;
use tonic::transport::Channel;
use zeroize::Zeroizing;

/// A client token (`mgt_...`). `Debug` never shows it.
#[derive(Clone)]
pub struct Credentials {
    token: Zeroizing<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

/// The message for a value that is not a client token; it never quotes
/// the value. `source` names where it came from (`--token-file <path>`, an
/// environment variable).
fn not_a_token(source: &str, value: &str) -> StoreError {
    let why = if wire::check_format(value) == Some(CredKind::Secret) {
        "it holds a cluster secret (`mgc_`), which is never a client credential"
    } else {
        "it is not a memory-graph token (`mgt_` followed by 43 characters)"
    };
    StoreError::Rejected(format!("{source}: {why}"))
}

impl Credentials {
    /// A token given as a value (`MEMORY_GRAPH_TOKEN`); `source` names it
    /// in errors.
    pub fn from_token(source: &str, token: &str) -> Result<Self, StoreError> {
        let t = token.trim();
        if wire::check_format(t) != Some(CredKind::Token) {
            return Err(not_a_token(source, t));
        }
        Ok(Self {
            token: Zeroizing::new(t.to_string()),
        })
    }

    /// A token read from a file (`--token-file`, `MEMORY_GRAPH_TOKEN_FILE`):
    /// read once, one trailing newline trimmed. On Unix a file readable by
    /// group or others is warned about on stderr.
    pub fn from_file(path: &Path) -> Result<Self, StoreError> {
        let bytes = Zeroizing::new(std::fs::read(path).map_err(|e| {
            StoreError::Rejected(format!("cannot read token file `{}`: {e}", path.display()))
        })?);
        warn_if_readable_by_others(path);
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            StoreError::Rejected(format!("token file `{}` is not UTF-8", path.display()))
        })?;
        let text = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        let source = format!("token file `{}`", path.display());
        if wire::check_format(text) != Some(CredKind::Token) {
            return Err(not_a_token(&source, text));
        }
        Ok(Self {
            token: Zeroizing::new(text.to_string()),
        })
    }

    /// The `authorization` header value, marked sensitive.
    pub(crate) fn header(&self) -> http::HeaderValue {
        let mut v = http::HeaderValue::from_str(&format!("Bearer {}", *self.token))
            .expect("a checked token is a valid header value");
        v.set_sensitive(true);
        v
    }
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::MetadataExt;
    if let Ok(m) = std::fs::metadata(path) {
        if m.mode() & 0o077 != 0 {
            eprintln!(
                "memory-graph: WARNING: token file `{}` is readable by group or others; restrict \
                 it (chmod 600)",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

/// A tonic channel that adds the `authorization` header (when the client
/// has credentials) to every request it carries.
#[derive(Clone, Debug)]
pub struct AuthChannel {
    inner: Channel,
    header: Option<http::HeaderValue>,
}

impl AuthChannel {
    pub fn new(inner: Channel, creds: Option<&Credentials>) -> Self {
        Self {
            inner,
            header: creds.map(Credentials::header),
        }
    }
}

impl tower_service::Service<http::Request<tonic::body::Body>> for AuthChannel {
    type Response = http::Response<tonic::body::Body>;
    type Error = tonic::transport::Error;
    type Future = tonic::transport::channel::ResponseFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tower_service::Service::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, mut req: http::Request<tonic::body::Body>) -> Self::Future {
        if let Some(h) = &self.header {
            req.headers_mut()
                .insert(http::header::AUTHORIZATION, h.clone());
        }
        tower_service::Service::call(&mut self.inner, req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn debug_and_errors_never_show_the_token() {
        let t = format!("mgt_{BODY}");
        let c = Credentials::from_token("MEMORY_GRAPH_TOKEN", &t).unwrap();
        assert!(!format!("{c:?}").contains(BODY));
        let h = c.header();
        assert!(h.is_sensitive());
        assert!(!format!("{h:?}").contains(BODY));
        for bad in [
            format!("mgc_{BODY}"),
            format!("mgt_{BODY}x"),
            "secret".into(),
        ] {
            let e = Credentials::from_token("MEMORY_GRAPH_TOKEN", &bad)
                .unwrap_err()
                .to_string();
            assert!(!e.contains(&bad) && !e.contains(BODY), "{e}");
            assert!(e.contains("MEMORY_GRAPH_TOKEN"), "{e}");
        }
    }

    #[test]
    fn a_token_file_loses_one_trailing_newline() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t");
        for text in [
            format!("mgt_{BODY}"),
            format!("mgt_{BODY}\n"),
            format!("mgt_{BODY}\r\n"),
        ] {
            std::fs::write(&p, &text).unwrap();
            Credentials::from_file(&p).unwrap();
        }
        std::fs::write(&p, format!("mgt_{BODY}\n\n")).unwrap();
        let e = Credentials::from_file(&p).unwrap_err().to_string();
        assert!(!e.contains(BODY), "{e}");
    }
}
