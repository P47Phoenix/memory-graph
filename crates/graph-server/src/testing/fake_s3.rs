//! [`FakeS3`]: an in-memory S3 over plain HTTP (hyper) for tests (ADR 0006
//! testing plan). It checks every request's SigV4 signature against a
//! fixed key ([`FAKE_KEY_ID`] / [`FAKE_SECRET`]) and the body against
//! `x-amz-content-sha256`, and speaks the subset the [`S3Sink`] uses: PUT,
//! GET, HEAD, DELETE, ListObjectsV2 (paged by [`FakeS3::set_page_size`]),
//! and multipart (create, upload part, complete, abort). Both path-style
//! and virtual-hosted addressing (a `Host` of `<bucket>.<name>`).
//!
//! Fault injection ([`Faults`]): drop the connection after N body bytes,
//! 500 on part k, a wrong ETag on every part, 403 on everything, a delay
//! before every response, and 500 on puts of keys with a given suffix.
use crate::backup::creds::Credentials;
use crate::backup::s3::{parse_endpoint, xml_texts, S3Options, S3Sink, S3Url};
use crate::backup::sigv4;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// The access key id [`FakeS3`] accepts.
pub const FAKE_KEY_ID: &str = "AKIDFAKES3EXAMPLE";
/// Its secret.
pub const FAKE_SECRET: &str = "fake/s3+secret-KEY-for-tests";
/// The region it signs for.
pub const FAKE_REGION: &str = "us-east-1";

/// What goes wrong (all off by default; each stays set until changed).
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Close the connection after reading this many bytes of a PUT body
    /// (an object or a part).
    pub drop_after_bytes: Option<u64>,
    /// Answer 500 to `UploadPart` number k.
    pub fail_part: Option<u32>,
    /// Hand out a wrong ETag for every part (so the complete fails).
    pub wrong_etag: bool,
    /// Answer 403 `AccessDenied` to everything.
    pub forbidden: bool,
    /// Sleep this long before every response.
    pub delay: Option<Duration>,
    /// Answer 500 to a PUT of any key ending with this.
    pub fail_puts_ending: Option<String>,
}

struct Obj {
    data: Bytes,
    modified: SystemTime,
}

struct Upload {
    key: String,
    initiated: SystemTime,
    parts: BTreeMap<u32, (Bytes, String)>,
}

