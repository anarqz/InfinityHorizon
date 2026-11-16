//! Short-lived portal codes (launcher → website, ACCOUNTS.md §4.2).
//! OD-74 removed browser-to-launcher and device/user codes.

use crate::{RandomUnavailable, Secret, random_bytes, sha256};

/// RFC 4648 base32.
pub const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
/// Characters of an authorization code.
pub const AUTHORIZATION_CODE_LEN: usize = 32;

fn pick<const N: usize>(alphabet: &[u8; 32]) -> Result<String, RandomUnavailable> {
    // 256 is a multiple of 32: masking keeps every character equally likely.
    Ok(random_bytes::<N>()?
        .iter()
        .map(|b| char::from(alphabet[usize::from(b & 31)]))
        .collect())
}

/// A new portal hand-off code (the retained launcher → website direction).
pub fn authorization_code() -> Result<Secret, RandomUnavailable> {
    pick::<AUTHORIZATION_CODE_LEN>(BASE32).map(Secret::new)
}

/// Whether `code` can be an authorization code (exact, upper case: it travels by machine).
pub fn is_authorization_code(code: &str) -> bool {
    code.len() == AUTHORIZATION_CODE_LEN && code.bytes().all(|b| BASE32.contains(&b))
}

/// The stored lookup value of a code in its canonical form.
pub fn lookup(canonical: &str) -> [u8; 32] {
    sha256(canonical.as_bytes())
}

/// A `local` instance's 6-digit e-mail confirmation code (CR-S53 follow-up): uniform over
/// `000000`-`999999` from the OS CSPRNG (rejection sampling of 32-bit values, no modulo bias).
pub fn confirmation_code() -> Result<Secret, RandomUnavailable> {
    // The largest multiple of 10^6 that fits in a u32: values at or above it are drawn again.
    const LIMIT: u32 = u32::MAX - (u32::MAX % 1_000_000);
    loop {
        let n = u32::from_le_bytes(random_bytes::<4>()?);
        if n < LIMIT {
            return Ok(Secret::new(format!("{:06}", n % 1_000_000)));
        }
    }
}

/// The 6 digits of a typed code (spaces and dashes ignored), or `None`.
pub fn normalize_confirmation_code(typed: &str) -> Option<String> {
    let digits: String = typed.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_their_shapes() {
        let code = authorization_code().unwrap();
        assert!(is_authorization_code(code.expose()), "{code:?}");
        assert!(!is_authorization_code(&code.expose().to_ascii_lowercase()));
        assert!(!is_authorization_code(&code.expose()[1..]));
        assert!(
            !is_authorization_code(&"0".repeat(32)),
            "0 and 1 are not base32"
        );
        assert_ne!(lookup("A"), lookup("B"));
    }

    #[test]
    fn confirmation_codes_are_six_digits_and_spread() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let c = confirmation_code().unwrap();
            assert_eq!(c.expose().len(), 6);
            assert!(c.expose().bytes().all(|b| b.is_ascii_digit()));
            seen.insert(c.expose().to_owned());
        }
        assert!(seen.len() > 190);
        assert_eq!(
            normalize_confirmation_code(" 123 456 ").as_deref(),
            Some("123456")
        );
        assert_eq!(
            normalize_confirmation_code("123-456").as_deref(),
            Some("123456")
        );
        for bad in ["12345", "1234567", "12a456", ""] {
            assert_eq!(normalize_confirmation_code(bad), None, "{bad}");
        }
    }
}
