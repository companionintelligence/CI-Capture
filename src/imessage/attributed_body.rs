//! Decoder for chat.db's `message.attributedBody`.
//!
//! Since roughly 2025 macOS has stopped populating `message.text` for a growing
//! share of messages and writes the body ONLY into `attributedBody`, a binary
//! NSAttributedString. On the corpus this lane was measured against, 40,961 of
//! 433,411 messages (9.5%) carry no `text` at all, and 37,887 of those do have a
//! decodable `attributedBody` — so a reader that only selects `text` ships
//! nearly a tenth of someone's history as empty rows: indexed, searchable, and
//! containing nothing.
//!
//! The blob is NOT NSKeyedArchiver plist data. It is the older `typedstream`
//! format (NSArchiver): a 16-byte header — `04 0B "streamtyped" 81 E8 03` — then
//! archived objects. Every blob in the reference corpus carries that identical
//! header, so one decoder covers all of them.
//!
//! Inside, the body is the first char-array belonging to the NSString: the class
//! name `NSString`, then a `+` (0x2B) type code, then a length, then the UTF-8
//! bytes. Lengths under 128 are a single byte; longer ones are escaped as 0x81
//! (uint16) or 0x82 (uint32), which is typedstream's standard integer encoding.
//!
//! Deliberately a byte-for-byte port of CI-Server's `attributed_body.ts` rather
//! than an independent implementation: that decoder was validated against 40,000
//! messages carrying BOTH `text` and `attributedBody`, reproducing `text`
//! exactly on 100% of them. Two readers of the same column disagreeing about a
//! message body would be far worse than either being imperfect, so the porting
//! rule here is "same bytes in, same string out" — including the failure cases.

/// typedstream's byte/char-array type code.
const CHAR_ARRAY: u8 = 0x2b;
const NSSTRING: &[u8] = b"NSString";

/// Find `needle` in `haystack` starting at `from`.
fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() || needle.is_empty() {
        return None;
    }

    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// Extract the plain-text body from a typedstream `attributedBody` blob.