#[derive(Default)]
struct State {
    /// `bucket/key` -> object.
    objects: Mutex<BTreeMap<String, Obj>>,
    uploads: Mutex<HashMap<String, Upload>>,
    faults: Mutex<Faults>,
    /// `METHOD path?query` of every request, in arrival order.
    log: Mutex<Vec<String>>,
    page_size: AtomicUsize,
    next_id: AtomicU64,
    /// TCP connections accepted.
    connections: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The in-process fake; stops when dropped.
pub struct FakeS3 {
    addr: SocketAddr,
    state: Arc<State>,
    rt: Option<tokio::runtime::Runtime>,
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

type Resp = hyper::Response<Full<Bytes>>;

fn resp(status: u16, body: impl Into<Bytes>) -> Resp {
    hyper::Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .expect("a response")
}

fn error(status: u16, code: &str, msg: &str) -> Resp {
    resp(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{}</Message></Error>",
            quick_xml::escape::escape(msg)
        ),
    )
}

fn etag_of(data: &[u8]) -> String {
    format!("\"{}\"", &sigv4::sha256_hex(data)[..32])
}

impl FakeS3 {
    /// Start on `127.0.0.1:0`.
    pub fn start() -> FakeS3 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fake-s3")
            .enable_all()
            .build()
            .expect("fake s3 runtime");
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("fake s3 binds");
        let addr = listener.local_addr().expect("an address");
        let state = Arc::new(State {
            page_size: AtomicUsize::new(1000),
            ..Default::default()
        });
        let st = Arc::clone(&state);
        rt.spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                st.connections.fetch_add(1, Ordering::SeqCst);
                let st = Arc::clone(&st);
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(move |req| {
                        let st = Arc::clone(&st);
                        async move { handle(&st, req).await }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        FakeS3 {
            addr,
            state,
            rt: Some(rt),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Options for a sink on this fake (no environment credentials).
    pub fn options(&self) -> S3Options {
        S3Options {
            endpoint: Some(self.endpoint()),
            region: FAKE_REGION.into(),
            credentials_from_env: false,
            timeout: Duration::from_secs(10),
            ..Default::default()
        }
    }

    /// A sink on `s3://<url>` here with the right key and `opts`.
    pub fn sink_with(&self, url: &str, opts: S3Options) -> S3Sink {
        let u: S3Url = crate::backup::s3::parse_s3_url(url).expect("an s3:// URL");
        let ep = parse_endpoint(opts.endpoint.as_deref().unwrap_or(&self.endpoint()))
            .expect("an endpoint");
        S3Sink::with_credentials(u, ep, opts, Credentials::new(FAKE_KEY_ID, FAKE_SECRET))
            .expect("a sink")
    }

    /// A sink on `s3://<url>` here with the default options.
    pub fn sink(&self, url: &str) -> S3Sink {
        self.sink_with(url, self.options())
    }

    /// A credentials file with the fake's key under `[default]`.
    pub fn write_credentials(&self, path: &std::path::Path) {
        std::fs::write(
            path,
            format!(
                "[default]\naws_access_key_id = {FAKE_KEY_ID}\naws_secret_access_key = {FAKE_SECRET}\n"
            ),
        )
        .expect("write the credentials file");
    }

    pub fn set_faults(&self, f: Faults) {
        *lock(&self.state.faults) = f;
    }

    pub fn clear_faults(&self) {
        self.set_faults(Faults::default());
    }

    /// ListObjectsV2 pages hold at most `n` keys.
    pub fn set_page_size(&self, n: usize) {
        self.state.page_size.store(n.max(1), Ordering::SeqCst);
    }

    /// `METHOD path?query` of every request so far.
    pub fn requests(&self) -> Vec<String> {
        lock(&self.state.log).clone()
    }

    /// TCP connections accepted so far.
    pub fn connections(&self) -> u64 {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// Multipart uploads started and neither completed nor aborted.
    pub fn pending_uploads(&self) -> usize {
        lock(&self.state.uploads).len()
    }

    /// Every key of `bucket`.
    pub fn keys(&self, bucket: &str) -> Vec<String> {
        let p = format!("{bucket}/");
        lock(&self.state.objects)
            .keys()
            .filter_map(|k| k.strip_prefix(&p).map(str::to_string))
            .collect()
    }

    pub fn object(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        lock(&self.state.objects)
            .get(&format!("{bucket}/{key}"))
            .map(|o| o.data.to_vec())
    }

    pub fn put_object(&self, bucket: &str, key: &str, data: &[u8]) {
        lock(&self.state.objects).insert(
            format!("{bucket}/{key}"),
            Obj {
                data: Bytes::copy_from_slice(data),
                modified: SystemTime::now(),
            },
        );
    }

    /// Flip one bit in the middle of an object; whether it existed.
    pub fn corrupt(&self, bucket: &str, key: &str) -> bool {
        let mut o = lock(&self.state.objects);
        let Some(obj) = o.get_mut(&format!("{bucket}/{key}")) else {
            return false;
        };
        let mut v = obj.data.to_vec();
        let mid = v.len() / 2;
        v[mid] ^= 1;
        obj.data = Bytes::from(v);
        true
    }

    /// Replace one object's bytes (e.g. with another's).
    pub fn replace(&self, bucket: &str, key: &str, data: Vec<u8>) {
        if let Some(o) = lock(&self.state.objects).get_mut(&format!("{bucket}/{key}")) {
            o.data = Bytes::from(data);
        }
    }

    /// Start a multipart upload directly (as a crashed client would have),
    /// begun at `initiated`; its upload id.
    pub fn begin_upload(&self, bucket: &str, key: &str, initiated: SystemTime) -> String {
        let id = format!("up-{}", self.state.next_id.fetch_add(1, Ordering::SeqCst));
        lock(&self.state.uploads).insert(
            id.clone(),
            Upload {
                key: format!("{bucket}/{key}"),
                initiated,
                parts: BTreeMap::new(),
            },
        );
        id
    }

    /// The keys (`bucket/key`) of the uploads in progress.
    pub fn upload_keys(&self) -> Vec<String> {
        let mut v: Vec<String> = lock(&self.state.uploads)
            .values()
            .map(|u| u.key.clone())
            .collect();
        v.sort();
        v
    }

    /// Back-date an object (the orphan sweep's age).
    pub fn set_modified(&self, bucket: &str, key: &str, t: SystemTime) {
        if let Some(o) = lock(&self.state.objects).get_mut(&format!("{bucket}/{key}")) {
            o.modified = t;
        }
    }
}

/// The request's bucket and (decoded) key, by addressing style.
fn address(host: &str, path: &str) -> Option<(String, String)> {
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?
        .into_owned();
    let path_style =
        name == "localhost" || name.parse::<std::net::IpAddr>().is_ok() || name.starts_with('[');
    if path_style {
        let rest = decoded.strip_prefix('/')?;
        let (b, k) = rest.split_once('/').unwrap_or((rest, ""));
        Some((b.to_string(), k.to_string()))
    } else {
        let (b, _) = name.split_once('.')?;
        Some((b.to_string(), decoded.strip_prefix('/')?.to_string()))
    }
}

fn query_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let d = |s: &str| {
                percent_encoding::percent_decode_str(s)
                    .decode_utf8_lossy()
                    .into_owned()
            };
            (d(k), d(v))
        })
        .collect()
}

/// Check the SigV4 `Authorization` of a request; the S3 error if wrong.
fn verify_signature(
    method: &str,
    path: &str,
    query: &[(String, String)],
    headers: &hyper::HeaderMap,
) -> Result<(), Box<Resp>> {
    let denied = |m: &str| Err(Box::new(error(403, "SignatureDoesNotMatch", m)));
    let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return Err(Box::new(error(
            403,
            "AccessDenied",
            "no Authorization header",
        )));
    };
    let Some(rest) = auth.strip_prefix("AWS4-HMAC-SHA256 ") else {
        return denied("not AWS4-HMAC-SHA256");
    };
    let field = |name: &str| {
        rest.split(',')
            .map(str::trim)
            .find_map(|p| p.strip_prefix(name))
            .map(str::to_string)
    };
    let (Some(cred), Some(signed)) = (field("Credential="), field("SignedHeaders=")) else {
        return denied("malformed Authorization");
    };
    let parts: Vec<&str> = cred.split('/').collect();
    if parts.len() != 5 || parts[0] != FAKE_KEY_ID {
        return Err(Box::new(error(
            403,
            "InvalidAccessKeyId",
            "unknown access key",
        )));
    }
    if parts[2] != FAKE_REGION || parts[3] != "s3" || parts[4] != "aws4_request" {
        return denied("wrong scope");
    }
    let Some(date) = headers.get("x-amz-date").and_then(|v| v.to_str().ok()) else {
        return denied("no x-amz-date");
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let parsed = sigv4::parse_iso8601(&format!(
        "{}-{}-{}T{}:{}:{}Z",
        date.get(0..4).unwrap_or(""),
        date.get(4..6).unwrap_or(""),
        date.get(6..8).unwrap_or(""),
        date.get(9..11).unwrap_or(""),
        date.get(11..13).unwrap_or(""),
        date.get(13..15).unwrap_or("")
    ));
    match parsed {
        Some(t) if t.abs_diff(now) <= 15 * 60 => {}
        _ => {
            return Err(Box::new(error(
                403,
                "RequestTimeTooSkewed",
                "The difference between the request time and the current time is too large.",
            )))
        }
    }
    if !signed.split(';').any(|h| h == "host") {
        return denied("host is not signed");
    }
    let mut hs = Vec::new();
    for h in signed.split(';') {
        for v in headers.get_all(h) {
            hs.push((h.to_string(), v.to_str().unwrap_or("").to_string()));
        }
    }
    let payload = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let want = sigv4::authorization(
        &sigv4::Request {
            method,
            path,
            query,
            headers: &hs,
            payload_sha256: payload,
        },
        &sigv4::Scope {
            amz_date: date,
            region: FAKE_REGION,
            service: "s3",
        },
        FAKE_KEY_ID,
        FAKE_SECRET,
    );
    if want != auth {
        return denied(
            "The request signature we calculated does not match the signature you provided.",
        );
    }
    Ok(())
}

async fn handle(st: &State, req: hyper::Request<Incoming>) -> Result<Resp, std::io::Error> {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let raw_query = req.uri().query().unwrap_or("").to_string();
    lock(&st.log).push(if raw_query.is_empty() {
        format!("{method} {path}")
    } else {
        format!("{method} {path}?{raw_query}")
    });
    let faults = lock(&st.faults).clone();
    let headers = req.headers().clone();
    // Read the body (dropping the connection part-way if asked to).
    let mut body = req.into_body();
    let mut data = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(std::io::Error::other)?;
        if let Ok(d) = frame.into_data() {
            data.extend_from_slice(&d);
        }
        if method == "PUT"
            && faults
                .drop_after_bytes
                .is_some_and(|n| data.len() as u64 >= n)
        {
            return Err(std::io::Error::other("fake s3: dropping the connection"));
        }
    }
    if let Some(d) = faults.delay {
        tokio::time::sleep(d).await;
    }
    if faults.forbidden {
        return Ok(error(403, "AccessDenied", "Access Denied"));
    }
    let query = query_pairs(&raw_query);
    if let Err(r) = verify_signature(&method, &path, &query, &headers) {
        return Ok(*r);
    }
    let sent = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if sent != "UNSIGNED-PAYLOAD" && sent != sigv4::sha256_hex(&data) {
        return Ok(error(
            400,
            "XAmzContentSHA256Mismatch",
            "The provided 'x-amz-content-sha256' header does not match what was computed.",
        ));
    }
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some((bucket, key)) = address(host, &path) else {
        return Ok(error(400, "InvalidRequest", "cannot address the request"));
    };
    let q = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let full = format!("{bucket}/{key}");
    let data = Bytes::from(data);
    Ok(match (method.as_str(), key.is_empty()) {
        ("GET", true) if q("list-type").as_deref() == Some("2") => list(
            st,
            &bucket,
            &q("prefix").unwrap_or_default(),
            q("continuation-token"),
        ),
        ("GET", true) if q("uploads").is_some() => {
            list_uploads(st, &bucket, &q("prefix").unwrap_or_default())
        }
        ("PUT", false) => {
            if let (Some(id), Some(n)) = (q("uploadId"), q("partNumber")) {
                let n: u32 = n.parse().unwrap_or(0);
                if faults.fail_part == Some(n) {
                    return Ok(error(
                        500,
                        "InternalError",
                        "We encountered an internal error.",
                    ));
                }
                let mut ups = lock(&st.uploads);
                let Some(up) = ups.get_mut(&id) else {
                    return Ok(error(404, "NoSuchUpload", "no such upload"));
                };
                let real = etag_of(&data);
                up.parts.insert(n, (data, real.clone()));
                let etag = if faults.wrong_etag {
                    "\"00000000000000000000000000000000\"".to_string()
                } else {
                    real
                };
                let mut r = resp(200, "");
                r.headers_mut()
                    .insert("etag", etag.parse().expect("an etag"));
                r
            } else {
                if faults
                    .fail_puts_ending
                    .as_deref()
                    .is_some_and(|s| key.ends_with(s))
                {
                    return Ok(error(
                        500,
                        "InternalError",
                        "We encountered an internal error.",
                    ));
                }
                let etag = etag_of(&data);
                lock(&st.objects).insert(
                    full,
                    Obj {
                        data,
                        modified: SystemTime::now(),
                    },
                );
                let mut r = resp(200, "");
                r.headers_mut()
                    .insert("etag", etag.parse().expect("an etag"));
                r
            }
        }
        ("POST", false) if q("uploads").is_some() => {
            let id = format!("up-{}", st.next_id.fetch_add(1, Ordering::SeqCst));
            lock(&st.uploads).insert(
                id.clone(),
                Upload {
                    key: full,
                    initiated: SystemTime::now(),
                    parts: BTreeMap::new(),
                },
            );
            resp(
                200,
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><InitiateMultipartUploadResult><Bucket>{bucket}</Bucket><Key>{}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>",
                    quick_xml::escape::escape(&key)
                ),
            )
        }
        ("POST", false) if q("uploadId").is_some() => {
            let id = q("uploadId").unwrap_or_default();
            let Ok(t) = xml_texts(&data) else {
                return Ok(error(400, "MalformedXML", "bad XML"));
            };
            let nums: Vec<u32> = t
                .iter()
                .filter(|(k, _)| k == "CompleteMultipartUpload/Part/PartNumber")
                .map(|(_, v)| v.parse().unwrap_or(0))
                .collect();
            let tags: Vec<String> = t
                .iter()
                .filter(|(k, _)| k == "CompleteMultipartUpload/Part/ETag")
                .map(|(_, v)| v.clone())
                .collect();
            let mut ups = lock(&st.uploads);
            let Some(up) = ups.get(&id) else {
                return Ok(error(404, "NoSuchUpload", "no such upload"));
            };
            if up.key != full || nums.is_empty() || nums.len() != tags.len() {
                return Ok(error(400, "InvalidPart", "parts do not match"));
            }
            let mut out = Vec::new();
            for (n, tag) in nums.iter().zip(&tags) {
                match up.parts.get(n) {
                    Some((d, e)) if e == tag => out.extend_from_slice(d),
                    _ => {
                        return Ok(error(
                            400,
                            "InvalidPart",
                            "One or more of the specified parts could not be found or the \
                             specified entity tag might not have matched the part's entity tag.",
                        ))
                    }
                }
            }
            ups.remove(&id);
            lock(&st.objects).insert(
                full,
                Obj {
                    data: Bytes::from(out),
                    modified: SystemTime::now(),
                },
            );
            resp(
                200,
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUploadResult/>",
            )
        }
        ("DELETE", false) if q("uploadId").is_some() => {
            match lock(&st.uploads).remove(&q("uploadId").unwrap_or_default()) {
                Some(_) => resp(204, ""),
                None => error(404, "NoSuchUpload", "no such upload"),
            }
        }
        ("DELETE", false) => {
            lock(&st.objects).remove(&full);
            resp(204, "")
        }
        ("GET", false) => match lock(&st.objects).get(&full) {
            Some(o) => resp(200, o.data.clone()),
            None => error(404, "NoSuchKey", "The specified key does not exist."),
        },
        ("HEAD", false) => match lock(&st.objects).get(&full) {
            Some(o) => {
                let mut r = resp(200, "");
                r.headers_mut()
                    .insert("content-length", o.data.len().into());
                r
            }
            None => resp(404, ""),
        },
        _ => error(400, "InvalidRequest", "not supported by FakeS3"),
    })
}

