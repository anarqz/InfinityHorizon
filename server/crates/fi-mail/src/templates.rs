//! Image-independent, table-based HTML presentation of existing account actions.
use crate::{Kind, Lang};

struct Copy {
    lang: &'static str,
    eyebrow: &'static str,
    headline: &'static str,
    intro: String,
    code_label: &'static str,
    expiry: &'static str,
    alternative: &'static str,
    cta: &'static str,
    consequence: &'static str,
    ignore: &'static str,
    fallback: &'static str,
}

fn copy(kind: Kind, pt: bool, product: &str) -> Copy {
    let product = escape(product);
    match (kind, pt) {
        (Kind::Confirm, false) => Copy {
            lang: "en",
            eyebrow: "Your account. Your starting line.",
            headline: "Confirm your email.",
            intro: format!(
                "Welcome to {product}. Enter this code in the launcher to confirm your email address and take the next step."
            ),
            code_label: "Your confirmation code",
            expiry: "Single use &middot; Valid for 24 hours",
            alternative: "Prefer to use your browser? Open the confirmation page and enter your account password.",
            cta: "Confirm email in browser",
            consequence: "",
            ignore: "Didn't create an account? You can ignore this email. Your address won't be confirmed without the code or the confirmation step.",
            fallback: "If the button doesn't work, copy this link into your browser:",
        },
        (Kind::Confirm, true) => Copy {
            lang: "pt-BR",
            eyebrow: "Sua conta. Seu ponto de partida.",
            headline: "Confirme seu e-mail.",
            intro: format!(
                "Boas-vindas ao {product}. Digite este código no launcher para confirmar seu endereço de e-mail e dar o próximo passo."
            ),
            code_label: "Seu código de confirmação",
            expiry: "Uso único &middot; Válido por 24 horas",
            alternative: "Prefere usar o navegador? Abra a página de confirmação e digite a senha da sua conta.",
            cta: "Confirmar e-mail no navegador",
            consequence: "",
            ignore: "Não criou uma conta? Pode ignorar este e-mail. Seu endereço não será confirmado sem o código ou a etapa de confirmação.",
            fallback: "Se o botão não funcionar, copie este link no navegador:",
        },
        (Kind::Reset, false) => Copy {
            lang: "en",
            eyebrow: "Back in your hands.",
            headline: "Set a new password.",
            intro: format!(
                "We received a request to set a new password for your {product} account. Use the button below to choose your new password."
            ),
            code_label: "",
            expiry: "Single use &middot; Valid for 1 hour",
            alternative: "",
            cta: "Choose a new password",
            consequence: "Setting a new password also confirms this email address and signs your account out everywhere. Sign back in with your new password.",
            ignore: "Didn't request this? You can ignore this email. Your password stays unchanged unless you complete the reset.",
            fallback: "If the button doesn't work, copy this link into your browser:",
        },
        (Kind::Reset, true) => Copy {
            lang: "pt-BR",
            eyebrow: "O controle volta para você.",
            headline: "Defina uma nova senha.",
            intro: format!(
                "Recebemos um pedido para definir uma nova senha para sua conta {product}. Use o botão abaixo para escolher sua nova senha."
            ),
            code_label: "",
            expiry: "Uso único &middot; Válido por 1 hora",
            alternative: "",
            cta: "Escolher uma nova senha",
            consequence: "Definir uma nova senha também confirma este e-mail e desconecta sua conta em todos os lugares. Entre novamente com a nova senha.",
            ignore: "Não fez este pedido? Pode ignorar este e-mail. Sua senha só muda se você concluir a redefinição.",
            fallback: "Se o botão não funcionar, copie este link no navegador:",
        },
        // An operator alert is plain text (`crate::alert_mail`) and never rendered here.
        (Kind::Alert, _) => unreachable!("an alert has no account template"),
    }
}

