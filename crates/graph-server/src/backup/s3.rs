//! [`S3Sink`]: `s3://bucket/prefix` over plain HTTP (ADR 0006 E3-E6, epic
//! story 36), a small hand-written S3 client: SigV4 ([`super::sigv4`]),
//! PUT (with `x-amz-content-sha256`), GET, HEAD, ListObjectsV2, DELETE, and
//! multipart above [`MULTIPART_THRESHOLD`] (aborted on any error). No AWS
//! SDK: it runs on the `hyper` / `hyper-util` already in the tree, a
//! kept-alive HTTP/1.1 connection, on a one-worker tokio runtime the sink
//! owns (the [`BackupSink`] trait is synchronous; the uploader
//! calls it from its own thread).
//!
//! Stage 1 speaks `http://` only (E5): an `https://` endpoint, or none (AWS
//! itself is HTTPS-only), is refused with guidance to a TLS sidecar and
//! #104. Addressing is path-style (`http://host/bucket/key`) by default,
//! virtual-hosted (`http://bucket.host/key`) with `--backup-virtual-host`.
//!
//! Integrity: every request body is signed with its own SHA-256
//! (`x-amz-content-sha256`, per part for a multipart upload), so the server
//! refuses a body that changed on the way; after an upload a HEAD checks
//! the stored size. A multipart upload that fails is aborted, so no parts
//! linger; a crash mid-upload leaves only an unfinished upload, never an
//! object under the key, and retention aborts unfinished uploads older than
//! the orphan age ([`BackupSink::sweep_incomplete`]; a bucket lifecycle rule
//! is the belt and braces). One HTTP/1.1 connection is kept alive and
//! reused while it is ready, and replaced when it breaks.
use super::creds::{self, Credentials};
use super::sigv4;
use super::{BackupSink, ObjectInfo};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Objects larger than this are uploaded in parts (the story's 64 MiB).
pub const MULTIPART_THRESHOLD: usize = 64 << 20;
/// The size of each part of a multipart upload (S3's minimum is 5 MiB).
pub const PART_SIZE: usize = 16 << 20;
/// The default region (`--backup-region`).
pub const DEFAULT_REGION: &str = "us-east-1";
/// The idle timeout of each step of a request: connecting, waiting for a
/// reusable connection, each chunk of a response body, and the response
/// head, whose wait also grows with the request body at [`MIN_RATE`] (a
/// 16 MiB part may take `60 s + 16 s`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// The slowest upload rate a request body is given time for (bytes/s).
pub const MIN_RATE: u64 = 1 << 20;
/// The largest response body read into memory (listings, errors); a GET
/// streams instead.
pub const MAX_RESPONSE: u64 = 8 << 20;

/// The guidance of an HTTPS refusal.
const HTTPS_GUIDANCE: &str = "stage 1 of ADR 0006 speaks plain http:// only (no pure-Rust TLS \
     provider passes the build's gate yet, #104). Reach an HTTPS endpoint (AWS S3 itself) \
     through a local TLS sidecar (stunnel, envoy) and point --backup-endpoint at it, e.g. \
     http://127.0.0.1:8080, or back up to a file:// directory and `aws s3 sync` it";

/// A parsed `s3://bucket/prefix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Url {
    pub bucket: String,
    /// No leading or trailing `/`; may be empty.
    pub prefix: String,
}

impl std::fmt::Display for S3Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.prefix.is_empty() {
            write!(f, "s3://{}", self.bucket)
        } else {
            write!(f, "s3://{}/{}", self.bucket, self.prefix)
        }
    }
}

/// Whether `b` is a bucket name S3 accepts (3-63 of `a-z 0-9 . -`,
/// starting and ending with a letter or digit).
pub fn valid_bucket(b: &str) -> bool {
    let ok = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (3..=63).contains(&b.len())
        && b.bytes().all(|c| ok(c) || c == b'.' || c == b'-')
        && ok(b.as_bytes()[0])
        && ok(b.as_bytes()[b.len() - 1])
}

/// Whether `k` is a `/`-separated relative key with no empty, `.` or `..`
/// segment and no control character or backslash.
pub fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && !k.contains('\\')
        && !k.chars().any(char::is_control)
        && k.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// Parse `s3://bucket[/prefix[/]]`.
pub fn parse_s3_url(url: &str) -> Result<S3Url, String> {
    let rest = url
        .strip_prefix("s3://")
        .ok_or_else(|| format!("`{url}`: not an s3:// URL"))?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if !valid_bucket(bucket) {
        return Err(format!(
            "`{url}`: `{bucket}` is not a bucket name (3-63 of a-z, 0-9, `.` and `-`)"
        ));
    }
    let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
    if !prefix.is_empty() && !valid_key(prefix) {
        return Err(format!(
            "`{url}`: the prefix `{prefix}` has an empty, `.` or `..` segment, or a control \
             character"
        ));
    }
    Ok(S3Url {
        bucket: bucket.into(),
        prefix: prefix.into(),
    })
}

