//! The mail and Firebase-import settings of a `local` sign-in instance (ACCOUNTS.md §4.1 "Two
//! identity backends", "E-mail confirmation and password reset", "Firebase import"; register
//! CR-S53). Read once at startup with the rest of [`crate::config::IdentityConfig`]:
//!
//! - `FI_MAIL_BACKEND`: `log` (default: no mail leaves the instance; each would-be mail is one
//!   warning line with its kind and a short address hash, and the ID tokens say confirmed, since
//!   nobody could receive a link; the rows stay unconfirmed, so they confirm once mail is on) or
//!   `smtp` (Resend's SMTP or any relay; confirmation required). Anything else fails startup.
//!   With `APP_ENV`/`FI_ENV=prod`, a `local` instance must use `smtp`; `log` is also refused on a
//!   public instance (`FI_CLIENT_ADDRESS=trusted-proxy` or an `https` `FI_PUBLIC_CORE_URL`) while
//!   `FI_REGISTRATION_ENABLED` is not `false`.
//! - `smtp` reads `FI_SMTP_HOST` (default `smtp.resend.com`), `FI_SMTP_PORT` (default 465: implicit
//!   TLS; 587 is STARTTLS), `FI_SMTP_USER` (default `resend`), `FI_SMTP_PASSWORD` (required, a
//!   secret: the Resend API key, from the environment or `<FI_KEYS_DIR>/FI_SMTP_PASSWORD`, never
//!   logged), `FI_MAIL_PRODUCT` (optional, the product name in mail copy; default `Game Server`)
//!   and `FI_MAIL_FROM` (required, `Name <address>` on the provider's verified domain).
//!   Links point at `FI_PUBLIC_CORE_URL` (the compose derives it from `FI_PUBLIC_URL`), required
//!   with `smtp` (and `https` in prod).
//! - `FI_FIREBASE_SCRYPT_SIGNER_KEY`, `FI_FIREBASE_SCRYPT_SALT_SEPARATOR` (secrets),
//!   `FI_FIREBASE_SCRYPT_ROUNDS`, `FI_FIREBASE_SCRYPT_MEM_COST`: the Firebase project's "Password
//!   Hash Parameters", all four or none; without them an imported Firebase hash never matches
//!   (the account uses the set-password mail).

use crate::config::{Env, is_prod};
use fi_auth::firebase_scrypt::HashConfig;
use fi_mail::{LogMailer, Mailer, Smtp, SmtpConfig};
use fi_runtime::config::ConfigError;
use std::fmt;
use std::sync::Arc;

/// Where a `local` instance's mail goes, and its links' origin.
#[derive(Clone)]
pub struct MailConfig {
    /// The backend.
    pub mailer: Arc<dyn Mailer>,
    /// The origin of the confirmation and reset pages (`FI_PUBLIC_CORE_URL`, no trailing `/`).
    pub link_base: String,
}

impl fmt::Debug for MailConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailConfig")
            .field("backend", &self.mailer.backend())
            .field("link_base", &self.link_base)
            .finish()
    }
}

impl MailConfig {
    /// `mailer` with links under `link_base`.
    pub fn new(mailer: Arc<dyn Mailer>, link_base: &str) -> Self {
        Self {
            mailer,
            link_base: link_base.trim_end_matches('/').to_owned(),
        }
    }

