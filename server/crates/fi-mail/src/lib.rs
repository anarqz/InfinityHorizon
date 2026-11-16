//! Transactional mail of a `local` sign-in instance (ACCOUNTS.md §4.1 "Two identity backends",
//! "E-mail confirmation and password reset"; register CR-S53).
//!
//! - [`Mailer`]: what the sign-in routes call, after their transaction committed and with no
//!   pooled connection held (services/AGENTS.md: no network call inside a transaction).
//! - [`Smtp`]: Resend's SMTP (`smtp.resend.com`, user `resend`, the API key as the password), or any
//!   SMTP relay: implicit TLS on 465/2465, STARTTLS (required) on 587/2587/25, rustls with the
//!   built-in Mozilla roots, a 10-second timeout.
//! - [`LogMailer`]: an instance without the key yet (`FI_MAIL_BACKEND=log`): it logs that a mail
//!   was not delivered, with its kind and a short hash of the address, never the address, the link
//!   or the token ([`log_line`]). It [`Mailer::delivers`] nothing, so the instance does not require
//!   a confirmation nobody can receive.
//! - [`Recording`]: the tests' fake; it delivers (into memory).
//! - [`confirm_mail`] and [`reset_mail`]: plain-text and HTML alternatives, in English, Brazilian Portuguese or both
//!   ([`Lang::from_accept_language`]).
//!
//! A [`Message`]'s `Debug` prints its kind only: its address and body (which holds the link) never
//! reach a log through it.

mod templates;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a mail is for (also the log's `kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The e-mail confirmation link.
    Confirm,
    /// The password reset (or first password) link.
    Reset,
    /// An operator alert to the owner ([`alert_mail`]; CR-S100's Worlds ceiling): no player data.
    Alert,
}

impl Kind {
    /// Its name in logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Confirm => "confirm",
            Kind::Reset => "reset",
            Kind::Alert => "alert",
        }
    }
}

/// One mail. `Debug` prints the kind only.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    /// What it is for.
    pub kind: Kind,
    /// The recipient's address.
    pub to: String,
    /// The subject.
    pub subject: String,
    /// The plain-text body (holds the link and its token).
    pub text: String,
    /// The HTML alternative (holds the same code/link; never log it).
    pub html: String,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A mail that could not be sent (the reason never holds the address or the body).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("mail not sent: {0}")]
pub struct MailError(pub String);

/// A boxed send.
pub type Sending<'a> = Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

/// Sends mail.
pub trait Mailer: Send + Sync {
    /// Sends `message`.
    fn send(&self, message: Message) -> Sending<'_>;
    /// Whether a mail can reach a person (false: the log backend). An instance whose mailer
    /// delivers nothing does not require e-mail confirmation.
    fn delivers(&self) -> bool;
    /// The backend's name (`smtp`, `log`, `recording`).
    fn backend(&self) -> &'static str;
}

/// The first 12 hex characters of SHA-256 of the address: lets an operator match a log line to a
/// complaint ("did my mail go out?") without the log holding the address.
pub fn address_hash(address: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(address.trim().to_lowercase().as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// The log backend's line for `message`: kind and address hash only.
pub fn log_line(message: &Message) -> String {
    format!(
        "mail not delivered (FI_MAIL_BACKEND=log): kind={} to={}",
        message.kind.as_str(),
        address_hash(&message.to)
    )
}

/// `FI_MAIL_BACKEND=log`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogMailer;

impl Mailer for LogMailer {
    fn send(&self, message: Message) -> Sending<'_> {
        Box::pin(async move {
            tracing::warn!(
                kind = message.kind.as_str(),
                to = %address_hash(&message.to),
                "mail not delivered (FI_MAIL_BACKEND=log): set FI_SMTP_PASSWORD and FI_MAIL_BACKEND=smtp"
            );
            Ok(())
        })
    }

    fn delivers(&self) -> bool {
        false
    }

    fn backend(&self) -> &'static str {
        "log"
    }
}

/// The tests' fake: keeps every message in memory and delivers.
#[derive(Clone, Debug, Default)]
pub struct Recording {
    sent: Arc<Mutex<Vec<Message>>>,
}

