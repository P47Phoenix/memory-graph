//! Source decoding (ADR 0007 C2-C5, C10): turns a file's raw bytes into the
//! UTF-8 text every extractor and the tokenizer see.
//!
//! Language-agnostic: nothing here knows about any programming language.
//! The detection order is fixed by ADR 0007 C3:
//!
//! 1. a BOM (UTF-8, UTF-16LE, UTF-16BE), decoded without stripping it, so
//!    U+FEFF stays at the start of the text; it wins over any hint;
//! 2. an explicit hint (already resolved by the client; `replacement` is
//!    never a hint, see [`hint_from_label`]);
//! 3. BOM-less UTF-16, sniffed from the NUL pattern, *before* the UTF-8
//!    check (ASCII-only UTF-16 is valid UTF-8 as bytes);
//!    Then (step 3b, amended 2026-09-30) 7-bit ISO-2022-JP, recognised by a
//!    JIS X 0208 designation, since it too is valid UTF-8 as bytes;
//! 4. valid UTF-8, borrowed unchanged;
//! 5. `chardetng`, restricted to the non-UTF-8 encodings it can return;
//! 6. windows-1252, which maps every byte.
//!
//! `lossy` is reachable only through steps 1 and 2.
//!
//! `chardetng` can guess wrong on short legacy files (a one-line
//! windows-1252 or Shift_JIS file may come out as another code page). No
//! length gate is applied, because it would misdecode short CJK files the
//! other way; `--encoding` or `.memory-graph.toml` is the remedy.

use std::borrow::Cow;

pub use encoding_rs::Encoding;
use encoding_rs::{
    BIG5, EUC_JP, EUC_KR, GB18030, GBK, IBM866, ISO_2022_JP, ISO_8859_10, ISO_8859_13, ISO_8859_14,
    ISO_8859_15, ISO_8859_16, ISO_8859_2, ISO_8859_3, ISO_8859_4, ISO_8859_5, ISO_8859_6,
    ISO_8859_7, ISO_8859_8, ISO_8859_8_I, KOI8_R, KOI8_U, REPLACEMENT, SHIFT_JIS, UTF_16BE,
    UTF_16LE, UTF_8, WINDOWS_1250, WINDOWS_1251, WINDOWS_1252, WINDOWS_1253, WINDOWS_1254,
    WINDOWS_1255, WINDOWS_1256, WINDOWS_1257, WINDOWS_1258, WINDOWS_874,
};

/// The decoder's behaviour version. Bumped with any `encoding_rs` or
/// `chardetng` bump and any change to the detection or binary rules; it is
/// part of the non-UTF-8 fingerprint suffix and the cluster's extractor hash.
pub const DECODER_VERSION: u32 = 1;

/// How many leading bytes the BOM-less UTF-16 sniff looks at.
pub const UTF16_SNIFF_WINDOW: usize = 4096;

/// A file's bytes decoded to UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded<'a> {
    /// The decoded text; borrowed when the input was valid UTF-8 as decoded.
    pub text: Cow<'a, str>,
    /// The encoding the text was decoded from.
    pub encoding: &'static Encoding,
    /// True when an invalid sequence was replaced with U+FFFD.
    pub lossy: bool,
}

/// Why a label cannot be used as an encoding hint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodingError {
    /// `encoding_rs` does not know the label.
    #[error("unknown encoding label `{0}` (examples: utf-8, utf-16le, utf-16be, windows-1252, latin1, shift_jis, gbk, gb18030, euc-kr, big5, iso-2022-jp, ansi)")]
    UnknownLabel(String),
    /// The label names the `replacement` encoding, which decodes everything
    /// to one U+FFFD.
    #[error(
        "encoding label `{0}` names the `replacement` encoding, which cannot be used as a hint"
    )]
    Replacement(String),
    /// `ansi` on Windows with a system code page that has no mapping.
    #[error("the system ANSI code page {0} has no supported encoding; pass --encoding explicitly")]
    UnmappedCodePage(u32),
}