/// A parsed `--backup-endpoint http://host[:port][/]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http://{}", self.authority(&self.host))
    }
}

impl Endpoint {
    /// `host[:port]` (the port only when it is not 80), with `host`.
    fn authority(&self, host: &str) -> String {
        if self.port == 80 {
            host.to_string()
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// Parse an endpoint; `https://` is refused with guidance (E5).
pub fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    if s.get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
    {
        return Err(format!(
            "--backup-endpoint `{s}`: https is refused: {HTTPS_GUIDANCE}"
        ));
    }
    let rest = s
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &s[7..])
        .ok_or_else(|| format!("--backup-endpoint `{s}`: expected http://host[:port]"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains(['/', '?', '#', '@']) || rest.is_empty() {
        return Err(format!(
            "--backup-endpoint `{s}`: expected http://host[:port] (no path, query or user)"
        ));
    }
    // `[v6]:port`, `[v6]`, `host:port` or `host`.
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let (h, after) = v6
            .split_once(']')
            .ok_or_else(|| format!("--backup-endpoint `{s}`: unclosed `[`"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return Err(format!("--backup-endpoint `{s}`: junk after `]`")),
        };
        (format!("[{h}]"), port)
    } else {
        match rest.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (rest.to_string(), None),
        }
    };
    let port = match port {
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("--backup-endpoint `{s}`: `{p}` is not a port"))?,
        None => 80,
    };
    if host.is_empty() || host == "[]" {
        return Err(format!("--backup-endpoint `{s}`: no host"));
    }
    Ok(Endpoint { host, port })
}

/// Everything an [`S3Sink`] needs beyond the URL.
#[derive(Clone)]
pub struct S3Options {
    /// `--backup-endpoint` (required in stage 1).
    pub endpoint: Option<String>,
    /// `--backup-region`.
    pub region: String,
    /// `--backup-virtual-host`: `bucket.host` instead of `host/bucket`.
    pub virtual_host: bool,
    /// `--backup-credentials-file`.
    pub credentials_file: Option<PathBuf>,
    /// `--backup-profile`.
    pub profile: Option<String>,
    /// Read `AWS_*` from the environment first (tests turn it off, so a
    /// developer's own credentials never leak into them).
    pub credentials_from_env: bool,
    /// [`MULTIPART_THRESHOLD`] (tests lower it).
    pub multipart_threshold: usize,
    /// [`PART_SIZE`] (tests lower it).
    pub part_size: usize,
    /// [`DEFAULT_TIMEOUT`] (see there for what it bounds).
    pub timeout: Duration,
    /// Tests: connect here whatever the host (a virtual-hosted name that
    /// does not resolve), like curl's `--connect-to`.
    pub connect_to: Option<SocketAddr>,
}

impl Default for S3Options {
    fn default() -> Self {
        Self {
            endpoint: None,
            region: DEFAULT_REGION.into(),
            virtual_host: false,
            credentials_file: None,
            profile: None,
            credentials_from_env: true,
            multipart_threshold: MULTIPART_THRESHOLD,
            part_size: PART_SIZE,
            timeout: DEFAULT_TIMEOUT,
            connect_to: None,
        }
    }
}

impl std::fmt::Debug for S3Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No secrets live here (only the credentials file's path).
        f.debug_struct("S3Options")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("virtual_host", &self.virtual_host)
            .field("credentials_file", &self.credentials_file)
            .field("profile", &self.profile)
            .field("credentials_from_env", &self.credentials_from_env)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("part_size", &self.part_size)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// The `s3://` sink.
pub struct S3Sink {
    url: S3Url,
    endpoint: Endpoint,
    opts: S3Options,
    creds: Credentials,
    /// The kept-alive connection (HTTP/1.1), reused while it is ready.
    conn: tokio::sync::Mutex<Option<hyper::client::conn::http1::SendRequest<Full<Bytes>>>>,
    rt: Option<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for S3Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Sink")
            .field("url", &self.url)
            .field("endpoint", &self.endpoint)
            .field("opts", &self.opts)
            .field("creds", &self.creds)
            .finish()
    }
}

impl Drop for S3Sink {
    fn drop(&mut self) {
        // Safe from inside an async context too (a plain drop would panic
        // there).
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// A response whose status was checked, body unread.
type Resp = hyper::Response<Incoming>;

fn io_other(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}

fn timed_out(what: &str, t: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("S3 {what}: no progress for {t:?}"),
    )
}

/// Every text node of `xml` (unescaped) with its element path
/// (`Error/Code`), and empty elements with empty text, via quick-xml.
pub fn xml_texts(xml: &[u8]) -> Result<Vec<(String, String)>, String> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(xml);
    r.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                stack.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned())
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let mut path = stack.join("/");
                path.push('/');
                path.push_str(&name);
                out.push((path, String::new()));
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().map_err(|e| e.to_string())?.into_owned();
                out.push((stack.join("/"), text));
            }
            Ok(Event::CData(t)) => {
                out.push((stack.join("/"), String::from_utf8_lossy(&t).into_owned()));
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("not valid XML: {e}")),
        }
        buf.clear();
    }
    Ok(out)
}