    /// Whether sign-up leaves the address unconfirmed until its link is opened (only when a
    /// mail can reach the person).
    pub fn requires_confirmation(&self) -> bool {
        self.mailer.delivers()
    }
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

/// The mail settings of a `local` instance whose public core origin is `core_url`.
pub fn mail_from_env(env: &dyn Env, core_url: &str) -> Result<MailConfig, ConfigError> {
    let prod = is_prod(env);
    // The product name in every mail (default: neutral; the official deployment sets its own).
    if let Some(product) = env.get("FI_MAIL_PRODUCT").filter(|v| !v.trim().is_empty()) {
        fi_mail::set_product(&product)
            .map_err(|_| invalid("FI_MAIL_PRODUCT", "1-48 characters"))?;
    }
    let smtp = match env.get("FI_MAIL_BACKEND").as_deref().map(str::trim) {
        None | Some("log") => false,
        Some("smtp") => true,
        Some(_) => return Err(invalid("FI_MAIL_BACKEND", "log or smtp")),
    };
    if !smtp {
        // A public instance that registers without mail would let anyone take any address
        // (security review H1): refuse it at startup.
        let registration = !matches!(
            env.get("FI_REGISTRATION_ENABLED").as_deref().map(str::trim),
            Some("false")
        );
        let public = env
            .get("FI_CLIENT_ADDRESS")
            .is_some_and(|v| v.trim() == "trusted-proxy")
            || core_url.trim().starts_with("https://")
            || public_host(core_url);
        if registration && public {
            return Err(invalid(
                "FI_MAIL_BACKEND",
                "log on a public instance (FI_CLIENT_ADDRESS=trusted-proxy, or an FI_PUBLIC_URL that is https or \
                 names a DNS host or a public IP) \
                 with FI_REGISTRATION_ENABLED=true: set FI_MAIL_BACKEND=smtp (FI_SMTP_PASSWORD, \
                 FI_MAIL_FROM) or FI_REGISTRATION_ENABLED=false",
            ));
        }
        if prod {
            return Err(invalid(
                "FI_MAIL_BACKEND",
                "a local sign-in instance in prod sends its confirmation and reset mail: smtp",
            ));
        }
        tracing::warn!(
            "FI_MAIL_BACKEND=log: no confirmation or password mail is sent and e-mail confirmation \
             is not required; set FI_SMTP_PASSWORD and FI_MAIL_BACKEND=smtp to turn mail on"
        );
        return Ok(MailConfig::new(Arc::new(LogMailer), core_url));
    }
    let port = match env.get("FI_SMTP_PORT") {
        None => 465,
        Some(p) => p
            .trim()
            .parse::<u16>()
            .map_err(|_| invalid("FI_SMTP_PORT", "a port number"))?,
    };
    let config = SmtpConfig {
        host: env
            .get("FI_SMTP_HOST")
            .unwrap_or_else(|| "smtp.resend.com".into()),
        port,
        user: env.get("FI_SMTP_USER").unwrap_or_else(|| "resend".into()),
        password: env
            .get("FI_SMTP_PASSWORD")
            .map(|p| p.trim().to_owned())
            .ok_or(ConfigError::Missing("FI_SMTP_PASSWORD"))?,
        from: env
            .get("FI_MAIL_FROM")
            .ok_or(ConfigError::Missing("FI_MAIL_FROM"))?,
    };
    let base = core_url.trim_end_matches('/');
    let scheme_ok = if prod {
        base.starts_with("https://")
    } else {
        base.starts_with("https://") || base.starts_with("http://")
    };
    if !scheme_ok {
        return Err(invalid(
            "FI_PUBLIC_CORE_URL",
            "the mail links need the instance's public origin (https in prod)",
        ));
    }
    let mailer = Smtp::new(&config).map_err(|e| invalid(e.name, e.reason))?;
    Ok(MailConfig::new(Arc::new(mailer), base))
}

/// Whether the host of origin `url` is reachable beyond this machine and its private network: a
/// DNS name or a public IP literal. `localhost`, loopback, private, link-local, unique-local and
/// unspecified addresses (and an empty origin) are not.
pub fn public_host(url: &str) -> bool {
    let rest = url.trim();
    let rest = rest.split_once("://").map_or(rest, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return false;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            !(ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified())
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local())
        }
        // A DNS name.
        Err(_) => true,
    }
}