impl Recording {
    /// An empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every message sent so far.
    pub fn sent(&self) -> Vec<Message> {
        self.sent.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// The messages sent to `address` (case-insensitive).
    pub fn sent_to(&self, address: &str) -> Vec<Message> {
        let address = address.trim().to_lowercase();
        self.sent()
            .into_iter()
            .filter(|m| m.to.to_lowercase() == address)
            .collect()
    }
}

impl Mailer for Recording {
    fn send(&self, message: Message) -> Sending<'_> {
        Box::pin(async move {
            if let Ok(mut sent) = self.sent.lock() {
                sent.push(message);
            }
            Ok(())
        })
    }

    fn delivers(&self) -> bool {
        true
    }

    fn backend(&self) -> &'static str {
        "recording"
    }
}

/// SMTP settings (`FI_SMTP_*`, `FI_MAIL_FROM`). `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    /// `FI_SMTP_HOST` (`smtp.resend.com`).
    pub host: String,
    /// `FI_SMTP_PORT`: 465 or 2465 (implicit TLS), 587, 2587 or 25 (STARTTLS, required).
    pub port: u16,
    /// `FI_SMTP_USER` (`resend`).
    pub user: String,
    /// `FI_SMTP_PASSWORD` (Resend: the API key). A secret.
    pub password: String,
    /// `FI_MAIL_FROM`, `Name <address>` or an address (on a domain verified at the provider).
    pub from: String,
}

impl fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// SMTP settings that cannot work (the reason names the setting).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{name}: {reason}")]
pub struct ConfigError {
    /// The variable.
    pub name: &'static str,
    /// Why.
    pub reason: &'static str,
}

/// How a port is secured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// TLS from the first byte (465, 2465).
    ImplicitTls,
    /// Plain connect, then STARTTLS, refused when the server does not offer it (587, 2587, 25).
    StartTls,
}

/// The security of `port`, or `None` for a port this client does not speak.
pub fn security_of(port: u16) -> Option<Security> {
    match port {
        465 | 2465 => Some(Security::ImplicitTls),
        587 | 2587 | 25 => Some(Security::StartTls),
        _ => None,
    }
}

/// `FI_MAIL_BACKEND=smtp`.
pub struct Smtp {
    transport: lettre::AsyncSmtpTransport<lettre::Tokio1Executor>,
    from: lettre::message::Mailbox,
}

impl fmt::Debug for Smtp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Smtp").finish_non_exhaustive()
    }
}

/// How long one send may take (connect, TLS, AUTH, DATA).
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

impl Smtp {
    /// A client for `config` (no connection is made until the first send).
    pub fn new(config: &SmtpConfig) -> Result<Self, ConfigError> {
        use lettre::transport::smtp::authentication::Credentials;
        let bad = |name, reason| ConfigError { name, reason };
        if config.host.trim().is_empty() {
            return Err(bad("FI_SMTP_HOST", "required"));
        }
        if config.user.is_empty() {
            return Err(bad("FI_SMTP_USER", "required"));
        }
        if config.password.is_empty() {
            return Err(bad("FI_SMTP_PASSWORD", "required"));
        }
        let from: lettre::message::Mailbox = config
            .from
            .trim()
            .parse()
            .map_err(|_| bad("FI_MAIL_FROM", "an address or `Name <address>`"))?;
        let host = config.host.trim();
        let builder = match security_of(config.port) {
            Some(Security::ImplicitTls) => {
                lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::relay(host)
            }
            Some(Security::StartTls) => {
                lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::starttls_relay(host)
            }
            None => {
                return Err(bad(
                    "FI_SMTP_PORT",
                    "465 or 2465 (TLS), 587, 2587 or 25 (STARTTLS)",
                ));
            }
        }
        .map_err(|_| bad("FI_SMTP_HOST", "not a host name"))?;
        let transport = builder
            .port(config.port)
            .credentials(Credentials::new(
                config.user.clone(),
                config.password.clone(),
            ))
            .timeout(Some(SEND_TIMEOUT))
            .build();
        Ok(Self { transport, from })
    }
}

impl Mailer for Smtp {
    fn send(&self, message: Message) -> Sending<'_> {
        Box::pin(async move {
            use lettre::AsyncTransport;
            let mail = smtp_message(self.from.clone(), message)?;
            match tokio::time::timeout(SEND_TIMEOUT, self.transport.send(mail)).await {
                Ok(Ok(_)) => Ok(()),
                // lettre's error says what failed (connection, TLS, AUTH, a refusal code); it
                // never holds the message or the credentials.
                Ok(Err(e)) => Err(MailError(smtp_reason(&e))),
                Err(_) => Err(MailError("timed out".into())),
            }
        })
    }

    fn delivers(&self) -> bool {
        true
    }

    fn backend(&self) -> &'static str {
        "smtp"
    }
}

