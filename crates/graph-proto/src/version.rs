//! The protocol version on every call (ADR 0004 D1). `Hello` exchanges the
//! version once per connection; on top of that the client sends it as the
//! `mg-protocol-version` metadata header on **every** call
//! ([`SendVersion`]), and the server refuses a call whose header names
//! another version with `FAILED_PRECONDITION` and a typed `Protocol` detail
//! ([`CheckVersion`]). A call without the header (a generic gRPC tool) is
//! let through: the header is a guard against a mismatched client that
//! skipped `Hello`, not an authentication step.
use crate::error::WireError;
use crate::PROTOCOL_VERSION;
use tonic::metadata::MetadataMap;
use tonic::service::Interceptor;
use tonic::{Request, Status};

/// The metadata key carrying the client's protocol version.
pub const PROTOCOL_VERSION_HEADER: &str = "mg-protocol-version";

/// Client-side interceptor: stamps [`PROTOCOL_VERSION`] on every request.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendVersion;

impl Interceptor for SendVersion {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        req.metadata_mut().insert(
            PROTOCOL_VERSION_HEADER,
            PROTOCOL_VERSION
                .to_string()
                .parse()
                .expect("a number is valid ASCII metadata"),
        );
        Ok(req)
    }
}

/// Server-side interceptor: [`check_version`] on every request.
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckVersion;

impl Interceptor for CheckVersion {
    fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
        check_version(req.metadata())?;
        Ok(req)
    }
}

/// `Ok` when the header is absent or names [`PROTOCOL_VERSION`]; else
/// `FAILED_PRECONDITION` with a `Protocol` detail naming both versions.
pub fn check_version(md: &MetadataMap) -> Result<(), Status> {
    let Some(v) = md.get(PROTOCOL_VERSION_HEADER) else {
        return Ok(());
    };
    let text = v.to_str().unwrap_or("<not ascii>");
    if text.trim().parse::<u32>().ok() == Some(PROTOCOL_VERSION) {
        return Ok(());
    }
    Err(WireError::Protocol(format!(
        "client speaks protocol version {text}, this server speaks {PROTOCOL_VERSION}"
    ))
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    #[test]
    fn send_then_check_accepts_and_a_mismatch_is_a_typed_protocol_error() {
        let req = SendVersion.call(Request::new(())).unwrap();
        assert_eq!(
            req.metadata().get(PROTOCOL_VERSION_HEADER).unwrap(),
            PROTOCOL_VERSION.to_string().as_str()
        );
        assert!(CheckVersion.call(req).is_ok());
        assert!(
            CheckVersion.call(Request::new(())).is_ok(),
            "absent is allowed"
        );
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(PROTOCOL_VERSION_HEADER, "99".parse().unwrap());
        let st = CheckVersion.call(req).unwrap_err();
        assert_eq!(st.code(), Code::FailedPrecondition);
        assert!(matches!(
            WireError::from(&st),
            WireError::Protocol(ref m) if m.contains("version 99")
        ));
    }
}