/// Decodes `bytes` following the ADR 0007 C3 order. Never fails and never
/// panics: every byte sequence decodes to something.
///
/// A `replacement` hint is ignored (treated as no hint); callers resolve
/// labels with [`hint_from_label`], which refuses it.
pub fn decode<'a>(bytes: &'a [u8], hint: Option<&'static Encoding>) -> Decoded<'a> {
    // 1. BOM: believed over any hint, and kept as U+FEFF.
    if let Some((encoding, _bom_len)) = Encoding::for_bom(bytes) {
        return decode_with(encoding, bytes);
    }
    // 2. Explicit hint.
    if let Some(encoding) = hint.filter(|e| *e != REPLACEMENT) {
        return decode_with(encoding, bytes);
    }
    // 3. BOM-less UTF-16, before the UTF-8 check.
    if let Some(encoding) = sniff_utf16(bytes) {
        return decode_with(encoding, bytes);
    }
    // 3b. 7-bit ISO-2022-JP, by its escapes, before the UTF-8 check
    // (amended 2026-09-30); only a clean decode is accepted.
    if looks_like_iso_2022_jp(bytes) {
        if let Some(text) = ISO_2022_JP.decode_without_bom_handling_and_without_replacement(bytes) {
            return Decoded {
                text,
                encoding: ISO_2022_JP,
                lossy: false,
            };
        }
    }
    // 4. Valid UTF-8, borrowed.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Decoded {
            text: Cow::Borrowed(text),
            encoding: UTF_8,
            lossy: false,
        };
    }
    // 5. chardetng, restricted; only a clean decode is accepted.
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let guess = detector.guess(None, chardetng::Utf8Detection::Deny);
    if is_detectable(guess) {
        if let Some(text) = guess.decode_without_bom_handling_and_without_replacement(bytes) {
            return Decoded {
                text,
                encoding: guess,
                lossy: false,
            };
        }
    }
    // 6. windows-1252 maps every byte, so this is never lossy.
    decode_with(WINDOWS_1252, bytes)
}

/// True when `bytes` are all 7-bit and contain a JIS X 0208 designation
/// (ESC $ @ or ESC $ B). ESC ( B / ESC ( J alone prove nothing: they only
/// switch back to ASCII / JIS-Roman.
fn looks_like_iso_2022_jp(bytes: &[u8]) -> bool {
    bytes.is_ascii()
        && bytes
            .windows(3)
            .any(|w| matches!(w, [0x1B, b'$', b'@'] | [0x1B, b'$', b'B']))
}

fn decode_with<'a>(encoding: &'static Encoding, bytes: &'a [u8]) -> Decoded<'a> {
    let (text, lossy) = encoding.decode_without_bom_handling(bytes);
    Decoded {
        text,
        encoding,
        lossy,
    }
}

/// The encodings step 5 may accept from `chardetng` (everything it can
/// return besides UTF-8): the legacy single-byte encodings, Shift_JIS,
/// EUC-JP, ISO-2022-JP, EUC-KR, GBK/GB18030 and Big5.
const DETECTABLE: &[&Encoding] = &[
    WINDOWS_1250,
    WINDOWS_1251,
    WINDOWS_1252,
    WINDOWS_1253,
    WINDOWS_1254,
    WINDOWS_1255,
    WINDOWS_1256,
    WINDOWS_1257,
    WINDOWS_1258,
    WINDOWS_874,
    ISO_8859_2,
    ISO_8859_3,
    ISO_8859_4,
    ISO_8859_5,
    ISO_8859_6,
    ISO_8859_7,
    ISO_8859_8,
    ISO_8859_8_I,
    ISO_8859_10,
    ISO_8859_13,
    ISO_8859_14,
    ISO_8859_15,
    ISO_8859_16,
    KOI8_R,
    KOI8_U,
    IBM866,
    SHIFT_JIS,
    EUC_JP,
    ISO_2022_JP,
    EUC_KR,
    GBK,
    GB18030,
    BIG5,
];

fn is_detectable(encoding: &'static Encoding) -> bool {
    DETECTABLE.contains(&encoding)
}