/// Build MIME without opening a connection; plain text comes first, HTML second.
fn smtp_message(
    from: lettre::message::Mailbox,
    message: Message,
) -> Result<lettre::Message, MailError> {
    let to: lettre::message::Mailbox = message
        .to
        .parse()
        .map_err(|_| MailError("the recipient is not an address".into()))?;
    lettre::Message::builder()
        .from(from)
        .to(to)
        .subject(message.subject)
        .multipart(lettre::message::MultiPart::alternative_plain_html(
            message.text,
            message.html,
        ))
        .map_err(|_| MailError("the message could not be built".into()))
}

fn smtp_reason(e: &lettre::transport::smtp::Error) -> String {
    if e.is_permanent() {
        "permanent refusal (check FI_MAIL_FROM's domain and the API key)".into()
    } else if e.is_transient() {
        "transient refusal (retry later)".into()
    } else if e.is_timeout() {
        "timed out".into()
    } else if e.is_tls() {
        "TLS failed".into()
    } else {
        "connection or protocol failure".into()
    }
}

/// The language of a mail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// English.
    En,
    /// Brazilian Portuguese.
    PtBr,
    /// Both, Portuguese first (the request named neither).
    Both,
}

impl Lang {
    /// From an `Accept-Language` header: its first language tag that is English or Portuguese
    /// decides; none (or no header) is both.
    pub fn from_accept_language(header: Option<&str>) -> Self {
        let Some(header) = header else {
            return Lang::Both;
        };
        for part in header.split(',') {
            let tag = part
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if tag == "pt" || tag.starts_with("pt-") {
                return Lang::PtBr;
            }
            if tag == "en" || tag.starts_with("en-") {
                return Lang::En;
            }
        }
        Lang::Both
    }
}

/// The product name in mail copy when `FI_MAIL_PRODUCT` is not set (neutral: an operator brands
/// its own mail).
pub const DEFAULT_PRODUCT: &str = "Game Server";

static PRODUCT_NAME: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Sets the product name every mail names (`FI_MAIL_PRODUCT`, read at startup by the service).
/// Empty or longer than 48 characters is refused.
pub fn set_product(name: &str) -> Result<(), MailError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 48 || name.chars().any(char::is_control) {
        return Err(MailError("FI_MAIL_PRODUCT: 1-48 characters".into()));
    }
    *PRODUCT_NAME.write().unwrap_or_else(|e| e.into_inner()) = Some(name.to_owned());
    Ok(())
}

/// The product name mails use now.
pub fn product() -> String {
    PRODUCT_NAME
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| DEFAULT_PRODUCT.to_owned())
}

fn compose(
    lang: Lang,
    subject_en: &str,
    subject_pt: &str,
    en: String,
    pt: String,
) -> (String, String) {
    let product = product();
    match lang {
        Lang::En => (format!("{product}: {subject_en}"), en),
        Lang::PtBr => (format!("{product}: {subject_pt}"), pt),
        Lang::Both => (
            format!("{product}: {subject_pt} / {subject_en}"),
            format!("{pt}\n\n----------------------------------------\n\n{en}"),
        ),
    }
}

/// The confirmation mail (CR-S53 follow-up "6-digit code"): the 6-digit `code` first, to type on
/// the launcher's confirmation screen; `link` second, the alternative page (valid 24 hours,
/// once, with the account's password).
pub fn confirm_mail(to: &str, code: &str, link: &str, lang: Lang) -> Message {
    let product = product();
    let shown = if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_owned()
    };
    let en = format!(
        "Your {product} confirmation code: {shown}\n\n\
         Type it in the launcher to confirm the e-mail address of your account (it works once, for \
         24 hours).\n\n\
         Or open this link and enter your account's password:\n{link}\n\n\
         If you did not create an account, ignore this e-mail: nothing happens without the code."
    );
    let pt = format!(
        "Seu código de confirmação {product}: {shown}\n\n\
         Digite-o no launcher para confirmar o endereço de e-mail da sua conta (vale uma vez, por \
         24 horas).\n\n\
         Ou abra este link e digite a senha da sua conta:\n{link}\n\n\
         Se você não criou uma conta, ignore este e-mail: nada acontece sem o código."
    );
    let (subject, text) = compose(
        lang,
        "your confirmation code",
        "seu código de confirmação",
        en,
        pt,
    );
    Message {
        html: templates::render(Kind::Confirm, Some(&shown), link, lang, &subject),
        kind: Kind::Confirm,
        to: to.to_owned(),
        subject,
        text,
    }
}

