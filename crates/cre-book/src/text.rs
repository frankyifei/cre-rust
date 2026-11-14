//! Plain-text decoding, for formats whose bytes are not declared as UTF-8.

/// Decodes a text file: strips a UTF-8 BOM, keeps UTF-8 when it is valid and
/// falls back to Latin-1 otherwise, normalising line endings either way.
///
/// A TXT book is often a legacy file, so a strict decode would lose the whole
/// book; a Latin-1 fallback keeps every byte readable.
pub fn decode_plain_text(bytes: &[u8]) -> String {
    let bytes = match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => rest,
        b => b,
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => text.replace("\r\n", "\n").replace('\r', "\n"),
        Err(_) => bytes
            .iter()
            .map(|&byte| byte as char)
            .collect::<String>()
            .replace("\r\n", "\n")
            .replace('\r', "\n"),
    }
}