/// The BOM-less UTF-16 sniff (ADR 0007 C3 step 3, amended 2026-09-30 for
/// CJK). Both byte orders are tried. In the first [`UTF16_SNIFF_WINDOW`]
/// bytes (trimmed to an even length), a candidate needs NULs in fewer than
/// 1/16 of its low-byte positions and, in its high-byte positions (even
/// offsets for BE, odd for LE), either NULs in at least 1/2 of the code units
/// (ASCII-heavy text, or short CJK whose code units happen to be valid UTF-8
/// bytes, like "-- 名前"), or, only when the whole input is *not* valid UTF-8
/// (CJK text), NULs in at least 1/64 of them and at least one. A UTF-8 file
/// with a stray NUL is valid UTF-8, so it stays binary; the whole file must then decode strictly as that
/// encoding, and the decoded text must have no C0 control other than tab,
/// LF, FF and CR (so no U+0000). ASCII-heavy UTF-16 has NULs in almost every
/// high byte; CJK UTF-16 has them only on its ASCII (newlines, spaces,
/// punctuation). If both orders pass, the one with more high-byte NULs wins
/// (LE on a tie).
pub fn sniff_utf16(bytes: &[u8]) -> Option<&'static Encoding> {
    let window = &bytes[..bytes.len().min(UTF16_SNIFF_WINDOW) & !1];
    if window.is_empty() {
        return None;
    }
    let units = window.len() / 2;
    let even_nuls = window.iter().step_by(2).filter(|b| **b == 0).count();
    let odd_nuls = window
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|b| **b == 0)
        .count();
    let valid_utf8 = std::str::from_utf8(bytes).is_ok();
    let candidate = |high: usize, low: usize, encoding: &'static Encoding| {
        let ascii_heavy = high * 2 >= units;
        let cjk = !valid_utf8 && high > 0 && high * 64 >= units;
        if !(ascii_heavy || cjk) || low * 16 >= units {
            return None;
        }
        let text = encoding.decode_without_bom_handling_and_without_replacement(bytes)?;
        let clean = !text
            .chars()
            .any(|c| c < ' ' && !matches!(c, '\t' | '\n' | '\x0C' | '\r'));
        clean.then_some((high, encoding))
    };
    let be = candidate(even_nuls, odd_nuls, UTF_16BE);
    let le = candidate(odd_nuls, even_nuls, UTF_16LE);
    match (le, be) {
        (Some(l), Some(b)) => Some(if b.0 > l.0 { b.1 } else { l.1 }),
        (l, b) => l.or(b).map(|(_, e)| e),
    }
}

/// True when the file should be skipped as binary (ADR 0007 C5): it has a
/// NUL byte, no BOM, and does not sniff as UTF-16. Because the sniff needs a
/// strict decode, a single invalid UTF-16 unit (an unpaired surrogate, or an
/// odd length) makes a BOM-less NUL-bearing file binary.
pub fn is_binary(bytes: &[u8]) -> bool {
    is_binary_with_hint(bytes, None)
}

/// [`is_binary`] for a file with a resolved hint: an explicit UTF-16 hint
/// also makes a file with NULs text. Any other hint changes nothing here,
/// but [`decode`] still uses it over the sniff.
pub fn is_binary_with_hint(bytes: &[u8], hint: Option<&'static Encoding>) -> bool {
    bytes.contains(&0)
        && Encoding::for_bom(bytes).is_none()
        && !matches!(hint, Some(e) if e == UTF_16LE || e == UTF_16BE)
        && sniff_utf16(bytes).is_none()
}

/// Resolves a concrete encoding label (any `encoding_rs` label, not `ansi`
/// or `auto`) to a hint, refusing `replacement` and unknown labels.
pub fn hint_from_label(label: &str) -> Result<&'static Encoding, EncodingError> {
    match Encoding::for_label(label.trim().as_bytes()) {
        Some(e) if e == REPLACEMENT => Err(EncodingError::Replacement(label.to_string())),
        Some(e) => Ok(e),
        None => Err(EncodingError::UnknownLabel(label.to_string())),
    }
}

/// Maps a Windows code page to its `encoding_rs` equivalent (ADR 0007 C4).
/// 65001 (UTF-8) and unmapped pages are `None`.
pub fn encoding_for_code_page(code_page: u32) -> Option<&'static Encoding> {
    Some(match code_page {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1252 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        _ => return None,
    })
}