/// S3's `<Error><Code>..</Code><Message>..</Message></Error>`, if any.
fn s3_error_code(body: &[u8]) -> Option<String> {
    xml_texts(body)
        .ok()?
        .into_iter()
        .find(|(k, _)| k == "Error/Code")
        .map(|(_, v)| v)
}

fn s3_error_text(body: &[u8]) -> Option<String> {
    let t = xml_texts(body).ok()?;
    let get = |p: &str| t.iter().find(|(k, _)| k == p).map(|(_, v)| v.clone());
    let code = get("Error/Code")?;
    Some(match get("Error/Message") {
        Some(m) if !m.is_empty() => format!("{code}: {m}"),
        _ => code,
    })
}

/// S3 error codes that are transient although sent with a 4xx status.
pub const RETRYABLE_CODES: [&str; 6] = [
    "RequestTimeout",
    "BadDigest",
    "XAmzContentSHA256Mismatch",
    "IncompleteBody",
    "SlowDown",
    "InternalError",
];

/// The `io::ErrorKind` of an S3 error (status and `<Code>`, if any): 404
/// `NotFound`; a code in [`RETRYABLE_CODES`] `Other`; 403
/// `PermissionDenied` (including `RequestTimeTooSkewed` and
/// `AccessDenied`); another 4xx that retrying cannot fix (405, 411, 413,
/// `InvalidArgument`, `InvalidRequest`, ...) `InvalidInput`; and anything
/// else (5xx, 408, 409, 429) `Other`. The uploader does not retry
/// `PermissionDenied` or `InvalidInput`: the failure is counted at once
/// and the next snapshot tries again.
pub fn error_kind(status: u16, code: Option<&str>) -> std::io::ErrorKind {
    if status == 404 {
        return std::io::ErrorKind::NotFound;
    }
    if code.is_some_and(|c| RETRYABLE_CODES.contains(&c)) {
        return std::io::ErrorKind::Other;
    }
    status_kind(status)
}

/// [`error_kind`] from the status alone (a HEAD has no body).
pub fn status_kind(status: u16) -> std::io::ErrorKind {
    match status {
        404 => std::io::ErrorKind::NotFound,
        403 => std::io::ErrorKind::PermissionDenied,
        408 | 409 | 429 => std::io::ErrorKind::Other,
        400..=499 => std::io::ErrorKind::InvalidInput,
        _ => std::io::ErrorKind::Other,
    }
}