/// Escape every dynamic value for text and quoted HTML attribute contexts.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn section(kind: Kind, code: Option<&str>, link: &str, pt: bool, product: &str) -> String {
    let c = copy(kind, pt, product);
    let lang = c.lang;
    // Origin validation is owned by fi-surfaces. Also prevent active non-web schemes in HTML.
    let url = if (link.starts_with("https://") || link.starts_with("http://"))
        && !link.chars().any(char::is_control)
    {
        escape(link)
    } else {
        String::new()
    };
    let code_box = code.map_or_else(String::new, |code| format!(r##"
<tr lang="{lang}"><td class="pad" style="padding:0 40px 28px;">
<table role="presentation" width="100%"><tr><td align="center" class="codebox" style="padding:22px 12px;background-color:#fff3ea;border:1px solid #f3d7c2;border-radius:12px;">
<p class="muted" style="margin:0 0 12px;color:#66617b;font-size:12px;font-weight:700;">{label}</p>
<p class="code" dir="ltr" style="margin:0;color:#8c340e;font-family:Consolas,Menlo,'Courier New',monospace;font-size:44px;line-height:1.25;font-weight:700;letter-spacing:7px;">{code}</p>
<p class="muted" style="margin:12px 0 0;color:#66617b;font-size:12px;line-height:1.5;">{expiry}</p>
</td></tr></table></td></tr>"##, label=c.code_label, code=escape(code), expiry=c.expiry));
    let alternative = if c.alternative.is_empty() {
        String::new()
    } else {
        format!(
            r##"<p class="muted" style="margin:0 0 18px;color:#66617b;font-size:14px;line-height:1.7;">{}</p>"##,
            c.alternative
        )
    };
    let expiry = if code.is_some() {
        String::new()
    } else {
        format!(
            r##"<p class="muted" style="margin:16px 0 0;color:#66617b;font-size:12px;line-height:1.5;">{}</p>"##,
            c.expiry
        )
    };
    let consequence = if c.consequence.is_empty() {
        String::new()
    } else {
        format!(
            r##"<p class="ink" style="margin:22px 0 0;color:#19182b;font-size:14px;line-height:1.7;">{}</p>"##,
            c.consequence
        )
    };
    let action = if url.is_empty() {
        String::new()
    } else {
        format!(
            r##"
<table role="presentation"><tr><td bgcolor="#ff744d" style="background-color:#ff744d;border-radius:8px;mso-padding-alt:16px 24px;">
<a class="button" href="{url}" style="display:inline-block;padding:16px 24px;color:#231017;font-size:15px;line-height:20px;font-weight:700;text-decoration:none;border:1px solid #ff744d;border-radius:8px;">{cta}</a>
</td></tr></table>"##,
            cta = c.cta
        )
    };
    let fallback = if url.is_empty() {
        String::new()
    } else {
        format!(
            r##"
<tr lang="{lang}"><td class="pad" style="padding:0 40px 30px;">
<p class="muted" style="margin:0 0 6px;color:#66617b;font-size:11px;line-height:1.6;">{fallback}</p>
<a class="actionlink" href="{url}" style="color:#a54414;font-size:11px;line-height:1.7;word-break:break-all;overflow-wrap:anywhere;">{url}</a>
</td></tr>"##,
            fallback = c.fallback
        )
    };
    format!(
        r##"
<tr><td class="pad" lang="{lang}" style="padding:36px 40px 24px;">
<p class="actionlink" style="margin:0 0 14px;color:#a54414;font-size:12px;font-weight:700;letter-spacing:.4px;">{eyebrow}</p>
<h1 class="headline ink" style="margin:0 0 18px;color:#19182b;font-family:Arial,Helvetica,sans-serif;font-size:38px;font-weight:800;letter-spacing:-1.2px;line-height:1.1;">{headline}</h1>
<p class="ink" style="margin:0;color:#353149;font-size:16px;line-height:1.75;">{intro}</p>
</td></tr>
{code_box}
<tr><td class="pad" lang="{lang}" style="padding:0 40px 28px;">{alternative}{action}{expiry}{consequence}</td></tr>
<tr><td class="pad" lang="{lang}" style="padding:0 40px 24px;">
<table role="presentation" width="100%"><tr><td class="notice" style="padding:16px 18px;background-color:#f4f3f8;border-radius:8px;"><p class="muted" style="margin:0;color:#66617b;font-size:12px;line-height:1.75;">{ignore}</p></td></tr></table>
</td></tr>
{fallback}"##,
        lang = c.lang,
        eyebrow = c.eyebrow,
        headline = c.headline,
        intro = c.intro,
        ignore = c.ignore
    )
}

/// "Infinity Horizon" -> `INFINITY <span>HORIZON</span>`: the last word takes the accent colour.
fn wordmark(product: &str) -> String {
    let upper = escape(&product.to_uppercase());
    match upper.rsplit_once(' ') {
        Some((head, last)) => format!("{head} <span style=\"color:#ff9a4d;\">{last}</span>"),
        None => format!("<span style=\"color:#ff9a4d;\">{upper}</span>"),
    }
}

pub(crate) fn render(
    kind: Kind,
    code: Option<&str>,
    link: &str,
    lang: Lang,
    subject: &str,
) -> String {
    let product = crate::product();
    let content = match lang {
        Lang::En => section(kind, code, link, false, &product),
        Lang::PtBr => section(kind, code, link, true, &product),
        Lang::Both => format!(
            "{}<tr><td class=\"rule\" style=\"border-top:1px solid #e7e4ef;\"></td></tr>{}",
            section(kind, code, link, true, &product),
            section(kind, code, link, false, &product)
        ),
    };
    let pt = lang != Lang::En;
    let preheader = match (kind, pt) {
        (Kind::Confirm, false) => "Your confirmation code is inside. The next step is yours.",
        (Kind::Confirm, true) => "Seu código de confirmação está aqui. O próximo passo é seu.",
        (Kind::Reset, false) => "Choose a new password securely. This link is valid for 1 hour.",
        (Kind::Reset, true) => "Escolha uma nova senha com segurança. Este link vale por 1 hora.",
        (Kind::Alert, _) => unreachable!("an alert has no account template"),
    };
    include_str!("../templates/shell.html")
        .replace("{{wordmark}}", &wordmark(&product))
        .replace("{{product}}", &escape(&product))
        .replace("{{language}}", if pt { "pt-BR" } else { "en" })
        .replace("{{subject}}", &escape(subject))
        .replace("{{preheader}}", preheader)
        .replace("{{footer}}", if pt { "Este é um e-mail automático sobre sua conta." } else { "This is an automated email about your account." })
        .replace("{{security_footer}}", if pt { "Não compartilhe códigos ou links de acesso. Nossa equipe nunca pedirá sua senha por e-mail." } else { "Keep access codes and links private. Our team will never ask for your password by email." })
        .replace("{{content}}", &content)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_wordmark_accents_the_last_word() {
        assert_eq!(
            wordmark("Infinity Horizon"),
            "INFINITY <span style=\"color:#ff9a4d;\">HORIZON</span>"
        );
        assert_eq!(
            wordmark("a<b"),
            "<span style=\"color:#ff9a4d;\">A&lt;B</span>"
        );
    }
    #[test]
    fn dynamic_values_are_escaped_and_active_schemes_have_no_button() {
        let html = render(
            Kind::Confirm,
            Some("<img>"),
            "https://example.test/?a=1&b=\"<tag>",
            Lang::En,
            "<subject>",
        );
        assert!(!html.contains("<img>"));
        assert!(html.contains("&lt;img&gt;"));
        assert!(html.contains("a=1&amp;b=&quot;&lt;tag&gt;"));
        assert!(html.contains("<title>&lt;subject&gt;</title>"));
        for url in [
            "javascript:alert(1)",
            "data:text/html,x",
            "https://example.test/\ninvalid",
        ] {
            let html = render(Kind::Reset, None, url, Lang::En, "Reset");
            assert!(!html.contains("href="));
        }
    }
}