fn list(st: &State, bucket: &str, prefix: &str, after: Option<String>) -> Resp {
    let page = st.page_size.load(Ordering::SeqCst);
    let b = format!("{bucket}/");
    let objects = lock(&st.objects);
    let mut items = objects
        .iter()
        .filter_map(|(k, o)| k.strip_prefix(&b).map(|k| (k, o)))
        .filter(|(k, _)| k.starts_with(prefix))
        .filter(|(k, _)| after.as_deref().is_none_or(|a| *k > a));
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    let mut last = None;
    let mut n = 0;
    for (k, o) in items.by_ref().take(page) {
        let secs = o
            .modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        xml.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{}</LastModified><Size>{}</Size></Contents>",
            quick_xml::escape::escape(k),
            sigv4::iso8601(secs),
            o.data.len()
        ));
        last = Some(k.to_string());
        n += 1;
    }
    let more = items.next().is_some();
    xml.push_str(&format!(
        "<KeyCount>{n}</KeyCount><IsTruncated>{more}</IsTruncated>"
    ));
    if more {
        xml.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            quick_xml::escape::escape(last.unwrap_or_default())
        ));
    }
    xml.push_str("</ListBucketResult>");
    resp(200, xml)
}

/// ListMultipartUploads (one page: the fake never truncates it).
fn list_uploads(st: &State, bucket: &str, prefix: &str) -> Resp {
    let b = format!("{bucket}/");
    let ups = lock(&st.uploads);
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListMultipartUploadsResult>");
    for (id, u) in ups.iter() {
        let Some(k) = u.key.strip_prefix(&b) else {
            continue;
        };
        if !k.starts_with(prefix) {
            continue;
        }
        let secs = u
            .initiated
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        xml.push_str(&format!(
            "<Upload><Key>{}</Key><UploadId>{id}</UploadId><Initiated>{}</Initiated></Upload>",
            quick_xml::escape::escape(k),
            sigv4::iso8601(secs)
        ));
    }
    xml.push_str("<IsTruncated>false</IsTruncated></ListMultipartUploadsResult>");
    resp(200, xml)
}
