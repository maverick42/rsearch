//! Encoding detection, content sniffing and text decoding.
//!
//! This module is the single reusable decoding boundary shared by normal
//! files, archive entries and (in the future) the exact search verifier.
//! The same bytes always produce the same logical UTF-8 text, whether they
//! are decoded for indexing or for verification; parity is enforced by
//! tests.
//!
//! # Detection order (mandatory)
//!
//! 1. BOM detection: UTF-32 LE, UTF-32 BE, UTF-8, UTF-16 LE, UTF-16 BE.
//! 2. ZIP magic detection (`PK\x03\x04`, `PK\x05\x06`, `PK\x07\x08`).
//! 3. Binary heuristic (only when no BOM was found).
//!
//! The binary heuristic must never run before BOM handling, otherwise
//! UTF-16 files (which legitimately contain NUL bytes) would be
//! misclassified as binary.
//!
//! # Binary heuristic (no BOM)
//!
//! A prefix is considered binary when:
//!
//! * it contains at least one NUL byte (`0x00`); or
//! * more than 10% of its bytes are C0 control characters other than
//!   tab (`0x09`), LF (`0x0A`), VT (`0x0B`), FF (`0x0C`) and CR (`0x0D`),
//!   or DEL (`0x7F`).
//!
//! High bytes (`>= 0x80`) alone never classify content as binary: they may
//! be valid UTF-8 or decodable through the configured fallback encoding.
//! UTF-16 without a BOM is not auto-detected; it is classified as binary
//! by the NUL rule. This is a documented limitation.

use crate::options::EncodingKind;

/// Number of prefix bytes read before deciding how to process a file.
pub const SNIFF_PREFIX_LEN: usize = 8 * 1024;

/// BOM kinds, in detection order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BomKind {
    /// No byte-order mark found.
    None,
    /// `EF BB BF`.
    Utf8,
    /// `FF FE` (after the UTF-32 LE check).
    Utf16Le,
    /// `FE FF`.
    Utf16Be,
    /// `FF FE 00 00`.
    Utf32Le,
    /// `00 00 FE FF`.
    Utf32Be,
}

/// Detects a byte-order mark. UTF-32 BOMs are checked first because the
/// UTF-16 LE BOM (`FF FE`) is a prefix of the UTF-32 LE BOM.
pub fn detect_bom(prefix: &[u8]) -> BomKind {
    if prefix.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        BomKind::Utf32Le
    } else if prefix.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        BomKind::Utf32Be
    } else if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        BomKind::Utf8
    } else if prefix.starts_with(&[0xFF, 0xFE]) {
        BomKind::Utf16Le
    } else if prefix.starts_with(&[0xFE, 0xFF]) {
        BomKind::Utf16Be
    } else {
        BomKind::None
    }
}

/// Result of sniffing a file prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    /// Text content; the BOM that was found (or `None`).
    Text,
    /// Binary content per the documented heuristic.
    Binary,
    /// A ZIP-based archive, detected by magic bytes.
    Archive,
}

/// Sniffs a file prefix: BOM first, then ZIP magic, then the binary
/// heuristic. See the [module documentation](self) for the exact rules.
pub fn sniff_prefix(prefix: &[u8]) -> Sniffed {
    match detect_bom(prefix) {
        // UTF-32 is explicitly unsupported; callers surface a structured
        // error rather than treating it as binary or decoding it wrongly.
        BomKind::Utf32Le | BomKind::Utf32Be => Sniffed::Text,
        // A text BOM wins over every binary heuristic: UTF-16 documents
        // contain NUL bytes but must not be classified as binary.
        BomKind::Utf8 | BomKind::Utf16Le | BomKind::Utf16Be => Sniffed::Text,
        BomKind::None => {
            if is_zip_magic(prefix) {
                Sniffed::Archive
            } else if looks_binary(prefix) {
                Sniffed::Binary
            } else {
                Sniffed::Text
            }
        }
    }
}

/// ZIP local-file-header, end-of-central-directory and spanned-marker
/// signatures.
pub fn is_zip_magic(prefix: &[u8]) -> bool {
    prefix.starts_with(&[0x50, 0x4B, 0x03, 0x04])
        || prefix.starts_with(&[0x50, 0x4B, 0x05, 0x06])
        || prefix.starts_with(&[0x50, 0x4B, 0x07, 0x08])
}

