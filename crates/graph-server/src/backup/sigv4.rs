//! AWS Signature Version 4 (ADR 0006 E4), on `sha2` (RustCrypto, pure
//! Rust). HMAC-SHA256 is the dozen lines of RFC 2104 below rather than the
//! `hmac` crate the ADR names: `hmac` pulls in `subtle`, which is
//! BSD-3-Clause, and story 36 requires every new crate to be MIT or Apache.
//! Constant-time comparison (`subtle`'s purpose) is not needed here: the
//! client only computes signatures, it never verifies one. Only what the S3 sink needs: header-based signing of one
//! request whose payload hash the caller gives (`x-amz-content-sha256`).
//!
//! The canonical request follows the AWS rules for S3: the path is
//! URI-encoded once (every byte but the unreserved set and `/`), the query
//! parameters are encoded and sorted, header names are lowercased, values
//! trimmed with inner runs of spaces collapsed, and duplicates joined by
//! commas.
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use sha2::{Digest, Sha256};

use crate::raft::snapshot_dir::hex;

/// Every byte but the unreserved set (`A-Z a-z 0-9 - _ . ~`) is encoded.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');
/// The same, keeping `/` (an S3 object key's path).
const PATH: &AsciiSet = &QUERY.remove(b'/');

/// The hash of an empty payload.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// URI-encode one query key or value (AWS rules: `%XX` uppercase, space as
/// `%20`).
pub fn encode_query(s: &str) -> String {
    utf8_percent_encode(s, QUERY).to_string()
}

/// URI-encode a path, keeping `/`.
pub fn encode_path(s: &str) -> String {
    utf8_percent_encode(s, PATH).to_string()
}

/// Lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// HMAC-SHA256 (RFC 2104; SHA-256's block is 64 bytes).
pub fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

/// `kSigning` for (secret, `YYYYMMDD`, region, service).
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// `YYYYMMDDTHHMMSSZ` of a Unix time (UTC).
pub fn amz_date(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    let s = unix % 86_400;
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        s / 3600,
        (s / 60) % 60,
        s % 60
    )
}

/// ISO 8601 (`2009-10-12T17:50:30.000Z`) of a Unix time, as S3 lists it.
pub fn iso8601(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    let s = unix % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        s / 3600,
        (s / 60) % 60,
        s % 60
    )
}

/// Parse S3's `LastModified` (`2009-10-12T17:50:30.000Z`, fraction and `Z`
/// optional) to Unix seconds.
pub fn parse_iso8601(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + se).ok()
}

// Howard Hinnant's civil-date algorithms (proleptic Gregorian, UTC).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One request to sign.
pub struct Request<'a> {
    pub method: &'a str,
    /// The canonical URI: already encoded (see [`encode_path`]).
    pub path: &'a str,
    /// Raw (not encoded) query parameters, in any order.
    pub query: &'a [(String, String)],
    /// Every header to sign (at least `host` and `x-amz-date`), raw.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the payload (or `UNSIGNED-PAYLOAD`).
    pub payload_sha256: &'a str,
}

/// The signing scope's parts.
pub struct Scope<'a> {
    /// `YYYYMMDDTHHMMSSZ`, as sent in `x-amz-date`.
    pub amz_date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

/// The canonical query string.
pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (encode_query(k), encode_query(v)))
        .collect();
    q.sort();
    q.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// (canonical headers, signed headers).
fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut map: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (k, v) in headers {
        let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
        map.entry(k.trim().to_ascii_lowercase())
            .or_default()
            .push(v);
    }
    let canon = map
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.join(",")))
        .collect::<String>();
    let signed = map.keys().cloned().collect::<Vec<_>>().join(";");
    (canon, signed)
}

/// The canonical request (for diagnostics and tests).
pub fn canonical_request(r: &Request<'_>) -> String {
    let (canon, signed) = canonical_headers(r.headers);
    format!(
        "{}\n{}\n{}\n{canon}\n{signed}\n{}",
        r.method,
        r.path,
        canonical_query(r.query),
        r.payload_sha256
    )
}

