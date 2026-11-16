//! Base64 in the two spellings the account plane uses: URL-safe without padding (tokens, codes,
//! PASETO, JWT, PKCE) and standard with padding (launch tickets on the wire, key configuration).
//! Decoding is strict: padding where there must be none, or non-zero trailing bits, is refused,
//! so every value has exactly one accepted spelling.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// URL-safe, no padding.
pub fn url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// URL-safe, no padding; `None` for anything else.
pub fn url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Standard alphabet with padding.
pub fn std_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Standard alphabet with padding; `None` for anything else.
pub fn std_decode(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_spelling_per_value() {
        assert_eq!(url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(url_decode("-_8"), Some(vec![0xfb, 0xff]));
        assert_eq!(url_decode("-_8="), None, "padding refused");
        assert_eq!(url_decode("-_9"), None, "non-zero trailing bits refused");
        assert_eq!(url_decode("+/8"), None, "standard alphabet refused");
        assert_eq!(std_encode(&[0xfb, 0xff]), "+/8=");
        assert_eq!(std_decode("+/8="), Some(vec![0xfb, 0xff]));
        assert_eq!(std_decode("+/8"), None);
    }
}