/// Binary heuristic for content without a BOM. See the
/// [module documentation](self).
pub fn looks_binary(prefix: &[u8]) -> bool {
    if prefix.is_empty() {
        return false;
    }
    if prefix.contains(&0x00) {
        return true;
    }
    let control = prefix
        .iter()
        .filter(|&&b| {
            let text_ws = matches!(b, 0x09..=0x0D);
            (b < 0x20 && !text_ws) || b == 0x7F
        })
        .count();
    control * 10 > prefix.len()
}

/// Structured decoding failure. All variants are recoverable per-file
/// errors; none of them ever produce lossy replacement output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// A UTF-32 BOM was found; UTF-32 is unsupported in this version.
    UnsupportedUtf32,
    /// The content is not valid UTF-8 and no fallback encoding is
    /// configured.
    InvalidUtf8,
    /// UTF-16 content (BOM present) is not valid UTF-16 (unpaired
    /// surrogate or odd length).
    InvalidUtf16,
    /// Defensive variant: the fallback encoding rejected the content.
    /// In practice the WHATWG windows-1252 mapping is total (every byte
    /// maps, including 0x81 to U+0081), so this error is not expected;
    /// it exists so that any future non-total fallback stays explicit
    /// instead of silently producing replacement characters.
    InvalidWindows1252,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            DecodeError::UnsupportedUtf32 => "UTF-32 content is not supported",
            DecodeError::InvalidUtf8 => "content is not valid UTF-8",
            DecodeError::InvalidUtf16 => "content is not valid UTF-16",
            DecodeError::InvalidWindows1252 => {
                "content is not valid for the Windows-1252 fallback encoding"
            }
        };
        f.write_str(msg)
    }
}

impl std::error::Error for DecodeError {}

/// Decoded text plus the metadata needed for statistics and parity
/// checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedText {
    /// Fully decoded, normalized UTF-8 content. BOM bytes are never part
    /// of it. Never lossy: decoding either succeeds exactly or fails with
    /// a [`DecodeError`].
    pub text: String,
    /// BOM that was detected on the input.
    pub bom: BomKind,
    /// Whether the Windows-1252 fallback encoding was used.
    pub used_fallback: bool,
}

/// Decodes raw bytes into normalized UTF-8 text.
///
/// Rules:
///
/// * UTF-32 BOM: [`DecodeError::UnsupportedUtf32`].
/// * UTF-8 (with or without BOM): strict decoding; the BOM is stripped
///   and never becomes part of the indexed content.
/// * UTF-16 LE/BE (BOM required): decoded with `encoding_rs`; invalid
///   content is an error, never lossy.
/// * No BOM: strict UTF-8 first; on failure, an explicit
///   `Windows1252` fallback may be used (`used_fallback` is then `true`).
///   Without a configured fallback the content is an
///   [`DecodeError::InvalidUtf8`] error.
pub fn decode_bytes(
    bytes: &[u8],
    fallback: Option<EncodingKind>,
) -> Result<DecodedText, DecodeError> {
    let bom = detect_bom(bytes);
    match bom {
        BomKind::Utf32Le | BomKind::Utf32Be => Err(DecodeError::UnsupportedUtf32),
        BomKind::Utf8 => {
            let body = &bytes[3..];
            match std::str::from_utf8(body) {
                Ok(text) => Ok(DecodedText {
                    text: text.to_owned(),
                    bom,
                    used_fallback: false,
                }),
                Err(_) => Err(DecodeError::InvalidUtf8),
            }
        }
        BomKind::Utf16Le => decode_utf16(&bytes[2..], bom, true),
        BomKind::Utf16Be => decode_utf16(&bytes[2..], bom, false),
        BomKind::None => match std::str::from_utf8(bytes) {
            Ok(text) => Ok(DecodedText {
                text: text.to_owned(),
                bom,
                used_fallback: false,
            }),
            Err(_) => match fallback {
                Some(EncodingKind::Windows1252) => {
                    let (text, had_errors) =
                        encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes);
                    if had_errors {
                        Err(DecodeError::InvalidWindows1252)
                    } else {
                        Ok(DecodedText {
                            text: text.into_owned(),
                            bom,
                            used_fallback: true,
                        })
                    }
                }
                // An explicit UTF-8 fallback behaves like no fallback:
                // strict UTF-8 already failed.
                Some(EncodingKind::Utf8) | None => Err(DecodeError::InvalidUtf8),
            },
        },
    }
}

