//! Outgoing mail over SMTP, with the settings on Cluster → Email. Budget alerts are the only mail
//! the gateway sends (see `alerts`).

use std::time::Duration;

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
    transport::smtp::authentication::Credentials,
};

use crate::{db, state::AppState};

/// How the connection to the mail server is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// A plain connection upgraded with STARTTLS, which the server must offer (usually port 587).
    Starttls,
    /// TLS from the start (usually port 465).
    Tls,
    /// No encryption: only for a relay on this machine or a trusted network.
    None,
}

impl SmtpSecurity {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "starttls" => Some(Self::Starttls),
            "tls" => Some(Self::Tls),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starttls => "starttls",
            Self::Tls => "tls",
            Self::None => "none",
        }
    }
}

/// A message: its subject, and its body as HTML and as plain text for clients that show no HTML.
pub struct Mail {
    pub subject: String,
    pub html: String,
    pub text: String,
}

/// The saved, enabled mail settings, ready to send with.
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    /// Where the console is, for links in messages.
    pub console_url: Option<String>,
}

/// How long connecting to the mail server and each step of sending may take.
const TIMEOUT: Duration = Duration::from_secs(20);

impl Mailer {
    /// The mailer for the saved settings: `None` when mail is not set up or switched off.
    pub async fn load(state: &AppState) -> Result<Option<Self>, String> {
        match db::get_smtp_settings(&state.db).await.map_err(|e| e.to_string())? {
            Some(row) if row.enabled => Self::new(state, &row).map(Some),
            _ => Ok(None),
        }
    }

    /// A mailer for these settings, whether or not they are enabled.
    pub fn new(state: &AppState, row: &db::SmtpSettingsRow) -> Result<Self, String> {
        let security = SmtpSecurity::parse(&row.security).ok_or("unknown SMTP security")?;
        let host = row.host.as_str();
        let builder = match security {
            SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| e.to_string())?,
            SmtpSecurity::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| e.to_string())?,
            SmtpSecurity::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        };
        let port = u16::try_from(row.port).map_err(|_| "invalid SMTP port")?;
        let mut builder = builder.port(port).timeout(Some(TIMEOUT));
        if let Some(user) = row.username.as_deref().filter(|u| !u.is_empty()) {
            let password = match &row.password {
                Some(sealed) => String::from_utf8(state.cfg.secret_box.open(sealed)?).map_err(|_| "the SMTP password is not text")?,
                None => String::new(),
            };
            builder = builder.credentials(Credentials::new(user.to_string(), password));
        }
        let from = mailbox(&row.from_address, row.from_name.as_deref())?;
        Ok(Self { transport: builder.build(), from, console_url: row.console_url.clone() })
    }

    /// Sends `mail` to `to` (addresses, with optional names) in one message.
    pub async fn send(&self, to: &[(String, Option<String>)], mail: &Mail) -> Result<(), String> {
        if to.is_empty() {
            return Err("no recipients".into());
        }
        let mut message = Message::builder().from(self.from.clone()).subject(&mail.subject);
        for (address, name) in to {
            message = message.to(mailbox(address, name.as_deref())?);
        }
        let message = message
            .multipart(MultiPart::alternative_plain_html(mail.text.clone(), mail.html.clone()))
            .map_err(|e| format!("could not build the message: {e}"))?;
        self.transport.send(message).await.map(|_| ()).map_err(|e| describe(&e))
    }
}

/// An address with an optional display name.
fn mailbox(address: &str, name: Option<&str>) -> Result<Mailbox, String> {
    let address = address.trim().parse().map_err(|_| format!("{address} is not an email address"))?;
    Ok(Mailbox::new(name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string), address))
}

/// Whether `s` is an email address the mailer accepts.
pub fn valid_address(s: &str) -> bool {
    s.trim().parse::<lettre::Address>().is_ok()
}

/// The mail server's error with its cause, which says what went wrong (refused, TLS, auth).
fn describe(e: &lettre::transport::smtp::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

/// Text made safe to put in HTML.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_names() {
        assert!(valid_address("ops@example.com"));
        assert!(!valid_address("not an address"));
        let m = mailbox("ops@example.com", Some("Gateway")).unwrap();
        assert_eq!(m.to_string(), "Gateway <ops@example.com>");
        assert_eq!(mailbox("ops@example.com", Some(" ")).unwrap().to_string(), "ops@example.com");
    }

    #[test]
    fn escapes_html() {
        assert_eq!(escape(r#"<b>"a" & 'b'</b>"#), "&lt;b&gt;&quot;a&quot; &amp; &#39;b&#39;&lt;/b&gt;");
    }
}