/// The Firebase project's hash parameters, when configured (all four or none).
pub fn firebase_hash_from_env(env: &dyn Env) -> Result<Option<HashConfig>, ConfigError> {
    let key = env.get("FI_FIREBASE_SCRYPT_SIGNER_KEY");
    let separator = env.get("FI_FIREBASE_SCRYPT_SALT_SEPARATOR");
    let rounds = env.get("FI_FIREBASE_SCRYPT_ROUNDS");
    let mem_cost = env.get("FI_FIREBASE_SCRYPT_MEM_COST");
    match (key, separator, rounds, mem_cost) {
        (None, None, None, None) => Ok(None),
        (Some(key), Some(separator), Some(rounds), Some(mem_cost)) => {
            let rounds = rounds
                .trim()
                .parse()
                .map_err(|_| invalid("FI_FIREBASE_SCRYPT_ROUNDS", "a number"))?;
            let mem_cost = mem_cost
                .trim()
                .parse()
                .map_err(|_| invalid("FI_FIREBASE_SCRYPT_MEM_COST", "a number"))?;
            HashConfig::new(&key, &separator, rounds, mem_cost)
                .map(Some)
                .map_err(|e| invalid("FI_FIREBASE_SCRYPT_SIGNER_KEY", e.0))
        }
        _ => Err(invalid(
            "FI_FIREBASE_SCRYPT_SIGNER_KEY",
            "set all four FI_FIREBASE_SCRYPT_* parameters or none",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    fn name(e: ConfigError) -> &'static str {
        match e {
            ConfigError::Invalid { name, .. } | ConfigError::Missing(name) => name,
        }
    }

    #[test]
    fn log_is_the_default_and_prod_needs_smtp() {
        let c = mail_from_env(&env(&[]), "").unwrap();
        assert_eq!(c.mailer.backend(), "log");
        assert!(!c.requires_confirmation());
        let e = mail_from_env(&env(&[("FI_ENV", "prod")]), "https://api.example.test");
        assert_eq!(name(e.unwrap_err()), "FI_MAIL_BACKEND");
        assert_eq!(
            name(mail_from_env(&env(&[("FI_MAIL_BACKEND", "sendgrid")]), "").unwrap_err()),
            "FI_MAIL_BACKEND"
        );
        // A public instance with open registration needs mail; closed registration may wait.
        for public in [
            mail_from_env(&env(&[]), "https://fi.example.test"),
            mail_from_env(
                &env(&[("FI_CLIENT_ADDRESS", "trusted-proxy")]),
                "http://192.0.2.1",
            ),
        ] {
            assert_eq!(name(public.unwrap_err()), "FI_MAIL_BACKEND");
        }
        // A DNS name or a public IP over plain HTTP is public too; a LAN or loopback one is not.
        for (url, public) in [
            ("http://fi.example.test:8080", true),
            ("http://203.0.113.7:8080", true),
            ("http://[2001:db8::1]:8080", true),
            ("http://127.0.0.1:8080", false),
            ("http://localhost:8080", false),
            ("http://192.168.1.91:8080", false),
            ("http://10.1.2.3", false),
            ("http://172.20.0.2:8080", false),
            ("http://169.254.1.1", false),
            ("http://[::1]:8080", false),
            ("http://[fd00::5]:8080", false),
            ("http://[fe80::1]:8080", false),
            ("", false),
        ] {
            assert_eq!(public_host(url), public, "{url}");
            assert_eq!(mail_from_env(&env(&[]), url).is_err(), public, "{url}");
        }
        let closed = env(&[
            ("FI_CLIENT_ADDRESS", "trusted-proxy"),
            ("FI_REGISTRATION_ENABLED", "false"),
        ]);
        assert!(mail_from_env(&closed, "https://fi.example.test").is_ok());
    }

    #[test]
    fn smtp_needs_the_key_the_sender_and_an_origin() {
        let full = [
            ("FI_MAIL_BACKEND", "smtp"),
            ("FI_SMTP_PASSWORD", "re_synthetic"),
            ("FI_MAIL_FROM", "Example Game <no-reply@example.test>"),
        ];
        let c = mail_from_env(&env(&full), "https://api.example.test/").unwrap();
        assert_eq!(c.mailer.backend(), "smtp");
        assert!(c.requires_confirmation());
        assert_eq!(c.link_base, "https://api.example.test");
        assert!(!format!("{c:?}").contains("re_synthetic"));
        assert_eq!(
            name(mail_from_env(&env(&full[..2]), "https://a.test").unwrap_err()),
            "FI_MAIL_FROM"
        );
        assert_eq!(
            name(mail_from_env(&env(&[full[0], full[2]]), "https://a.test").unwrap_err()),
            "FI_SMTP_PASSWORD"
        );
        assert_eq!(
            name(mail_from_env(&env(&full), "").unwrap_err()),
            "FI_PUBLIC_CORE_URL"
        );
        let mut prod = env(&full);
        prod.push(("FI_ENV", "prod".into()));
        assert_eq!(
            name(mail_from_env(&prod, "http://192.0.2.1:8080").unwrap_err()),
            "FI_PUBLIC_CORE_URL"
        );
        assert!(mail_from_env(&prod, "https://api.example.test").is_ok());
        let mut bad_port = env(&full);
        bad_port.push(("FI_SMTP_PORT", "2525".to_owned()));
        assert_eq!(
            name(mail_from_env(&bad_port, "https://a.test").unwrap_err()),
            "FI_SMTP_PORT"
        );
    }

    #[test]
    fn firebase_parameters_are_all_or_none() {
        assert!(firebase_hash_from_env(&env(&[])).unwrap().is_none());
        let all = [
            ("FI_FIREBASE_SCRYPT_SIGNER_KEY", "c2lnbmVy"),
            ("FI_FIREBASE_SCRYPT_SALT_SEPARATOR", "Bw=="),
            ("FI_FIREBASE_SCRYPT_ROUNDS", "8"),
            ("FI_FIREBASE_SCRYPT_MEM_COST", "14"),
        ];
        assert!(firebase_hash_from_env(&env(&all)).unwrap().is_some());
        assert!(firebase_hash_from_env(&env(&all[..3])).is_err());
        let mut bad = env(&all);
        bad[3].1 = "99".into();
        assert!(firebase_hash_from_env(&bad).is_err());
    }
}