fn decode_utf16(
    body: &[u8],
    bom: BomKind,
    little_endian: bool,
) -> Result<DecodedText, DecodeError> {
    let encoding = if little_endian {
        encoding_rs::UTF_16LE
    } else {
        encoding_rs::UTF_16BE
    };
    let (text, had_errors) = encoding.decode_without_bom_handling(body);
    if had_errors {
        Err(DecodeError::InvalidUtf16)
    } else {
        Ok(DecodedText {
            text: text.into_owned(),
            bom,
            used_fallback: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        v.extend_from_slice(
            &s.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>(),
        );
        v
    }

    fn utf16be(s: &str) -> Vec<u8> {
        let mut v = vec![0xFE, 0xFF];
        v.extend_from_slice(
            &s.encode_utf16()
                .flat_map(u16::to_be_bytes)
                .collect::<Vec<u8>>(),
        );
        v
    }

    #[test]
    fn bom_detection_covers_all_kinds_in_priority_order() {
        assert_eq!(detect_bom(&[0xFF, 0xFE, 0x00, 0x00]), BomKind::Utf32Le);
        assert_eq!(detect_bom(&[0x00, 0x00, 0xFE, 0xFF]), BomKind::Utf32Be);
        assert_eq!(detect_bom(&[0xEF, 0xBB, 0xBF]), BomKind::Utf8);
        assert_eq!(detect_bom(&[0xFF, 0xFE]), BomKind::Utf16Le);
        assert_eq!(detect_bom(&[0xFE, 0xFF]), BomKind::Utf16Be);
        assert_eq!(detect_bom(b"plain"), BomKind::None);
        // UTF-32 LE BOM must not be detected as UTF-16 LE.
        assert_eq!(
            detect_bom(&[0xFF, 0xFE, 0x00, 0x00, 0x41, 0x00]),
            BomKind::Utf32Le
        );
    }

    #[test]
    fn zip_magic_is_detected() {
        assert!(is_zip_magic(&[0x50, 0x4B, 0x03, 0x04, 0x00]));
        assert!(is_zip_magic(&[0x50, 0x4B, 0x05, 0x06]));
        assert!(is_zip_magic(&[0x50, 0x4B, 0x07, 0x08]));
        assert!(!is_zip_magic(b"PK\x02\x03"));
        assert!(!is_zip_magic(b"hello"));
    }

    #[test]
    fn binary_heuristic_uses_nul_and_control_ratio() {
        assert!(looks_binary(b"hello\x00world"));
        assert!(looks_binary(&[0x01; 100]));
        assert!(!looks_binary(b"plain text with spaces and punctuation!"));
        assert!(!looks_binary(&[0xC3, 0xA9, 0xC3, 0xA8, 0xE2, 0x82, 0xAC])); // UTF-8 é è €
        assert!(!looks_binary(b""));
        assert!(!looks_binary(b"line1\r\nline2\ttab"));
    }

    #[test]
    fn sniff_detects_utf16_with_nul_bytes_as_text() {
        let bytes = utf16le("hello");
        assert_eq!(sniff_prefix(&bytes), Sniffed::Text);
    }

    #[test]
    fn sniff_detects_zip_as_archive() {
        assert_eq!(sniff_prefix(&[0x50, 0x4B, 0x03, 0x04]), Sniffed::Archive);
    }

    #[test]
    fn decode_empty_file() {
        let out = decode_bytes(b"", None).unwrap();
        assert_eq!(out.text, "");
        assert_eq!(out.bom, BomKind::None);
        assert!(!out.used_fallback);
    }

    #[test]
    fn decode_plain_utf8() {
        let out = decode_bytes("héllo wörld".as_bytes(), None).unwrap();
        assert_eq!(out.text, "héllo wörld");
    }

    #[test]
    fn decode_utf8_bom_is_stripped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("hello".as_bytes());
        let out = decode_bytes(&bytes, None).unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(out.bom, BomKind::Utf8);
    }

    #[test]
    fn decode_utf8_bom_with_invalid_content_fails() {
        let bytes = [0xEF, 0xBB, 0xBF, 0xFF, 0xFE];
        assert_eq!(decode_bytes(&bytes, None), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn decode_utf16_le() {
        let out = decode_bytes(&utf16le("héllo €"), None).unwrap();
        assert_eq!(out.text, "héllo €");
        assert_eq!(out.bom, BomKind::Utf16Le);
    }

    #[test]
    fn decode_utf16_be() {
        let out = decode_bytes(&utf16be("héllo €"), None).unwrap();
        assert_eq!(out.text, "héllo €");
        assert_eq!(out.bom, BomKind::Utf16Be);
    }

    #[test]
    fn decode_utf16_with_nul_codepoint_round_trips() {
        let out = decode_bytes(&utf16le("a\u{0}b"), None).unwrap();
        assert_eq!(out.text, "a\u{0}b");
    }

    #[test]
    fn decode_utf16_unpaired_surrogate_fails() {
        let bytes = [0xFF, 0xFE, 0x00, 0xD8];
        assert_eq!(decode_bytes(&bytes, None), Err(DecodeError::InvalidUtf16));
    }

    #[test]
    fn decode_utf16_odd_length_fails() {
        let bytes = [0xFF, 0xFE, b'a'];
        assert_eq!(decode_bytes(&bytes, None), Err(DecodeError::InvalidUtf16));
    }

    #[test]
    fn decode_invalid_utf8_without_fallback_fails() {
        // Careful: 0xFF 0xFE is a UTF-16 LE BOM, so invalid UTF-8 test
        // data must not accidentally start with a BOM.
        assert_eq!(
            decode_bytes(&[0xC0, 0xAF], None),
            Err(DecodeError::InvalidUtf8)
        );
    }

    #[test]
    fn decode_invalid_utf8_with_windows1252_fallback() {
        let bytes = b"caf\xE9"; // é in Windows-1252, invalid UTF-8
        let out = decode_bytes(bytes, Some(EncodingKind::Windows1252)).unwrap();
        assert_eq!(out.text, "café");
        assert!(out.used_fallback);
    }

    #[test]
    fn decode_valid_utf8_with_windows1252_fallback_stays_strict() {
        // Valid UTF-8 never goes through the fallback, whatever the
        // configured fallback: é is the two-byte UTF-8 sequence, and
        // decoding it as Windows-1252 would have produced "cafÃ©".
        let bytes = "café".as_bytes();
        let out = decode_bytes(bytes, Some(EncodingKind::Windows1252)).unwrap();
        assert_eq!(out.text, "café");
        assert!(!out.used_fallback);
    }

    #[test]
    fn decode_fallback_windows1252_is_total_per_whatwg() {
        // The WHATWG windows-1252 mapping is total: every byte has a
        // defined mapping (0x81 maps to U+0081, and so on). The fallback
        // decode is therefore deterministic and never produces
        // replacement characters.
        let bytes = [0x41, 0x81];
        let out = decode_bytes(&bytes, Some(EncodingKind::Windows1252)).unwrap();
        assert_eq!(out.text, "A\u{81}");
        assert!(out.used_fallback);
    }

    #[test]
    fn decode_utf32_le_fails_explicitly() {
        assert_eq!(
            decode_bytes(&[0xFF, 0xFE, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00], None),
            Err(DecodeError::UnsupportedUtf32)
        );
    }

    #[test]
    fn decode_utf32_be_fails_explicitly() {
        assert_eq!(
            decode_bytes(&[0x00, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x00, 0x41], None),
            Err(DecodeError::UnsupportedUtf32)
        );
    }

    #[test]
    fn decode_binary_content_is_reported_as_invalid_utf8() {
        let bytes: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        assert_eq!(decode_bytes(&bytes, None), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn decode_never_produces_replacement_characters() {
        // Invalid UTF-8 with and without fallback must either fail or
        // decode exactly; U+FFFD must never appear from decoding.
        let bytes = b"abc\xFFdef";
        let out = decode_bytes(bytes, Some(EncodingKind::Windows1252)).unwrap();
        assert!(!out.text.contains('\u{FFFD}'));
        assert!(decode_bytes(bytes, None).is_err());
    }

    #[test]
    fn decode_utf8_fallback_behaves_like_strict_utf8() {
        let good = "hello".as_bytes();
        assert!(decode_bytes(good, Some(EncodingKind::Utf8)).is_ok());
        let bad = [0xFF];
        assert_eq!(
            decode_bytes(&bad, Some(EncodingKind::Utf8)),
            Err(DecodeError::InvalidUtf8)
        );
    }

    #[test]
    fn decoding_is_deterministic_for_indexing_and_verification_parity() {
        // The same logical text encoded in different ways must decode to
        // the same UTF-8 string.
        let text = "The quick brown fox jumps over the lazy dog — héllo € 日本語";
        let utf8 = decode_bytes(text.as_bytes(), None).unwrap();
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(text.as_bytes());
        let utf8_bom = decode_bytes(&with_bom, None).unwrap();
        assert_eq!(utf8.text, utf8_bom.text);
        assert_eq!(utf8.text, text);
    }
}