/// The `Authorization` header value.
pub fn authorization(r: &Request<'_>, s: &Scope<'_>, access_key: &str, secret: &str) -> String {
    let (_, signed) = canonical_headers(r.headers);
    let date = &s.amz_date[..8];
    let scope = format!("{date}/{}/{}/aws4_request", s.region, s.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        s.amz_date,
        sha256_hex(canonical_request(r).as_bytes())
    );
    let key = signing_key(secret, date, s.region, s.service);
    let sig = hex(&hmac(&key, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={sig}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The AWS SigV4 test suite (`aws-sig-v4-test-suite`, credentials
    /// AKIDEXAMPLE, 2015-08-30T12:36:00Z, us-east-1, service `service`).
    #[test]
    fn aws_sigv4_test_suite_vectors() {
        const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let scope = Scope {
            amz_date: "20150830T123600Z",
            region: "us-east-1",
            service: "service",
        };
        let base = h(&[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ]);
        // (name, method, query, headers, signature)
        type Case<'a> = (
            &'a str,
            &'a str,
            Vec<(String, String)>,
            Vec<(String, String)>,
            &'a str,
        );
        let cases: &[Case<'_>] = &[
            (
                "get-vanilla",
                "GET",
                vec![],
                base.clone(),
                "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            ),
            (
                "get-vanilla-query-order-key-case",
                "GET",
                h(&[("Param2", "value2"), ("Param1", "value1")]),
                base.clone(),
                "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
            ),
            (
                "post-vanilla",
                "POST",
                vec![],
                base.clone(),
                "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
            ),
        ];
        for (name, method, query, headers, want) in cases {
            let r = Request {
                method,
                path: "/",
                query,
                headers,
                payload_sha256: EMPTY_SHA256,
            };
            let auth = authorization(&r, &scope, "AKIDEXAMPLE", SECRET);
            assert!(
                auth.ends_with(&format!("Signature={want}")),
                "{name}: {auth}\n{}",
                canonical_request(&r)
            );
            assert!(auth.contains(
                "Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date,"
            ));
        }
    }

    /// RFC 4231 test cases 1, 2 and 6 (a key longer than the block).
    #[test]
    fn hmac_sha256_rfc4231() {
        let cases: [(&[u8], &[u8], &str); 3] = [
            (
                &[0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (key, data, want) in cases {
            assert_eq!(hex(&hmac(key, data)), want);
        }
    }

    /// The signing-key example of the AWS docs.
    #[test]
    fn signing_key_example() {
        let k = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&k),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    /// The S3 examples of the AWS docs ("Signature calculations for the
    /// Authorization header": examplebucket, 2013-05-24).
    #[test]
    fn s3_documentation_examples() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let scope = Scope {
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            service: "s3",
        };
        let host = "examplebucket.s3.amazonaws.com";
        // GET Object with a Range.
        let headers = h(&[
            ("Host", host),
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let r = Request {
            method: "GET",
            path: "/test.txt",
            query: &[],
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        assert!(authorization(&r, &scope, KEY, SECRET).ends_with(
            "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        ));
        // PUT Object.
        let body = sha256_hex(b"Welcome to Amazon S3.");
        let headers = h(&[
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("Host", host),
            ("x-amz-content-sha256", &body),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ]);
        let r = Request {
            method: "PUT",
            path: &encode_path("/test$file.text"),
            query: &[],
            headers: &headers,
            payload_sha256: &body,
        };
        assert!(authorization(&r, &scope, KEY, SECRET).ends_with(
            "Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        ));
        // GET Bucket lifecycle.
        let headers = h(&[
            ("Host", host),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let q = h(&[("lifecycle", "")]);
        let r = Request {
            method: "GET",
            path: "/",
            query: &q,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        assert!(authorization(&r, &scope, KEY, SECRET).ends_with(
            "Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        ));
        // GET Bucket (list objects).
        let q = h(&[("max-keys", "2"), ("prefix", "J")]);
        let r = Request {
            method: "GET",
            path: "/",
            query: &q,
            headers: &headers,
            payload_sha256: EMPTY_SHA256,
        };
        assert!(authorization(&r, &scope, KEY, SECRET).ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            parse_iso8601("2015-08-30T12:36:00.000Z"),
            Some(1_440_938_160)
        );
        assert_eq!(parse_iso8601("2015-08-30T12:36:00Z"), Some(1_440_938_160));
        assert_eq!(parse_iso8601("garbage"), None);
        for t in [0u64, 951_782_400, 1_709_164_800, 4_102_444_799] {
            assert_eq!(parse_iso8601(&iso8601(t)), Some(t), "{t}");
        }
    }

    #[test]
    fn canonical_headers_trim_and_join() {
        let hs = h(&[
            ("My-Header1", "  a   b   c "),
            ("my-header1", "d"),
            ("Host", "x"),
        ]);
        let (c, s) = canonical_headers(&hs);
        assert_eq!(c, "host:x\nmy-header1:a b c,d\n");
        assert_eq!(s, "host;my-header1");
    }

    proptest::proptest! {
        /// Encoding keeps only the unreserved set (and `/` in a path), and
        /// decodes back to the input.
        #[test]
        fn encoding_is_reversible_and_safe(s in "\\PC{0,40}") {
            for (enc, slash) in [(encode_query(&s), false), (encode_path(&s), true)] {
                proptest::prop_assert!(enc.bytes().all(|b| b.is_ascii_alphanumeric()
                    || b"-_.~%".contains(&b) || (slash && b == b'/')), "{}", enc);
                let back = percent_encoding::percent_decode_str(&enc).decode_utf8().unwrap();
                proptest::prop_assert_eq!(back.as_ref(), s.as_str());
            }
        }

        #[test]
        fn dates_round_trip_any(t in 0u64..253_402_300_799) {
            proptest::prop_assert_eq!(parse_iso8601(&iso8601(t)), Some(t));
        }
    }
}