/// The escaped text of an XML element's content.
fn xml_escape(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

impl S3Sink {
    /// Parse the URL and endpoint, resolve credentials (environment, then
    /// the file) and build the sink. Nothing is sent yet.
    pub fn new(url: &str, opts: S3Options) -> Result<Self, String> {
        let url = parse_s3_url(url)?;
        let endpoint = match opts.endpoint.as_deref() {
            Some(e) => parse_endpoint(e)?,
            None => {
                return Err(format!(
                    "`{url}` needs --backup-endpoint http://host:port (MinIO, Ceph RGW, R2, B2, \
                     Garage, or a TLS sidecar in front of AWS): the default AWS endpoint is \
                     HTTPS, and {HTTPS_GUIDANCE}"
                ))
            }
        };
        if opts.region.trim().is_empty() || opts.region.contains(['/', ' ']) {
            return Err(format!("--backup-region `{}` is not a region", opts.region));
        }
        let env = |k: &str| std::env::var(k).ok();
        let none = |_: &str| None;
        let lookup: &dyn Fn(&str) -> Option<String> = if opts.credentials_from_env {
            &env
        } else {
            &none
        };
        let creds = creds::resolve(
            lookup,
            opts.credentials_file.as_deref(),
            opts.profile.as_deref(),
        )?;
        Self::with_credentials(url, endpoint, opts, creds)
    }

    /// With explicit credentials (tests, `FakeS3`).
    pub fn with_credentials(
        url: S3Url,
        endpoint: Endpoint,
        opts: S3Options,
        creds: Credentials,
    ) -> Result<Self, String> {
        if opts.part_size == 0 || opts.multipart_threshold == 0 {
            return Err("the S3 part size and multipart threshold must be positive".into());
        }
        // One worker drives the connections; any number of threads may
        // block on it (the uploader, a restore, a test).
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("backup-s3")
            .build()
            .map_err(|e| format!("starting the S3 client runtime: {e}"))?;
        tracing::info!(
            url = %url,
            endpoint = %endpoint,
            region = %opts.region,
            virtual_host = opts.virtual_host,
            credentials = ?creds.source,
            "S3 backup sink"
        );
        Ok(Self {
            url,
            endpoint,
            opts,
            creds,
            conn: tokio::sync::Mutex::new(None),
            rt: Some(rt),
        })
    }

    pub fn url(&self) -> &S3Url {
        &self.url
    }

    /// The object key of a sink key.
    fn object_key(&self, key: &str) -> std::io::Result<String> {
        if !valid_key(key) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("backup key `{key}` is not a relative object name"),
            ));
        }
        Ok(if self.url.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.url.prefix)
        })
    }

    /// Run `f` on the sink's runtime (never from inside another runtime:
    /// that would panic, so it is an error instead).
    fn run<T>(
        &self,
        f: impl std::future::Future<Output = std::io::Result<T>>,
    ) -> std::io::Result<T> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(io_other(
                "the S3 backup sink is synchronous: call it off the async runtime (e.g. \
                 spawn_blocking)"
                    .into(),
            ));
        }
        self.rt.as_ref().expect("runtime until drop").block_on(f)
    }

    /// (host header, path) of an object key (`None`: the bucket itself).
    fn target(&self, key: Option<&str>) -> (String, String) {
        let enc = key.map(sigv4::encode_path).unwrap_or_default();
        if self.opts.virtual_host {
            let host = self
                .endpoint
                .authority(&format!("{}.{}", self.url.bucket, self.endpoint.host));
            (host, format!("/{enc}"))
        } else {
            let host = self.endpoint.authority(&self.endpoint.host);
            let path = match key {
                Some(_) => format!("/{}/{enc}", self.url.bucket),
                None => format!("/{}", self.url.bucket),
            };
            (host, path)
        }
    }

    /// Sign and send one request; the response with its body unread.
    async fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(String, String)],
        body: Bytes,
        extra: &[(&str, String)],
    ) -> std::io::Result<Resp> {
        let t = self.opts.timeout;
        let (host, path) = self.target(key);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| {
                io_other("S3: the system clock is before 1970, so no request can be signed".into())
            })?;
        let amz_date = sigv4::amz_date(now);
        let payload = sigv4::sha256_hex(&body);
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), host.clone()),
            ("x-amz-content-sha256".into(), payload.clone()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        if let Some(tok) = &self.creds.session_token {
            headers.push(("x-amz-security-token".into(), tok.expose().to_string()));
        }
        for (k, v) in extra {
            headers.push((k.to_string(), v.clone()));
        }
        let auth = sigv4::authorization(
            &sigv4::Request {
                method,
                path: &path,
                query,
                headers: &headers,
                payload_sha256: &payload,
            },
            &sigv4::Scope {
                amz_date: &amz_date,
                region: &self.opts.region,
                service: "s3",
            },
            &self.creds.access_key_id,
            self.creds.secret_access_key.expose(),
        );
        let q = sigv4::canonical_query(query);
        let uri = if q.is_empty() {
            path
        } else {
            format!("{path}?{q}")
        };
        let build = |body: Bytes| {
            let mut req = hyper::Request::builder().method(method).uri(&uri);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req.header("authorization", auth.as_str())
                .header("content-length", body.len())
                .body(Full::new(body))
                .map_err(|e| io_other(format!("S3 {method} {uri}: building the request: {e}")))
        };
        // The response head may take as long as sending the body does.
        let head_wait = t + Duration::from_secs(body.len() as u64 / MIN_RATE);
        let mut slot = self.conn.lock().await;
        // A kept-alive connection first (it may have been closed by the
        // server while idle: then once more on a fresh one).
        for fresh in [false, true] {
            let mut sender = match (fresh, slot.take()) {
                (false, Some(mut s)) => match tokio::time::timeout(t, s.ready()).await {
                    Ok(Ok(())) => s,
                    _ => continue,
                },
                (false, None) => continue,
                (true, _) => self.connect(method, &uri).await?,
            };
            let sent = tokio::time::timeout(head_wait, sender.send_request(build(body.clone())?))
                .await
                .map_err(|_| timed_out(&format!("{method} {uri}"), head_wait))?;
            match sent {
                Ok(resp) => {
                    *slot = Some(sender);
                    return Ok(resp);
                }
                // A reused connection that the server had closed: retry on a
                // fresh one if the request never went out, or if it may
                // safely be repeated (everything but the POSTs, which
                // create or complete a multipart upload).
                Err(e)
                    if !fresh
                        && (e.is_closed()
                            || e.is_canceled()
                            || (e.is_incomplete_message() && method != "POST")) =>
                {
                    continue
                }
                Err(e) => return Err(io_other(format!("S3 {method} {uri}: {e}"))),
            }
        }
        unreachable!("the fresh connection returns")
    }

    /// A new HTTP/1.1 connection to the endpoint.
    async fn connect(
        &self,
        method: &str,
        uri: &str,
    ) -> std::io::Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>> {
        let t = self.opts.timeout;
        let connect = async {
            match self.opts.connect_to {
                Some(a) => tokio::net::TcpStream::connect(a).await,
                None => {
                    let host = self
                        .endpoint
                        .host
                        .trim_start_matches('[')
                        .trim_end_matches(']');
                    let h = if self.opts.virtual_host {
                        format!("{}.{host}", self.url.bucket)
                    } else {
                        host.to_string()
                    };
                    tokio::net::TcpStream::connect((h.as_str(), self.endpoint.port)).await
                }
            }
        };
        let stream = tokio::time::timeout(t, connect)
            .await
            .map_err(|_| timed_out(&format!("connecting to {}", self.endpoint), t))?
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("S3: connecting to {}: {e}", self.endpoint),
                )
            })?;
        let _ = stream.set_nodelay(true);
        let (sender, conn) = tokio::time::timeout(
            t,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|_| timed_out(&format!("{method} {uri}"), t))?
        .map_err(|e| io_other(format!("S3 {method} {uri}: {e}")))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(sender)
    }

    /// Stream a response body into `sink` (with the idle timeout); the
    /// bytes.
    async fn drain(
        &self,
        resp: Resp,
        what: &str,
        dst: &mut dyn Write,
        max: Option<u64>,
    ) -> std::io::Result<u64> {
        let t = self.opts.timeout;
        let mut body = resp.into_body();
        let mut n = 0u64;
        loop {
            let frame = tokio::time::timeout(t, body.frame())
                .await
                .map_err(|_| timed_out(what, t))?;
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|e| io_other(format!("S3 {what}: {e}")))?;
            if let Ok(data) = frame.into_data() {
                n += data.len() as u64;
                if max.is_some_and(|m| n > m) {
                    return Err(io_other(format!(
                        "S3 {what}: the response is over {} bytes",
                        max.unwrap_or(0)
                    )));
                }
                dst.write_all(&data)?;
            }
        }
        Ok(n)
    }

    async fn body_of(&self, resp: Resp, what: &str) -> std::io::Result<Vec<u8>> {
        let mut v = Vec::new();
        self.drain(resp, what, &mut v, Some(MAX_RESPONSE)).await?;
        Ok(v)
    }

    /// The response, or its S3 error as an `io::Error` (404 `NotFound`,
    /// 403 `PermissionDenied`, the code and message verbatim, e.g.
    /// `RequestTimeTooSkewed`, which is a 403 and so is not retried; the
    /// next snapshot tries again; see [`error_kind`]).
    async fn check(&self, resp: Resp, what: &str) -> std::io::Result<Resp> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = self.body_of(resp, what).await.unwrap_or_default();
        let detail = s3_error_text(&body).unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("unexpected status")
                .to_string()
        });
        Err(std::io::Error::new(
            error_kind(status.as_u16(), s3_error_code(&body).as_deref()),
            format!("S3 {what}: HTTP {} {detail}", status.as_u16()),
        ))
    }

    async fn put_object(&self, okey: &str, body: Bytes) -> std::io::Result<()> {
        let what = format!("PUT `{okey}`");
        let r = self.send("PUT", Some(okey), &[], body, &[]).await?;
        let r = self.check(r, &what).await?;
        self.body_of(r, &what).await?;
        Ok(())
    }

    /// The stored size of `okey` (HEAD).
    async fn head(&self, okey: &str) -> std::io::Result<u64> {
        let what = format!("HEAD `{okey}`");
        let r = self
            .send("HEAD", Some(okey), &[], Bytes::new(), &[])
            .await?;
        if !r.status().is_success() {
            return Err(std::io::Error::new(
                status_kind(r.status().as_u16()),
                format!("S3 {what}: HTTP {}", r.status().as_u16()),
            ));
        }
        r.headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| io_other(format!("S3 {what}: no Content-Length")))
    }

    async fn verify_size(&self, okey: &str, want: u64) -> std::io::Result<()> {
        let got = self.head(okey).await?;
        if got != want {
            // Best effort: the object under the key is not what was sent.
            // (A later `.meta` could never commit it anyway: the uploader
            // only writes the `.meta` after this put succeeds.)
            if let Err(e) = self.delete_object(okey).await {
                tracing::warn!(key = %okey, error = %e, "backup: removing a mis-sized object failed");
            }
            return Err(io_other(format!(
                "S3 PUT `{okey}`: stored {got} bytes, sent {want}"
            )));
        }
        Ok(())
    }

    async fn delete_object(&self, okey: &str) -> std::io::Result<()> {
        let what = format!("DELETE `{okey}`");
        let r = self
            .send("DELETE", Some(okey), &[], Bytes::new(), &[])
            .await?;
        match self.check(r, &what).await {
            Ok(r) => self.body_of(r, &what).await.map(|_| ()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// (key, upload id, initiated) of every multipart upload in progress
    /// under the object prefix `full` (ListMultipartUploads, paged).
    async fn list_uploads(&self, full: &str) -> std::io::Result<Vec<(String, String, SystemTime)>> {
        let mut out = Vec::new();
        let mut markers: Option<(String, String)> = None;
        loop {
            let mut q = vec![
                ("uploads".to_string(), String::new()),
                ("prefix".to_string(), full.to_string()),
            ];
            if let Some((k, u)) = &markers {
                q.push(("key-marker".into(), k.clone()));
                q.push(("upload-id-marker".into(), u.clone()));
            }
            let what = format!("ListMultipartUploads `{full}`");
            let r = self.send("GET", None, &q, Bytes::new(), &[]).await?;
            let r = self.check(r, &what).await?;
            let body = self.body_of(r, &what).await?;
            let t = xml_texts(&body).map_err(|e| io_other(format!("S3 {what}: {e}")))?;
            let (mut truncated, mut nk, mut nu) = (false, None, None);
            let mut cur: Option<(String, String, SystemTime)> = None;
            for (path, text) in t {
                match path.as_str() {
                    "ListMultipartUploadsResult/Upload/Key" => {
                        out.extend(cur.take());
                        cur = Some((text, String::new(), SystemTime::now()));
                    }
                    "ListMultipartUploadsResult/Upload/UploadId" => {
                        if let Some(c) = cur.as_mut() {
                            c.1 = text;
                        }
                    }
                    "ListMultipartUploadsResult/Upload/Initiated" => {
                        if let (Some(c), Some(s)) = (cur.as_mut(), sigv4::parse_iso8601(&text)) {
                            c.2 = SystemTime::UNIX_EPOCH + Duration::from_secs(s);
                        }
                    }
                    "ListMultipartUploadsResult/IsTruncated" => truncated = text == "true",
                    "ListMultipartUploadsResult/NextKeyMarker" => nk = Some(text),
                    "ListMultipartUploadsResult/NextUploadIdMarker" => nu = Some(text),
                    _ => {}
                }
            }
            out.extend(cur.take());
            if !truncated {
                break;
            }
            let next = (nk.unwrap_or_default(), nu.unwrap_or_default());
            if next.0.is_empty() || markers.as_ref() == Some(&next) {
                return Err(io_other(format!(
                    "S3 {what}: truncated without new markers"
                )));
            }
            markers = Some(next);
        }
        Ok(out)
    }

    fn upload_query(id: &str) -> Vec<(String, String)> {
        vec![("uploadId".into(), id.into())]
    }

    async fn create_multipart(&self, okey: &str) -> std::io::Result<String> {
        let what = format!("CreateMultipartUpload `{okey}`");
        let q = [("uploads".to_string(), String::new())];
        let r = self.send("POST", Some(okey), &q, Bytes::new(), &[]).await?;
        let r = self.check(r, &what).await?;
        let body = self.body_of(r, &what).await?;
        let t = xml_texts(&body).map_err(|e| io_other(format!("S3 {what}: {e}")))?;
        t.into_iter()
            .find(|(k, _)| k == "InitiateMultipartUploadResult/UploadId")
            .map(|(_, v)| v)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| io_other(format!("S3 {what}: no UploadId in the response")))
    }

    async fn upload_part(
        &self,
        okey: &str,
        id: &str,
        n: u32,
        body: Bytes,
    ) -> std::io::Result<String> {
        let what = format!("UploadPart {n} of `{okey}`");
        let mut q = Self::upload_query(id);
        q.push(("partNumber".into(), n.to_string()));
        let r = self.send("PUT", Some(okey), &q, body, &[]).await?;
        let r = self.check(r, &what).await?;
        let etag = r
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| io_other(format!("S3 {what}: no ETag")))?;
        self.body_of(r, &what).await?;
        Ok(etag)
    }

    async fn complete(&self, okey: &str, id: &str, etags: &[String]) -> std::io::Result<()> {
        let what = format!("CompleteMultipartUpload `{okey}`");
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, e) in etags.iter().enumerate() {
            xml.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                xml_escape(e)
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let q = Self::upload_query(id);
        let r = self
            .send("POST", Some(okey), &q, Bytes::from(xml), &[])
            .await?;
        let r = self.check(r, &what).await?;
        // S3 may answer 200 and still fail, with an <Error> body.
        let body = self.body_of(r, &what).await?;
        if let Some(e) = s3_error_text(&body) {
            return Err(io_other(format!("S3 {what}: {e}")));
        }
        Ok(())
    }

    async fn abort(&self, okey: &str, id: &str) -> std::io::Result<()> {
        let what = format!("AbortMultipartUpload `{okey}`");
        let q = Self::upload_query(id);
        let r = self
            .send("DELETE", Some(okey), &q, Bytes::new(), &[])
            .await?;
        match self.check(r, &what).await {
            Ok(r) => self.body_of(r, &what).await.map(|_| ()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Read up to `max` bytes (fewer only at the end of `src`).
    fn read_chunk(src: &mut dyn Read, max: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(max.min(1 << 20));
        src.take(max as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn put_multipart(
        &self,
        okey: &str,
        first: Vec<u8>,
        src: &mut dyn Read,
    ) -> std::io::Result<u64> {
        let id = self.run(self.create_multipart(okey))?;
        let mut parts = Vec::new();
        let result = (|| {
            let mut total = 0u64;
            let mut src = std::io::Cursor::new(first).chain(src);
            loop {
                let chunk = Self::read_chunk(&mut src, self.opts.part_size)?;
                if chunk.is_empty() && !parts.is_empty() {
                    break;
                }
                let len = chunk.len();
                let n = u32::try_from(parts.len() + 1)
                    .ok()
                    .filter(|n| *n <= 10_000)
                    .ok_or_else(|| io_other(format!("S3 PUT `{okey}`: over 10000 parts")))?;
                parts.push(self.run(self.upload_part(okey, &id, n, Bytes::from(chunk)))?);
                total += len as u64;
                if len < self.opts.part_size {
                    break;
                }
            }
            self.run(self.complete(okey, &id, &parts))?;
            Ok::<u64, std::io::Error>(total)
        })();
        match result {
            Ok(total) => Ok(total),
            Err(e) => {
                if let Err(a) = self.run(self.abort(okey, &id)) {
                    tracing::warn!(key = %okey, error = %a, "backup: aborting the multipart upload failed");
                }
                Err(e)
            }
        }
    }
}

impl BackupSink for S3Sink {
    fn put(&self, key: &str, src: &mut dyn Read) -> std::io::Result<u64> {
        let okey = self.object_key(key)?;
        let first = Self::read_chunk(src, self.opts.multipart_threshold.saturating_add(1))?;
        let n = if first.len() <= self.opts.multipart_threshold {
            let n = first.len() as u64;
            self.run(self.put_object(&okey, Bytes::from(first)))?;
            n
        } else {
            self.put_multipart(&okey, first, src)?
        };
        self.run(self.verify_size(&okey, n))?;
        Ok(n)
    }

    fn get(&self, key: &str, dst: &mut dyn Write) -> std::io::Result<u64> {
        let okey = self.object_key(key)?;
        let what = format!("GET `{okey}`");
        self.run(async {
            let r = self
                .send("GET", Some(&okey), &[], Bytes::new(), &[])
                .await?;
            let r = self.check(r, &what).await?;
            let want = r
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let n = self.drain(r, &what, dst, None).await?;
            if want.is_some_and(|w| w != n) {
                return Err(io_other(format!(
                    "S3 {what}: got {n} bytes of {}",
                    want.unwrap_or(0)
                )));
            }
            Ok(n)
        })
    }

    fn list(&self, prefix: &str) -> std::io::Result<Vec<ObjectInfo>> {
        let root = if self.url.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.url.prefix)
        };
        let full = format!("{root}{prefix}");
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), full.clone()),
            ];
            if let Some(t) = &token {
                q.push(("continuation-token".into(), t.clone()));
            }
            let what = format!("ListObjectsV2 `{full}`");
            let body = self.run(async {
                let r = self.send("GET", None, &q, Bytes::new(), &[]).await?;
                let r = self.check(r, &what).await?;
                self.body_of(r, &what).await
            })?;
            let t = xml_texts(&body).map_err(|e| io_other(format!("S3 {what}: {e}")))?;
            let mut truncated = false;
            let mut next = None;
            let mut cur: Option<(String, u64, SystemTime)> = None;
            let flush = |cur: &mut Option<(String, u64, SystemTime)>, out: &mut Vec<ObjectInfo>| {
                if let Some((k, size, modified)) = cur.take() {
                    if let Some(rel) = k.strip_prefix(&root) {
                        out.push(ObjectInfo {
                            key: rel.to_string(),
                            size,
                            modified,
                        });
                    }
                }
            };
            for (path, text) in t {
                match path.as_str() {
                    "ListBucketResult/Contents/Key" => {
                        flush(&mut cur, &mut out);
                        cur = Some((text, 0, SystemTime::UNIX_EPOCH));
                    }
                    "ListBucketResult/Contents/Size" => {
                        if let Some(c) = cur.as_mut() {
                            c.1 = text.parse().unwrap_or(0);
                        }
                    }
                    "ListBucketResult/Contents/LastModified" => {
                        if let (Some(c), Some(s)) = (cur.as_mut(), sigv4::parse_iso8601(&text)) {
                            c.2 = SystemTime::UNIX_EPOCH + Duration::from_secs(s);
                        }
                    }
                    "ListBucketResult/IsTruncated" => truncated = text == "true",
                    "ListBucketResult/NextContinuationToken" => next = Some(text),
                    _ => {}
                }
            }
            flush(&mut cur, &mut out);
            match (truncated, next) {
                (true, Some(n)) if !n.is_empty() && token.as_deref() != Some(n.as_str()) => {
                    token = Some(n)
                }
                (true, _) => {
                    return Err(io_other(format!(
                        "S3 {what}: truncated without a new continuation token"
                    )))
                }
                _ => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete(&self, key: &str) -> std::io::Result<()> {
        let okey = self.object_key(key)?;
        self.run(self.delete_object(&okey))
    }

    /// Abort the multipart uploads under `prefix` begun before
    /// `older_than` (a crash or an outage mid-upload leaves them behind;
    /// they are billed and invisible to a listing).
    fn sweep_incomplete(&self, prefix: &str, older_than: SystemTime) -> std::io::Result<usize> {
        let full = if self.url.prefix.is_empty() {
            prefix.to_string()
        } else {
            format!("{}/{prefix}", self.url.prefix)
        };
        let uploads = self.run(self.list_uploads(&full))?;
        let mut n = 0;
        for (key, id, initiated) in uploads {
            if initiated < older_than && !id.is_empty() {
                tracing::info!(key = %key, "backup: aborting a stale multipart upload");
                self.run(self.abort(&key, &id))?;
                n += 1;
            }
        }
        Ok(n)
    }

    fn describe(&self) -> String {
        format!("{} via {}", self.url, self.endpoint)
    }

    fn url_of(&self, key: &str) -> String {
        format!("{}/{key}", self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_urls_parse() {
        assert_eq!(
            parse_s3_url("s3://my-bucket/a/b/").unwrap(),
            S3Url {
                bucket: "my-bucket".into(),
                prefix: "a/b".into()
            }
        );
        assert_eq!(parse_s3_url("s3://bkt").unwrap().prefix, "");
        assert_eq!(parse_s3_url("s3://bkt/").unwrap().prefix, "");
        for bad in [
            "s3://",
            "s3://Bad",
            "s3://ab",
            "s3://-ab/x",
            "s3://bkt//x",
            "s3://bkt/a/../b",
            "s3://bkt/a\\b",
            "file:///x",
        ] {
            assert!(parse_s3_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn endpoints_parse_and_https_is_refused() {
        let e = parse_endpoint("http://minio:9000/").unwrap();
        assert_eq!((e.host.as_str(), e.port), ("minio", 9000));
        assert_eq!(parse_endpoint("HTTP://h").unwrap().port, 80);
        let v6 = parse_endpoint("http://[::1]:9000").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("[::1]", 9000));
        let https = parse_endpoint("https://s3.amazonaws.com").unwrap_err();
        assert!(
            https.contains("sidecar") && https.contains("#104"),
            "{https}"
        );
        for bad in [
            "minio:9000",
            "http://",
            "http://h:0",
            "http://h:x",
            "http://h/p",
            "http://u@h",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad}");
        }
        // No endpoint at all: AWS itself, which is HTTPS.
        let opts = S3Options {
            credentials_from_env: false,
            ..Default::default()
        };
        let e = S3Sink::new("s3://bkt/p", opts).unwrap_err();
        assert!(e.contains("--backup-endpoint") && e.contains("#104"), "{e}");
    }

    #[test]
    fn error_kinds_by_code() {
        use std::io::ErrorKind::*;
        for code in RETRYABLE_CODES {
            assert_eq!(error_kind(400, Some(code)), Other, "{code}");
        }
        assert_eq!(
            error_kind(403, Some("RequestTimeTooSkewed")),
            PermissionDenied
        );
        assert_eq!(error_kind(403, Some("AccessDenied")), PermissionDenied);
        for (st, code) in [
            (400, "InvalidArgument"),
            (400, "InvalidRequest"),
            (405, "MethodNotAllowed"),
            (411, "MissingContentLength"),
            (413, "EntityTooLarge"),
        ] {
            assert_eq!(error_kind(st, Some(code)), InvalidInput, "{code}");
        }
        assert_eq!(error_kind(404, Some("NoSuchKey")), NotFound);
        assert_eq!(error_kind(503, None), Other);
    }

    #[test]
    fn xml_errors_parse() {
        let body = b"<?xml version=\"1.0\"?><Error><Code>RequestTimeTooSkewed</Code>\
            <Message>The difference &amp; more</Message></Error>";
        assert_eq!(
            s3_error_text(body).unwrap(),
            "RequestTimeTooSkewed: The difference & more"
        );
        assert_eq!(s3_error_text(b"<ok/>"), None);
    }

    proptest::proptest! {
        /// Any valid bucket and prefix round-trip through the URL.
        #[test]
        fn s3_url_round_trips(
            bucket in "[a-z0-9][a-z0-9.-]{1,20}[a-z0-9]",
            segs in proptest::collection::vec("[A-Za-z0-9_ ~+=-]{1,8}", 0..4),
        ) {
            let prefix = segs.join("/");
            let url = if prefix.is_empty() { format!("s3://{bucket}") } else { format!("s3://{bucket}/{prefix}") };
            let u = parse_s3_url(&url).unwrap();
            proptest::prop_assert_eq!(&u.bucket, &bucket);
            proptest::prop_assert_eq!(&u.prefix, &prefix);
            proptest::prop_assert_eq!(u.to_string(), url);
        }

        /// Parsing never panics, and whatever parses has a valid bucket.
        #[test]
        fn s3_url_parsing_is_total(s in "s3://\\PC{0,30}") {
            if let Ok(u) = parse_s3_url(&s) {
                proptest::prop_assert!(valid_bucket(&u.bucket));
                proptest::prop_assert!(u.prefix.is_empty() || valid_key(&u.prefix));
            }
        }

        #[test]
        fn endpoint_parsing_is_total(s in "(https?://)?\\PC{0,30}") {
            if let Ok(e) = parse_endpoint(&s) {
                proptest::prop_assert!(!e.host.is_empty() && e.port != 0);
            }
        }
    }
}