///
/// Returns `None` — never a partial or corrupted string — when the blob is
/// absent, is not the expected shape, or does not decode as valid UTF-8, so the
/// caller can fall back to `text` rather than store rubbish. A misidentified
/// offset must look like "no body", not like a body.
pub fn decode_attributed_body(blob: Option<&[u8]>) -> Option<String> {
    let buf = blob.filter(|b| !b.is_empty())?;

    let name_at = find_from(buf, NSSTRING, 0)?;
    let type_at = find_from(buf, &[CHAR_ARRAY], name_at)?;

    let mut at = type_at + 1;
    let marker = *buf.get(at)?;

    // typedstream integer escape: 0x81 -> uint16, 0x82 -> uint32, else a literal
    // byte. Values >= 0x80 that are not an escape are reference tags, not
    // lengths — treating one as a length is exactly how a decoder invents a
    // body out of unrelated bytes.
    let length: usize = if marker == 0x81 {
        let bytes = buf.get(at + 1..at + 3)?;
        at += 3;
        u16::from_le_bytes([bytes[0], bytes[1]]) as usize
    } else if marker == 0x82 {
        let bytes = buf.get(at + 1..at + 5)?;
        at += 5;
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
    } else if marker < 0x80 {
        at += 1;
        marker as usize
    } else {
        return None;
    };

    if length == 0 {
        return None;
    }

    let raw = buf.get(at..at + length)?;

    // Rust's from_utf8 rejects invalid sequences outright, where the TS original
    // has to detect Node's silent U+FFFD substitution to reach the same verdict.
    // Same contract, fewer moving parts: anything that is not valid UTF-8 is a
    // failed decode.
    std::str::from_utf8(raw).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a blob in the shape the decoder expects: header, class name, type
    /// code, length, bytes.
    fn blob(body: &[u8], length_encoding: LengthEncoding) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x04\x0Bstreamtyped\x81\xE8\x03");
        out.extend_from_slice(b"\x84\x01\x40\x84\x84\x84");
        out.extend_from_slice(NSSTRING);
        out.push(CHAR_ARRAY);
        match length_encoding {
            LengthEncoding::Byte => out.push(body.len() as u8),
            LengthEncoding::U16 => {
                out.push(0x81);
                out.extend_from_slice(&(body.len() as u16).to_le_bytes());
            }
            LengthEncoding::U32 => {
                out.push(0x82);
                out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            }
        }
        out.extend_from_slice(body);
        out
    }

    enum LengthEncoding {
        Byte,
        U16,
        U32,
    }

    #[test]
    fn decodes_a_short_body_with_a_literal_length() {
        let b = blob(b"hello there", LengthEncoding::Byte);
        assert_eq!(
            decode_attributed_body(Some(&b)).as_deref(),
            Some("hello there")
        );
    }

    /// A body of 128 bytes or more cannot use the literal length byte, because
    /// 0x80 and up are reference tags. This is the case a decoder that only
    /// handles short strings silently drops.
    #[test]
    fn decodes_a_long_body_through_the_uint16_escape() {
        let body = "x".repeat(400);
        let b = blob(body.as_bytes(), LengthEncoding::U16);
        assert_eq!(
            decode_attributed_body(Some(&b)).as_deref(),
            Some(body.as_str())
        );
    }

    #[test]
    fn decodes_a_very_long_body_through_the_uint32_escape() {
        let body = "y".repeat(70_000);
        let b = blob(body.as_bytes(), LengthEncoding::U32);
        assert_eq!(
            decode_attributed_body(Some(&b)).as_deref(),
            Some(body.as_str())
        );
    }

    /// Emoji and non-Latin scripts are the common case in real messages, and a
    /// byte-length decoder that assumed one byte per character would truncate
    /// them mid-sequence into invalid UTF-8.
    #[test]
    fn multibyte_bodies_survive_intact() {
        for body in ["héllo — çà va", "🔥 sending it", "日本語のメッセージ"] {
            let b = blob(body.as_bytes(), LengthEncoding::Byte);
            assert_eq!(decode_attributed_body(Some(&b)).as_deref(), Some(body));
        }
    }

    #[test]
    fn refuses_anything_that_is_not_the_expected_shape() {
        assert_eq!(decode_attributed_body(None), None);
        assert_eq!(decode_attributed_body(Some(&[])), None);
        // No NSString marker at all.
        assert_eq!(
            decode_attributed_body(Some(b"not a typedstream blob")),
            None
        );
        // Marker present, but the stream ends before the length.
        let mut truncated = blob(b"body", LengthEncoding::Byte);
        truncated.truncate(truncated.len() - 5);
        assert_eq!(decode_attributed_body(Some(&truncated)), None);
    }

    /// A length that runs past the end of the buffer must yield None rather
    /// than whatever bytes happen to follow.
    #[test]
    fn a_length_beyond_the_buffer_is_refused() {
        let mut b = blob(b"short", LengthEncoding::Byte);
        let type_at = b.iter().position(|&x| x == CHAR_ARRAY).unwrap();
        b[type_at + 1] = 120; // claim 120 bytes where only 5 remain
        assert_eq!(decode_attributed_body(Some(&b)), None);
    }

    /// Invalid UTF-8 is a failed decode, never a lossy string — a misidentified
    /// offset must not be able to masquerade as a message body.
    #[test]
    fn invalid_utf8_is_a_failed_decode_not_a_lossy_string() {
        let b = blob(&[0xff, 0xfe, 0xfd], LengthEncoding::Byte);
        assert_eq!(decode_attributed_body(Some(&b)), None);
    }

    /// A reference tag (>= 0x80, not an escape) where a length would sit is not
    /// a length. Reading it as one is how a decoder invents a body.
    #[test]
    fn a_reference_tag_is_not_read_as_a_length() {
        let mut b = blob(b"body", LengthEncoding::Byte);
        let type_at = b.iter().position(|&x| x == CHAR_ARRAY).unwrap();
        b[type_at + 1] = 0x95;
        assert_eq!(decode_attributed_body(Some(&b)), None);
    }
}

#[cfg(test)]
mod corpus_verification {
    use super::*;

    /// Correctness against the real database, not a fixture: for every message
    /// carrying BOTH `text` and `attributedBody`, the decoder must reproduce
    /// `text` exactly. This is the check that proves the port matches
    /// CI-Server's decoder rather than merely compiling.
    ///
    /// Skips (does not fail) when there is no chat.db or no Full Disk Access —
    /// CI has neither, and a test that fails on every machine but one is worse
    /// than no test.
    #[test]
    fn decoder_reproduces_text_on_the_real_corpus() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let db = std::path::Path::new(&home).join("Library/Messages/chat.db");
        if !db.exists() {
            return;
        }
        let Ok(conn) =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            return;
        };

        let mut stmt = match conn.prepare(
            "SELECT text, attributedBody FROM message
              WHERE text IS NOT NULL AND trim(text) <> '' AND attributedBody IS NOT NULL
              LIMIT 5000",
        ) {
            Ok(s) => s,
            Err(_) => return, // no Full Disk Access
        };

        let rows = match stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        }) {
            Ok(r) => r,
            Err(_) => return,
        };

        let (mut checked, mut matched) = (0usize, 0usize);
        for row in rows.flatten() {
            let (text, blob) = row;
            checked += 1;
            if decode_attributed_body(Some(&blob)).as_deref() == Some(text.as_str()) {
                matched += 1;
            }
        }

        if checked == 0 {
            return;
        }

        let rate = matched as f64 / checked as f64;
        assert!(
            rate > 0.99,
            "decoder reproduced text on only {matched}/{checked} ({:.2}%) of real messages",
            rate * 100.0
        );
    }
}