/// The password mail: `link` opens the instance's page that sets a new password (valid 1 hour,
/// once). Also the "set your password" mail of an account imported without one.
pub fn reset_mail(to: &str, link: &str, lang: Lang) -> Message {
    let product = product();
    let en = format!(
        "Someone asked to set a new password for the {product} account of this address.\n\n\
         Open this link to choose the password (it works once, for 1 hour):\n{link}\n\n\
         Setting it also confirms this address and signs the account out everywhere.\n\n\
         If you did not ask, ignore this e-mail: your password stays as it is."
    );
    let pt = format!(
        "Alguém pediu para definir uma nova senha para a conta {product} deste endereço.\n\n\
         Abra este link para escolher a senha (vale uma vez, por 1 hora):\n{link}\n\n\
         Definir a senha também confirma este endereço e desconecta a conta em todos os lugares.\n\n\
         Se você não pediu, ignore este e-mail: sua senha continua a mesma."
    );
    let (subject, text) = compose(lang, "set your password", "defina sua senha", en, pt);
    Message {
        html: templates::render(Kind::Reset, None, link, lang, &subject),
        kind: Kind::Reset,
        to: to.to_owned(),
        subject,
        text,
    }
}

/// An operator alert (English, plain text with an escaped HTML copy): `subject` and `text` carry
/// ids and numbers only, never player data.
pub fn alert_mail(to: &str, subject: &str, text: &str) -> Message {
    let product = product();
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Message {
        kind: Kind::Alert,
        to: to.to_owned(),
        subject: format!("[{product}] {subject}"),
        text: text.to_owned(),
        html: format!("<pre style=\"font-family:monospace\">{escaped}</pre>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alert_is_plain_and_escaped() {
        let m = alert_mail("ops@example.test", "VM v1", "7 of 8 <worlds>");
        assert_eq!(m.kind, Kind::Alert);
        assert_eq!(m.kind.as_str(), "alert");
        assert!(m.subject.ends_with("VM v1"));
        assert_eq!(m.text, "7 of 8 <worlds>");
        assert!(m.html.contains("7 of 8 &lt;worlds&gt;"));
    }

    const LINK: &str = "https://api.example.test/launcher/v1/auth/email/confirm?token=fi_mt_SECRET";

    #[test]
    fn languages_follow_accept_language() {
        assert_eq!(
            Lang::from_accept_language(Some("pt-BR,pt;q=0.9")),
            Lang::PtBr
        );
        assert_eq!(Lang::from_accept_language(Some("en-US")), Lang::En);
        assert_eq!(Lang::from_accept_language(Some("de, en;q=0.5")), Lang::En);
        assert_eq!(Lang::from_accept_language(Some("fr")), Lang::Both);
        assert_eq!(Lang::from_accept_language(None), Lang::Both);
        let both = confirm_mail("a@example.test", "123456", LINK, Lang::Both);
        assert!(both.text.contains("Seu código") && both.text.contains("Your Game Server"));
        assert!(
            both.text
                .starts_with("Seu código de confirmação Game Server: 123 456")
        );
        assert!(both.text.find("123 456").unwrap() < both.text.find(LINK).unwrap());
        assert_eq!(both.text.matches(LINK).count(), 2);
        let pt = reset_mail("a@example.test", LINK, Lang::PtBr);
        assert!(pt.subject.contains("defina sua senha") && !pt.text.contains("Someone"));
    }

    #[test]
    fn nothing_secret_reaches_debug_or_the_log_line() {
        let m = reset_mail("Ana@Example.TEST", LINK, Lang::En);
        let c = confirm_mail("Ana@Example.TEST", "987654", LINK, Lang::Both);
        for shown in [format!("{c:?}"), log_line(&c)] {
            assert!(!shown.contains("987") && !shown.contains("654"), "{shown}");
        }
        for shown in [format!("{m:?}"), log_line(&m)] {
            assert!(
                !shown.contains("fi_mt_") && !shown.contains("SECRET"),
                "{shown}"
            );
            assert!(!shown.to_lowercase().contains("example.test"), "{shown}");
        }
        assert_eq!(
            address_hash("ana@example.test"),
            address_hash(" Ana@Example.TEST")
        );
        assert_eq!(address_hash("ana@example.test").len(), 12);
        let c = SmtpConfig {
            host: "smtp.resend.com".into(),
            port: 465,
            user: "resend".into(),
            password: "re_SECRET".into(),
            from: "Example Game <no-reply@example.test>".into(),
        };
        assert!(!format!("{c:?}").contains("re_SECRET"));
    }

    #[test]
    fn html_templates_and_mime_preserve_actions_languages_and_plain_text() {
        for lang in [Lang::En, Lang::PtBr, Lang::Both] {
            for message in [
                confirm_mail("synthetic@example.test", "123456", LINK, lang),
                reset_mail("synthetic@example.test", LINK, lang),
            ] {
                assert!(message.html.contains("GAME") && message.html.contains("SERVER"));
                assert!(message.html.contains("prefers-color-scheme:dark"));
                assert!(!message.html.contains("<script") && !message.html.contains("<img"));
                assert!(!message.html.contains("{{"));
                assert!(message.html.len() < 30_000);
                assert!(message.html.contains(LINK));
                assert!(!format!("{message:?}").contains(LINK));
                match lang {
                    Lang::En => assert!(message.html.contains("<html lang=\"en\"")),
                    Lang::PtBr => assert!(message.html.contains("<html lang=\"pt-BR\"")),
                    Lang::Both => assert!(
                        message
                            .html
                            .find("Confirme")
                            .or_else(|| message.html.find("Defina"))
                            .unwrap()
                            < message
                                .html
                                .find("Confirm your")
                                .or_else(|| message.html.find("Set a new"))
                                .unwrap()
                    ),
                }
                if message.kind == Kind::Confirm {
                    assert!(message.html.contains("123 456"));
                    assert!(message.html.contains("24 hours") || message.html.contains("24 horas"));
                } else {
                    assert!(message.html.contains("1 hour") || message.html.contains("1 hora"));
                }
                let mail = smtp_message(
                    "Example Game <sender@example.test>".parse().unwrap(),
                    message,
                )
                .unwrap();
                let mime = String::from_utf8(mail.formatted()).unwrap();
                assert!(mime.contains("multipart/alternative"));
                assert!(mime.find("text/plain").unwrap() < mime.find("text/html").unwrap());
            }
        }
        // Arbitrary Unicode never panics while formatting a malformed six-byte code.
        assert!(
            confirm_mail("synthetic@example.test", "ééé", LINK, Lang::En)
                .html
                .contains("ééé")
        );
    }

    #[test]
    fn smtp_settings_are_checked() {
        let good = SmtpConfig {
            host: "smtp.resend.com".into(),
            port: 465,
            user: "resend".into(),
            password: "re_x".into(),
            from: "Example Game <no-reply@example.test>".into(),
        };
        assert!(Smtp::new(&good).is_ok());
        assert!(
            Smtp::new(&SmtpConfig {
                port: 587,
                ..good.clone()
            })
            .is_ok()
        );
        for (bad, name) in [
            (
                SmtpConfig {
                    port: 2525,
                    ..good.clone()
                },
                "FI_SMTP_PORT",
            ),
            (
                SmtpConfig {
                    password: String::new(),
                    ..good.clone()
                },
                "FI_SMTP_PASSWORD",
            ),
            (
                SmtpConfig {
                    from: "not an address".into(),
                    ..good.clone()
                },
                "FI_MAIL_FROM",
            ),
            (
                SmtpConfig {
                    host: " ".into(),
                    ..good.clone()
                },
                "FI_SMTP_HOST",
            ),
        ] {
            assert_eq!(Smtp::new(&bad).unwrap_err().name, name);
        }
        assert_eq!(security_of(465), Some(Security::ImplicitTls));
        assert_eq!(security_of(587), Some(Security::StartTls));
    }

    #[tokio::test]
    async fn the_fake_records_and_the_log_backend_delivers_nothing() {
        let r = Recording::new();
        r.send(confirm_mail("a@example.test", "123456", LINK, Lang::En))
            .await
            .unwrap();
        assert_eq!(r.sent_to("A@example.test").len(), 1);
        assert!(r.delivers() && !LogMailer.delivers());
        LogMailer
            .send(confirm_mail("a@example.test", "123456", LINK, Lang::En))
            .await
            .unwrap();
    }
}
