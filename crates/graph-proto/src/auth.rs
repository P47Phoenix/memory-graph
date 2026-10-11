//! Bearer credentials on the wire (ADR 0012 D1/D3): the header names, the
//! token formats, the pre-hash format check and the hash a server keeps.
//!
//! * A client token is `mgt_` followed by 43 characters of unpadded
//!   base64url (32 random bytes); the cluster secret has the same shape
//!   with `mgc_`. Both travel as `authorization: Bearer <value>`.
//! * A server stores only `sha256(value)` and compares fixed-length
//!   digests in constant time (in `graph-server`).
//! * [`parse_bearer`] refuses everything that is not exactly one of the two
//!   formats before anything is hashed, so a hostile header costs no more
//!   than a length check.
//!
//! Nothing here formats a credential into a message: every error names the
//! class of the problem only.
use sha2::{Digest, Sha256};
use tonic::metadata::{Ascii, MetadataValue};

/// The standard header (lower case, as gRPC metadata keys are).
pub const AUTHORIZATION: &str = "authorization";

/// On a forwarded call (ADR 0012 D3): the role the forwarding node granted
/// the original caller (`read` or `write`). Honoured only on a call that
/// authenticated with the cluster secret.
pub const FORWARDED_ROLE_HEADER: &str = "mg-forwarded-role";

/// On a forwarded call: the id (never the value) of the caller's token,
/// for the leader's logs.
pub const FORWARDED_TOKEN_ID_HEADER: &str = "mg-forwarded-token-id";

/// The prefix of a client token.
pub const TOKEN_PREFIX: &str = "mgt_";

/// The prefix of the cluster secret.
pub const SECRET_PREFIX: &str = "mgc_";

/// Characters after the prefix: 32 bytes in unpadded base64url.
pub const BODY_LEN: usize = 43;

/// The longest `authorization` value looked at; longer is refused unread.
pub const MAX_HEADER_LEN: usize = 128;

/// Which credential a well-formed value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredKind {
    /// `mgt_...`: a client token.
    Token,
    /// `mgc_...`: the cluster secret.
    Secret,
}

/// Why a header was refused before hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// Longer than [`MAX_HEADER_LEN`].
    TooLong,
    /// A byte outside printable ASCII.
    NotAscii,
    /// Not `Bearer <value>`.
    Scheme,
    /// The value is not `mgt_`/`mgc_` plus 43 base64url characters.
    Format,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Malformed::TooLong => "the credential is too long",
            Malformed::NotAscii => "the credential is not printable ASCII",
            Malformed::Scheme => "the authorization header is not `Bearer <token>`",
            Malformed::Format => "the credential is not in the memory-graph token format",
        })
    }
}

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Which credential `value` is, when it is exactly one of the two formats.
pub fn check_format(value: &str) -> Option<CredKind> {
    let kind = if let Some(rest) = value.strip_prefix(TOKEN_PREFIX) {
        (CredKind::Token, rest)
    } else {
        (CredKind::Secret, value.strip_prefix(SECRET_PREFIX)?)
    };
    (kind.1.len() == BODY_LEN && kind.1.bytes().all(is_b64url)).then_some(kind.0)
}

/// Check a raw `authorization` header value: at most [`MAX_HEADER_LEN`]
/// bytes, printable ASCII, the `Bearer` scheme (any case) and one
/// well-formed value. Never panics on any input.
pub fn parse_bearer(raw: &[u8]) -> Result<(CredKind, &str), Malformed> {
    if raw.len() > MAX_HEADER_LEN {
        return Err(Malformed::TooLong);
    }
    if !raw.iter().all(|b| (0x20..0x7f).contains(b)) {
        return Err(Malformed::NotAscii);
    }
    // Printable ASCII is valid UTF-8.
    let s = std::str::from_utf8(raw).map_err(|_| Malformed::NotAscii)?;
    let (scheme, value) = s.split_once(' ').ok_or(Malformed::Scheme)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(Malformed::Scheme);
    }
    let kind = check_format(value).ok_or(Malformed::Format)?;
    Ok((kind, value))
}

/// The digest a server keeps for a credential: SHA-256 of its text.
pub fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// [`digest`] as 64 lower-case hex characters (the token file's `sha256`).
pub fn digest_hex(value: &str) -> String {
    use std::fmt::Write as _;
    digest(value)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The `authorization` metadata value for `value`, marked sensitive (so
/// `Debug` prints `Sensitive` and HTTP/2 never indexes it). `None` when the
/// value is not a well-formed credential.
pub fn bearer_value(value: &str) -> Option<MetadataValue<Ascii>> {
    check_format(value)?;
    let mut v: MetadataValue<Ascii> = format!("Bearer {value}").parse().ok()?;
    v.set_sensitive(true);
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BODY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn formats() {
        assert_eq!(BODY.len(), BODY_LEN);
        assert_eq!(check_format(&format!("mgt_{BODY}")), Some(CredKind::Token));
        assert_eq!(check_format(&format!("mgc_{BODY}")), Some(CredKind::Secret));
        assert_eq!(check_format(&format!("mgx_{BODY}")), None);
        assert_eq!(check_format(&format!("mgt_{BODY}A")), None);
        assert_eq!(check_format(&format!("mgt_{}", &BODY[1..])), None);
        assert_eq!(check_format(&format!("mgt_{}+", &BODY[1..])), None);
        let ok = format!("Bearer mgt_{BODY}");
        assert_eq!(
            parse_bearer(ok.as_bytes()).unwrap(),
            (CredKind::Token, &ok[7..])
        );
        assert!(parse_bearer(format!("bearer mgc_{BODY}").as_bytes()).is_ok());
        assert_eq!(
            parse_bearer(format!("Basic mgt_{BODY}").as_bytes()),
            Err(Malformed::Scheme)
        );
        assert_eq!(parse_bearer(b"Bearer"), Err(Malformed::Scheme));
        assert_eq!(
            parse_bearer(format!("Bearer  mgt_{BODY}").as_bytes()),
            Err(Malformed::Format)
        );
        assert_eq!(parse_bearer(&[b'a'; 129]), Err(Malformed::TooLong));
        assert_eq!(parse_bearer(b"Bearer \xff"), Err(Malformed::NotAscii));
        assert_eq!(parse_bearer(b"Bearer \tx"), Err(Malformed::NotAscii));
    }

    #[test]
    fn the_header_is_sensitive_and_its_debug_hides_it() {
        let v = bearer_value(&format!("mgt_{BODY}")).unwrap();
        assert!(v.is_sensitive());
        assert!(!format!("{v:?}").contains(BODY));
        assert!(bearer_value("not a token").is_none());
    }

    #[test]
    fn digest_hex_is_sha256() {
        assert_eq!(
            digest_hex("test"),
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    proptest! {
        /// ADR 0012 D10: the header parser never panics on arbitrary bytes.
        #[test]
        fn parse_bearer_never_panics(raw in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = parse_bearer(&raw);
        }
    }
}