/// Resolves `ansi` (ADR 0007 C4) on the client: the Windows system code page
/// (`GetACP`) on Windows, windows-1252 elsewhere. Never called by a server.
pub fn resolve_ansi() -> Result<&'static Encoding, EncodingError> {
    resolve_ansi_impl()
}

#[cfg(windows)]
fn resolve_ansi_impl() -> Result<&'static Encoding, EncodingError> {
    // SAFETY: GetACP takes no arguments and only reads process state.
    let code_page = unsafe { windows_sys::Win32::Globalization::GetACP() };
    encoding_for_code_page(code_page).ok_or(EncodingError::UnmappedCodePage(code_page))
}

#[cfg(not(windows))]
fn resolve_ansi_impl() -> Result<&'static Encoding, EncodingError> {
    Ok(WINDOWS_1252)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn utf16(text: &str, be: bool, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let units = bom
            .then_some(0xFEFFu16)
            .into_iter()
            .chain(text.encode_utf16());
        for u in units {
            out.extend_from_slice(&if be { u.to_be_bytes() } else { u.to_le_bytes() });
        }
        out
    }

    fn encode(encoding: &'static Encoding, text: &str) -> Vec<u8> {
        let (bytes, _, unmappable) = encoding.encode(text);
        assert!(
            !unmappable,
            "fixture not representable in {}",
            encoding.name()
        );
        bytes.into_owned()
    }

    const ASCII_SRC: &str = "public class CustomerId {\n    int Value;\n}\n";

    #[test]
    fn utf16_with_bom_keeps_feff_and_beats_hint() {
        for be in [false, true] {
            let bytes = utf16("class Café {}\n", be, true);
            for hint in [None, Some(SHIFT_JIS), Some(UTF_8)] {
                let d = decode(&bytes, hint);
                assert_eq!(d.encoding, if be { UTF_16BE } else { UTF_16LE });
                assert_eq!(d.text, "\u{FEFF}class Café {}\n");
                assert!(!d.lossy);
            }
        }
    }

    #[test]
    fn utf8_bom_is_kept_and_beats_hint() {
        let bytes = b"\xEF\xBB\xBFfn main() {}".to_vec();
        let d = decode(&bytes, Some(UTF_16LE));
        assert_eq!(d.encoding, UTF_8);
        assert_eq!(d.text, "\u{FEFF}fn main() {}");
        assert!(!d.lossy);
    }

    #[test]
    fn bom_only_files_are_exactly_feff() {
        for bom in [&b"\xEF\xBB\xBF"[..], b"\xFF\xFE", b"\xFE\xFF"] {
            let d = decode(bom, None);
            assert_eq!(d.text, "\u{FEFF}");
            assert!(!d.lossy);
            assert!(!is_binary(bom));
        }
        // A BOM makes NUL-bearing bytes that fail the sniff text, not binary.
        let bytes = b"\xEF\xBB\xBF\0\0\0\x07";
        assert_eq!(sniff_utf16(bytes), None);
        assert!(!is_binary(bytes));
    }

    #[test]
    fn a_disagreeing_hint_wins_over_the_sniff() {
        let bytes = utf16(ASCII_SRC, false, false);
        assert!(!is_binary_with_hint(&bytes, Some(WINDOWS_1252)));
        let d = decode(&bytes, Some(WINDOWS_1252));
        assert_eq!(d.encoding, WINDOWS_1252);
        assert!(d.text.contains('\0'));
    }

    #[test]
    fn short_legacy_files_can_be_misdetected() {
        // Pinned so a behaviour change is visible (ADR 0007 C9): a short
        // windows-1252 file is guessed as ISO-8859-4.
        let bytes = encode(WINDOWS_1252, "naïve = 1;\n");
        let d = decode(&bytes, None);
        assert_eq!(d.encoding, ISO_8859_4);
        assert!(!d.lossy);
    }

    #[test]
    fn bom_followed_by_invalid_bytes_is_lossy() {
        let d = decode(b"\xEF\xBB\xBFok \xFF\xFE!", None);
        assert_eq!(d.encoding, UTF_8);
        assert!(d.lossy);
        assert!(d.text.contains('\u{FFFD}'));
        // UTF-16LE BOM with an unpaired surrogate and an odd trailing byte.
        let d = decode(&[0xFF, 0xFE, b'a', 0, 0x00, 0xD8, b'b'], None);
        assert_eq!(d.encoding, UTF_16LE);
        assert!(d.lossy);
    }

    #[test]
    fn valid_utf8_is_borrowed_unchanged() {
        let src = "fn café() { let 日本 = 1; }\n";
        let d = decode(src.as_bytes(), None);
        assert!(matches!(d.text, Cow::Borrowed(s) if s.as_ptr() == src.as_ptr()));
        assert_eq!(d.text.as_bytes(), src.as_bytes());
        assert_eq!(d.encoding, UTF_8);
        assert!(!d.lossy);
        assert_eq!(decode(b"", None).encoding, UTF_8);
    }

    #[test]
    fn bomless_ascii_utf16_is_sniffed_before_utf8() {
        for be in [false, true] {
            let bytes = utf16(ASCII_SRC, be, false);
            assert!(
                std::str::from_utf8(&bytes).is_ok(),
                "fixture is valid UTF-8"
            );
            let d = decode(&bytes, None);
            assert_eq!(d.encoding, if be { UTF_16BE } else { UTF_16LE });
            assert_eq!(d.text, ASCII_SRC);
            assert!(!d.lossy);
        }
    }

    #[test]
    fn bomless_non_ascii_utf16_is_sniffed() {
        let src = "// Grüße\nclass Café { }\n".repeat(4);
        for be in [false, true] {
            let bytes16 = utf16(&src, be, false);
            let d = decode(&bytes16, None);
            assert_eq!(d.encoding, if be { UTF_16BE } else { UTF_16LE });
            assert_eq!(d.text, src);
        }
    }

    /// 64 units; `high_nul` of them have a NUL high byte, the next `low_nul`
    /// a NUL low byte, the rest `filler` as the high byte (never U+0000).
    fn units64_with(high_nul: usize, low_nul: usize, be: bool, filler: u8) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..64 {
            let (low, high) = if i < high_nul {
                (b'a', 0)
            } else if i < high_nul + low_nul {
                (0, filler)
            } else {
                (b'a', filler)
            };
            v.extend_from_slice(&if be { [high, low] } else { [low, high] });
        }
        v
    }

    /// CJK-range filler 0x9E: the bytes are not valid UTF-8 (loose path).
    fn units64(high_nul: usize, low_nul: usize, be: bool) -> Vec<u8> {
        units64_with(high_nul, low_nul, be, 0x9E)
    }

    #[test]
    fn sniff_thresholds_at_their_edges() {
        for (be, enc) in [(false, UTF_16LE), (true, UTF_16BE)] {
            // Valid-UTF-8 input (filler 0x4E): only the 1/2 path applies.
            assert!(std::str::from_utf8(&units64_with(32, 0, be, 0x4E)).is_ok());
            assert_eq!(sniff_utf16(&units64_with(32, 0, be, 0x4E)), Some(enc));
            assert_eq!(sniff_utf16(&units64_with(31, 0, be, 0x4E)), None);
            assert_eq!(sniff_utf16(&units64_with(1, 0, be, 0x4E)), None);
            // high NULs >= 1/64 of units (here 1 of 64), and at least one.
            assert_eq!(sniff_utf16(&units64(1, 0, be)), Some(enc));
            assert_eq!(sniff_utf16(&units64(0, 0, be)), None);
            // low NULs < 1/16 of units: 3 of 64 passes, 4 fails.
            assert_eq!(sniff_utf16(&units64(10, 3, be)), Some(enc));
            assert_eq!(sniff_utf16(&units64(10, 4, be)), None);
        }
        // An odd total length fails the whole-file decode.
        let mut odd = units64(64, 0, false);
        odd.push(b'x');
        assert_eq!(sniff_utf16(&odd), None);
        assert_eq!(sniff_utf16(b""), None);
        // A C0 control other than tab/LF/FF/CR rejects the candidate.
        let mut ctl = units64(64, 0, false);
        ctl.extend_from_slice(&[0x07, 0]);
        assert_eq!(sniff_utf16(&ctl), None);
        for ok in [b'\t', b'\n', 0x0C, b'\r'] {
            let mut v = units64(64, 0, false);
            v.extend_from_slice(&[ok, 0]);
            assert_eq!(sniff_utf16(&v), Some(UTF_16LE));
        }
    }

    #[test]
    fn bomless_cjk_utf16_is_sniffed_not_binary() {
        let cases = [
            "// 日本語\n",
            "-- 名前\n",
            "// 这是一个用于测试的中文注释，客户信息更新函数。我们需要检查编码是否正确识别。\nint x;\n",
            "/* 고객 정보를 업데이트하는 함수입니다 */\n",
            "日本語のテキストだけ。\n",
        ];
        for src in cases {
            for (be, enc) in [(false, UTF_16LE), (true, UTF_16BE)] {
                let bytes = utf16(src, be, false);
                assert!(!is_binary(&bytes), "{src:?} be={be}");
                let d = decode(&bytes, None);
                assert_eq!((d.encoding, d.text.as_ref(), d.lossy), (enc, src, false));
            }
        }
    }

    #[test]
    fn a_stray_nul_in_short_utf8_is_binary() {
        let bytes = [
            &b"public class A {\n  // caf\xc3\xa9\n}\n"[..],
            b"\0",
            b"int x = 1;\n",
        ]
        .concat();
        assert_eq!(bytes.len(), 42);
        assert_eq!(sniff_utf16(&bytes), None);
        assert!(is_binary(&bytes));
    }

    #[test]
    fn short_cjk_utf16_is_detected() {
        for (be, enc) in [(false, UTF_16LE), (true, UTF_16BE)] {
            let bytes = utf16("日本語\n", be, false);
            assert!(!is_binary(&bytes));
            assert_eq!(decode(&bytes, None).encoding, enc);
        }
    }

    #[test]
    fn real_binaries_stay_binary() {
        let png: &[u8] =
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x10\0\0\0\x10\x08\x06\0\0\0\x1f\xf3\xffa";
        let zip: &[u8] = b"PK\x03\x04\x14\0\0\0\x08\0\x9c\x5e\x3e\x5b\xa1\x2c\x11\x07\x0c\0\0\0\x0a\0\0\0\x05\0\0\0a.txt";
        let mut exe = b"MZ\x90\0\x03\0\0\0\x04\0\0\0\xff\xff\0\0\xb8\0\0\0\0\0\0\0@\0".to_vec();
        exe.extend_from_slice(&[0; 32]);
        exe.extend_from_slice(b"This program cannot be run in DOS mode.\r\r\n$\0\0\0");
        for bin in [png, zip, &exe[..], &[0u8; 64][..]] {
            assert!(is_binary(bin));
            assert_eq!(sniff_utf16(bin), None);
        }
    }

    #[test]
    fn sniff_only_looks_at_the_window() {
        // Past the window the NUL pattern stops; the file still decodes.
        let mut src = "a".repeat(UTF16_SNIFF_WINDOW);
        src.push_str(&"日本".repeat(100));
        let bytes16 = utf16(&src, false, false);
        let d = decode(&bytes16, None);
        assert_eq!(d.encoding, UTF_16LE);
        assert_eq!(d.text, src);
    }

    #[test]
    fn legacy_encodings_are_detected_and_never_lossy() {
        let cases: &[(&'static Encoding, &str)] = &[
            (
                WINDOWS_1252,
                "// Données du client: café, naïve, façade, élève, garçon, déjà vu.\nprocedure MettreÀJour(var Enregistrement: TClient);\nbegin\n  Résultat := 'Référence générée';\nend;\n",
            ),
            (
                SHIFT_JIS,
                "// 顧客情報を更新する関数です。日本語のコメントがここにあります。\nint 顧客番号 = 0; // これはテストのためのサンプルコードです。\n",
            ),
            (
                GBK,
                "// 这是一个用于测试的中文注释，客户信息更新函数。\nint 客户编号 = 0; // 我们需要检查编码是否正确识别。\n",
            ),
            (
                EUC_KR,
                "// 고객 정보를 업데이트하는 함수입니다. 한국어 주석이 여기에 있습니다.\nint 고객번호 = 0; // 이것은 테스트용 샘플 코드입니다.\n",
            ),
            (
                BIG5,
                "// 這是一個用於測試的中文註解，客戶資訊更新函式。\nint 客戶編號 = 0; // 我們需要檢查編碼是否正確識別。\n",
            ),
        ];
        for (encoding, src) in cases {
            let bytes = encode(encoding, src);
            assert!(std::str::from_utf8(&bytes).is_err());
            let d = decode(&bytes, None);
            assert_eq!(d.encoding, *encoding, "detecting {}", encoding.name());
            assert_eq!(d.text, *src);
            assert!(!d.lossy);
            assert!(!is_binary(&bytes));
        }
    }

    #[test]
    fn iso_2022_jp_decodes_with_a_hint() {
        let src = "// 顧客情報を更新する\nint x = 0;\n";
        let bytes = encode(ISO_2022_JP, src);
        let d = decode(&bytes, Some(ISO_2022_JP));
        assert_eq!(d.encoding, ISO_2022_JP);
        assert_eq!(d.text, src);
        assert!(!d.lossy);
        // Auto-detected by its escapes, before the UTF-8 check.
        let d = decode(&bytes, None);
        assert_eq!(
            (d.encoding, d.text.as_ref(), d.lossy),
            (ISO_2022_JP, src, false)
        );
        // Both JIS X 0208 designations are recognised; ESC ( B / ESC ( J
        // alone are not, and plain ASCII is UTF-8.
        for esc in [&b"\x1b$@"[..], b"\x1b$B"] {
            assert!(looks_like_iso_2022_jp(&[b"x ", esc, b" y"].concat()));
        }
        for esc in [&b"\x1b(B"[..], b"\x1b(J"] {
            let bytes = [b"x ", esc, b" y"].concat();
            assert!(!looks_like_iso_2022_jp(&bytes));
            assert_eq!(decode(&bytes, None).encoding, UTF_8);
        }
        assert_eq!(decode(b"int x = 0;", None).encoding, UTF_8);
        // Escapes that do not decode cleanly fall through to UTF-8.
        let bad = b"\x1b$B\x7f\x7f";
        assert!(ISO_2022_JP
            .decode_without_bom_handling_and_without_replacement(bad)
            .is_none());
        assert_eq!(decode(bad, None).encoding, UTF_8);
        // Not 7-bit: not ISO-2022-JP.
        assert!(!looks_like_iso_2022_jp("\x1b$B é".as_bytes()));
    }

    #[test]
    fn undetectable_bytes_fall_back_to_windows_1252_without_loss() {
        // Every byte value that is not valid UTF-8 on its own.
        let bytes: Vec<u8> = (0x80u8..=0xFF).collect();
        let d = decode(&bytes, None);
        assert_eq!(d.encoding, WINDOWS_1252);
        assert!(!d.lossy);
        assert_eq!(d.text.chars().count(), bytes.len());
    }

    #[test]
    fn hint_is_used_and_lossy_when_it_does_not_fit() {
        let bytes = encode(WINDOWS_1252, "café");
        let d = decode(&bytes, Some(WINDOWS_1252));
        assert_eq!(
            (d.encoding, d.text.as_ref(), d.lossy),
            (WINDOWS_1252, "café", false)
        );
        let d = decode(&bytes, Some(UTF_8));
        assert_eq!(d.encoding, UTF_8);
        assert_eq!(d.text, "caf\u{FFFD}");
        assert!(d.lossy);
        // A hint beats the UTF-16 sniff and the UTF-8 check.
        let bytes16 = utf16(ASCII_SRC, false, false);
        let d = decode(&bytes16, Some(WINDOWS_1252));
        assert_eq!(d.encoding, WINDOWS_1252);
    }

    #[test]
    fn replacement_is_refused_as_a_hint() {
        for label in ["replacement", "iso-2022-kr", "csiso2022kr", "hz-gb-2312"] {
            assert_eq!(
                hint_from_label(label),
                Err(EncodingError::Replacement(label.to_string()))
            );
        }
        assert!(matches!(
            hint_from_label("klingon"),
            Err(EncodingError::UnknownLabel(_))
        ));
        assert_eq!(hint_from_label("latin1"), Ok(WINDOWS_1252));
        assert_eq!(hint_from_label("Shift_JIS"), Ok(SHIFT_JIS));
        assert_eq!(hint_from_label("utf-16le"), Ok(UTF_16LE));
        // decode itself ignores a replacement hint rather than erasing the file.
        let d = decode(b"fn main() {}", Some(REPLACEMENT));
        assert_eq!((d.encoding, d.text.as_ref()), (UTF_8, "fn main() {}"));
    }

    #[test]
    fn binary_detection() {
        let png: &[u8] =
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x10\0\0\0\x10\x08\x06\0\0\0\x1f\xf3\xffa";
        assert!(is_binary(png));
        assert!(!is_binary(&utf16(ASCII_SRC, false, false)));
        assert!(!is_binary(&utf16(ASCII_SRC, true, false)));
        assert!(!is_binary(&utf16(ASCII_SRC, true, true)));
        assert!(!is_binary(b"plain text"));
        assert!(is_binary(b"text\0with a nul"));
        // An explicit UTF-16 hint makes NUL-bearing bytes text.
        assert!(!is_binary_with_hint(b"a\0b\0\0\0\0\0", Some(UTF_16LE)));
        assert!(is_binary_with_hint(b"a\0b\0\0\0\0\0", Some(WINDOWS_1252)));
    }

    #[test]
    fn code_page_mapping() {
        assert_eq!(encoding_for_code_page(1252), Some(WINDOWS_1252));
        assert_eq!(encoding_for_code_page(932), Some(SHIFT_JIS));
        assert_eq!(encoding_for_code_page(936), Some(GBK));
        assert_eq!(encoding_for_code_page(949), Some(EUC_KR));
        assert_eq!(encoding_for_code_page(950), Some(BIG5));
        assert_eq!(encoding_for_code_page(874), Some(WINDOWS_874));
        for cp in 1250..=1258 {
            assert!(encoding_for_code_page(cp).is_some());
        }
        assert_eq!(encoding_for_code_page(65001), None);
        assert_eq!(encoding_for_code_page(437), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn ansi_is_windows_1252_off_windows() {
        assert_eq!(resolve_ansi(), Ok(WINDOWS_1252));
    }

    #[cfg(windows)]
    #[test]
    fn ansi_is_the_mapped_system_code_page_on_windows() {
        let cp = unsafe { windows_sys::Win32::Globalization::GetACP() };
        match encoding_for_code_page(cp) {
            Some(e) => assert_eq!(resolve_ansi(), Ok(e)),
            None => assert_eq!(resolve_ansi(), Err(EncodingError::UnmappedCodePage(cp))),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        #[test]
        fn random_bytes_never_panic_and_lossy_iff_replaced(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            hint_ix in 0usize..6,
        ) {
            let hints = [None, Some(UTF_8), Some(UTF_16LE), Some(UTF_16BE), Some(SHIFT_JIS), Some(WINDOWS_1252)];
            let hint = hints[hint_ix];
            let d = decode(&bytes, hint);
            let _ = is_binary(&bytes);
            // Reference: the input's own U+FFFDs, decoded strictly, versus the output's.
            match d.encoding.decode_without_bom_handling_and_without_replacement(&bytes) {
                Some(clean) => {
                    prop_assert!(!d.lossy);
                    prop_assert_eq!(clean.as_ref(), d.text.as_ref());
                }
                None => prop_assert!(d.lossy),
            }
            // Auto-detection without a BOM is never lossy.
            if hint.is_none() && Encoding::for_bom(&bytes).is_none() {
                prop_assert!(!d.lossy);
            }
        }

        #[test]
        fn valid_utf8_without_nuls_decodes_to_itself(s in "[^\u{0}]{0,200}") {
            let d = decode(s.as_bytes(), None);
            prop_assert_eq!(d.encoding, UTF_8);
            prop_assert!(!d.lossy);
            prop_assert_eq!(d.text.as_ref(), s.as_str());
        }
    }
}
