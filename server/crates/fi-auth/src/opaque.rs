//! Opaque bearer secrets (ACCOUNTS.md §4.2): a kind prefix and 43 base64url characters (256 random
//! bits). The server stores only the SHA-256 of the whole token and looks a presented token up by
//! it, so a copy of the database never holds a usable secret and the lookup itself leaks nothing
//! useful through timing (the attacker does not control the hash).

use crate::{RandomUnavailable, Secret, b64, random_bytes, sha256};

/// Which secret, by prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `fi_rt_`: a launcher refresh token (rotates on every use).
    RefreshToken,
    /// `fi_it_`: an identity ticket (15 minutes, single use).
    IdentityTicket,
    /// `fi_ps_`: a portal session token (non-rotating, sliding).
    PortalSession,
    /// `fi_mt_`: a `local` instance's mail link (e-mail confirmation, password reset; single use).
    MailToken,
}

impl Kind {
    /// The prefix.
    pub const fn prefix(self) -> &'static str {
        match self {
            Kind::RefreshToken => "fi_rt_",
            Kind::IdentityTicket => "fi_it_",
            Kind::PortalSession => "fi_ps_",
            Kind::MailToken => "fi_mt_",
        }
    }
}

/// Characters after the prefix (32 bytes in base64url without padding).
pub const BODY_LEN: usize = 43;

/// A freshly minted secret and its lookup value.
#[derive(Debug)]
pub struct Minted {
    /// Sent to the client once.
    pub token: Secret,
    /// SHA-256 of the token: what the database stores.
    pub lookup: [u8; 32],
}

/// Mints a secret of `kind`.
pub fn mint(kind: Kind) -> Result<Minted, RandomUnavailable> {
    let token = format!(
        "{}{}",
        kind.prefix(),
        b64::url_encode(&random_bytes::<32>()?)
    );
    let lookup = sha256(token.as_bytes());
    Ok(Minted {
        token: Secret::new(token),
        lookup,
    })
}

/// The lookup value of a presented secret of `kind`, or `None` when it cannot be one (wrong prefix,
/// length or alphabet): the caller answers that like an unknown secret.
pub fn lookup(kind: Kind, presented: &str) -> Option<[u8; 32]> {
    let body = presented.strip_prefix(kind.prefix())?;
    if body.len() != BODY_LEN || b64::url_decode(body)?.len() != 32 {
        return None;
    }
    Some(sha256(presented.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_look_up_and_others_do_not() {
        for kind in [
            Kind::RefreshToken,
            Kind::IdentityTicket,
            Kind::PortalSession,
            Kind::MailToken,
        ] {
            let m = mint(kind).unwrap();
            let token = m.token.expose();
            assert!(token.starts_with(kind.prefix()));
            assert_eq!(token.len(), 6 + BODY_LEN);
            assert_eq!(lookup(kind, token), Some(m.lookup));
            assert_ne!(mint(kind).unwrap().lookup, m.lookup);
        }
        let rt = mint(Kind::RefreshToken).unwrap();
        let rt = rt.token.expose();
        assert_eq!(lookup(Kind::PortalSession, rt), None, "kinds never mix");
        assert_eq!(lookup(Kind::RefreshToken, &rt[..rt.len() - 1]), None);
        assert_eq!(lookup(Kind::RefreshToken, &format!("{rt}A")), None);
        assert_eq!(
            lookup(Kind::RefreshToken, &format!("fi_rt_{}", "+".repeat(43))),
            None
        );
        assert_eq!(lookup(Kind::RefreshToken, ""), None);
    }
}
