# Infinity Horizon transactional email

`confirm_mail` and `reset_mail` produce a subject plus plain-text and HTML alternatives.
`Smtp` sends them as `multipart/alternative`, plain text first. Existing callers, delivery
configuration, authentication actions, code/link expiry and asynchronous dispatch are preserved.
No email is sent by building, testing or previewing this crate.

## Design

The shared shell is `templates/shell.html`; localized content is in `src/templates.rs`.
The Infinity Horizon wordmark is live text, with a midnight header, sunset stripe and coral
buttons. Confirmation puts the six-digit launcher code first and the browser alternative second.
Password reset explains its one-hour expiry, email verification and account-wide sign-out.
Security notes and a copyable fallback link are included in both.

English, pt-BR and Portuguese-first bilingual variants follow the existing `Lang` selection.
Dark and light styles, responsive padding, inline base styles, presentation tables and an Outlook
fixed-width wrapper are included. No image, web font, JavaScript, tracking pixel or new dependency
is required. Dynamic codes, subjects and URLs are HTML-escaped; HTML actions permit HTTP(S) only.
`Message::Debug` and the log backend still omit the recipient, code, URL, text and HTML bodies.

The HTML field is internal Rust state, not a new HTTP/wire DTO. No protocol mirror, route,
migration, sender address or SMTP/provider setting changes.

## Local previews

```sh
export PATH=$HOME/.cargo/bin:$HOME/.local/bin:$PATH
cargo run -p fi-mail --example preview -- /tmp/fi-mail-preview
python3 -m http.server 3142 --bind 127.0.0.1 --directory /tmp/fi-mail-preview
```

Open `http://127.0.0.1:3142/`. The example writes six HTML emails, their plain-text alternatives
and an index, using only `example.invalid` links and an invented code. It reads no credentials,
opens no SMTP connection and touches no database. Output stays outside the repository.
Use the browser's light/dark preference for theme previews.

## Checks and release boundary

```sh
cargo fmt -p fi-mail --check
cargo test -p fi-mail
cargo clippy -p fi-mail --all-targets -- -D warnings
cargo check -p fi-surfaces --tests
```

Local browser checks cover all six variants at 390px and 900px in both color schemes, no page
horizontal overflow and no external requests (24 combinations). These are browser layout checks,
not Gmail, Apple Mail or Outlook inbox acceptance. Before a production release, inspect actual
inbox delivery in those clients, including images disabled, dark mode and the plain-text view,
using separately authorized test recipients. No recipient message or production deployment is
part of this preparation.

Compatibility fallbacks intentionally remain simple: unsupported dark-mode CSS keeps the inline
light design, unsupported rounding keeps rectangular panels, and unsupported CSS buttons retain
a normal actionable text link. The code and both action paths stay readable without assets.
