// SMTP mailer primitive over `lettre`.
//
// No permission enforcement here. Scripting/context layer gates by
// `net:<host>` before calling `send`. A `Mailer` is a plain object like an
// HTTP client: it holds SMTP config + a pooled async transport and sends mail.

use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::message::{Attachment as LettreAttachment, Mailbox, MultiPart, SinglePart};
use lettre::message::header::MessageId as MessageIdHeader;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};
use thiserror::Error;

/// Transport security for an SMTP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Security {
    /// Plaintext upgraded to TLS via STARTTLS (default submission, port 587).
    #[default]
    StartTls,
    /// Implicit TLS from connect (SMTPS, port 465).
    Tls,
    /// No TLS at all. `builder_dangerous` — dev/test only.
    Plaintext,
}

/// SMTP connection + default-From configuration.
#[derive(Debug, Clone, Default)]
pub struct MailerConfig {
    pub host: String,
    /// Defaults per `security`: 465 (Tls), 587 (StartTls), 25 (Plaintext).
    pub port: Option<u16>,
    pub user: Option<String>,
    pub pass: Option<String>,
    /// Default `From` mailbox (required).
    pub from: String,
    pub security: Security,
    /// Per-operation timeout in milliseconds. Default 30_000.
    pub timeout_ms: Option<u64>,
}

/// A file attachment.
#[derive(Debug, Clone, Default)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A single outgoing message.
#[derive(Debug, Clone, Default)]
pub struct Mail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    /// Override `MailerConfig.from` for this message.
    pub from: Option<String>,
    pub reply_to: Option<String>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
}

/// Result of a successful send.
#[derive(Debug, Clone, Default)]
pub struct SendOutcome {
    pub message_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum MailerError {
    #[error("config: {0}")]
    Config(String),
    #[error("address: {0}")]
    Address(String),
    #[error("build: {0}")]
    Build(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("send: {0}")]
    Send(String),
}

/// Pooled async SMTP transport + default From mailbox. Clone is cheap
/// (lettre's `AsyncSmtpTransport` shares its connection pool), so an
/// `Arc<Mailer>` is unnecessary but harmless.
#[derive(Clone)]
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    host: String,
}

fn parse_mailbox(s: &str) -> Result<Mailbox, MailerError> {
    s.parse::<Mailbox>()
        .map_err(|e| MailerError::Address(format!("`{s}`: {e}")))
}

impl Mailer {
    pub fn connect(cfg: MailerConfig) -> Result<Self, MailerError> {
        if cfg.host.is_empty() {
            return Err(MailerError::Config("host is empty".into()));
        }
        if cfg.from.is_empty() {
            return Err(MailerError::Config("from is empty".into()));
        }
        let from = parse_mailbox(&cfg.from)?;

        let mut builder = match cfg.security {
            Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
                .map_err(|e| MailerError::Config(e.to_string()))?,
            Security::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
                .map_err(|e| MailerError::Config(e.to_string()))?,
            Security::Plaintext => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host)
            }
        };

        if let Some(port) = cfg.port {
            builder = builder.port(port);
        }
        if let (Some(user), Some(pass)) = (cfg.user.as_ref(), cfg.pass.as_ref()) {
            builder = builder.credentials(Credentials::new(user.clone(), pass.clone()));
        }
        let timeout = Duration::from_millis(cfg.timeout_ms.unwrap_or(30_000));
        builder = builder.timeout(Some(timeout));

        let transport = builder.build();
        Ok(Self {
            transport,
            from,
            host: cfg.host,
        })
    }

    /// Config host, for deriving the `net:<host>` permission slug.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub async fn send(&self, mail: Mail) -> Result<SendOutcome, MailerError> {
        let message = self.build_message(mail)?;
        let message_id = message
            .headers()
            .get::<MessageIdHeader>()
            .map(|m| m.as_ref().to_string());

        self.transport
            .send(message)
            .await
            .map_err(|e| MailerError::Send(e.to_string()))?;

        Ok(SendOutcome { message_id })
    }

    fn build_message(&self, mail: Mail) -> Result<Message, MailerError> {
        let from = match &mail.from {
            Some(f) => parse_mailbox(f)?,
            None => self.from.clone(),
        };

        // Force an auto-generated Message-ID so callers get a stable handle.
        let mut builder = Message::builder().from(from).message_id(None);

        for addr in &mail.to {
            builder = builder.to(parse_mailbox(addr)?);
        }
        for addr in &mail.cc {
            builder = builder.cc(parse_mailbox(addr)?);
        }
        for addr in &mail.bcc {
            builder = builder.bcc(parse_mailbox(addr)?);
        }
        if let Some(rt) = &mail.reply_to {
            builder = builder.reply_to(parse_mailbox(rt)?);
        }
        builder = builder.subject(&mail.subject);

        let body = self.assemble_body(&mail)?;

        builder
            .multipart(body)
            .map_err(|e| MailerError::Build(e.to_string()))
    }

    /// Assemble the MIME body. The chosen text/html body is always wrapped in a
    /// `MultiPart` so the builder path is uniform; attachments add a
    /// `multipart/mixed` outer layer.
    fn assemble_body(&self, mail: &Mail) -> Result<MultiPart, MailerError> {
        let inner: MultiPart = match (&mail.text, &mail.html) {
            (Some(text), Some(html)) => MultiPart::alternative()
                .singlepart(SinglePart::plain(text.clone()))
                .singlepart(SinglePart::html(html.clone())),
            (Some(text), None) => {
                MultiPart::mixed().singlepart(SinglePart::plain(text.clone()))
            }
            (None, Some(html)) => {
                MultiPart::mixed().singlepart(SinglePart::html(html.clone()))
            }
            (None, None) => {
                return Err(MailerError::Build(
                    "mail has neither text nor html body".into(),
                ));
            }
        };

        if mail.attachments.is_empty() {
            return Ok(inner);
        }

        let mut mixed = MultiPart::mixed().multipart(inner);
        for att in &mail.attachments {
            let ct = att
                .content_type
                .parse::<ContentType>()
                .map_err(|e| MailerError::Build(format!("attachment content-type: {e}")))?;
            mixed = mixed.singlepart(
                LettreAttachment::new(att.filename.clone()).body(att.bytes.clone(), ct),
            );
        }
        Ok(mixed)
    }
}

#[cfg(test)]
mod tests {
    //! Tests against a minimal local SMTP mock (Plaintext, no TLS) — just
    //! enough of the protocol to satisfy lettre's client and capture the
    //! transmitted message.

    use std::sync::Arc;

    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// Captured from one SMTP session.
    #[derive(Debug, Default, Clone)]
    struct Captured {
        rcpts: Vec<String>,
        data: String,
    }

    /// Result of a single mock SMTP session: either captured data, or a flag
    /// that the session was rejected at RCPT.
    /// `reject_rcpt` makes the server answer `550` to every RCPT TO.
    async fn serve_session(
        stream: tokio::net::TcpStream,
        reject_rcpt: bool,
    ) -> Captured {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut captured = Captured::default();

        write_half.write_all(b"220 mock ESMTP\r\n").await.unwrap();

        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.unwrap();
            if n == 0 {
                break;
            }
            let upper = line.to_uppercase();
            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                write_half
                    .write_all(b"250-mock\r\n250 OK\r\n")
                    .await
                    .unwrap();
            } else if upper.starts_with("MAIL FROM") {
                write_half.write_all(b"250 OK\r\n").await.unwrap();
            } else if upper.starts_with("RCPT TO") {
                let addr = line.trim().to_string();
                captured.rcpts.push(addr);
                if reject_rcpt {
                    write_half
                        .write_all(b"550 No such user\r\n")
                        .await
                        .unwrap();
                } else {
                    write_half.write_all(b"250 OK\r\n").await.unwrap();
                }
            } else if upper.starts_with("DATA") {
                write_half
                    .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                    .await
                    .unwrap();
                // Read until a line that is exactly ".".
                let mut body = String::new();
                loop {
                    let mut dl = String::new();
                    let dn = reader.read_line(&mut dl).await.unwrap();
                    if dn == 0 {
                        break;
                    }
                    if dl == ".\r\n" || dl == ".\n" {
                        break;
                    }
                    body.push_str(&dl);
                }
                captured.data = body;
                write_half
                    .write_all(b"250 Ok: queued as MOCK123\r\n")
                    .await
                    .unwrap();
            } else if upper.starts_with("QUIT") {
                write_half.write_all(b"221 Bye\r\n").await.unwrap();
                break;
            } else {
                write_half.write_all(b"250 OK\r\n").await.unwrap();
            }
            // As soon as we've captured a full message, we have everything the
            // test needs. Returning here (rather than waiting for QUIT) avoids
            // blocking on lettre's pool keep-alive, which otherwise holds the
            // connection open until the transport timeout.
            if !captured.data.is_empty() {
                break;
            }
        }
        captured
    }

    /// Bind a one-shot mock SMTP server; returns the bound port and a receiver
    /// that yields the captured session.
    async fn mock_smtp() -> (u16, oneshot::Receiver<Captured>) {
        mock_smtp_inner(false).await
    }

    async fn mock_smtp_inner(reject_rcpt: bool) -> (u16, oneshot::Receiver<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let captured = serve_session(stream, reject_rcpt).await;
            let _ = tx.send(captured);
        });
        (port, rx)
    }

    /// Mock that accepts N sequential sessions, returning all captures.
    async fn mock_smtp_n(n: usize) -> (u16, oneshot::Receiver<Vec<Captured>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut all = Vec::new();
            for _ in 0..n {
                let (stream, _) = listener.accept().await.unwrap();
                all.push(serve_session(stream, false).await);
            }
            let _ = tx.send(all);
        });
        (port, rx)
    }

    fn plaintext_mailer(port: u16) -> Mailer {
        Mailer::connect(MailerConfig {
            host: "127.0.0.1".into(),
            port: Some(port),
            from: "a@b.c".into(),
            security: Security::Plaintext,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn send_text_only() {
        let (port, rx) = mock_smtp().await;
        let m = plaintext_mailer(port);
        let outcome = m
            .send(Mail {
                to: vec!["x@y.z".into()],
                subject: "hi".into(),
                text: Some("body".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let cap = rx.await.unwrap();
        assert!(cap.data.contains("Subject: hi"), "data: {}", cap.data);
        assert!(cap.data.contains("body"), "data: {}", cap.data);
        assert!(
            cap.rcpts.iter().any(|r| r.contains("<x@y.z>")),
            "rcpts: {:?}",
            cap.rcpts
        );
        assert!(outcome.message_id.is_some());
    }

    #[tokio::test]
    async fn send_html_only() {
        let (port, rx) = mock_smtp().await;
        let m = plaintext_mailer(port);
        m.send(Mail {
            to: vec!["x@y.z".into()],
            subject: "h".into(),
            html: Some("<b>hi</b>".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let cap = rx.await.unwrap();
        assert!(cap.data.contains("text/html"), "data: {}", cap.data);
        assert!(cap.data.contains("<b>hi</b>"), "data: {}", cap.data);
    }

    #[tokio::test]
    async fn send_multipart_alternative() {
        let (port, rx) = mock_smtp().await;
        let m = plaintext_mailer(port);
        m.send(Mail {
            to: vec!["x@y.z".into()],
            subject: "m".into(),
            text: Some("plain body".into()),
            html: Some("<b>rich</b>".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let cap = rx.await.unwrap();
        assert!(
            cap.data.contains("multipart/alternative"),
            "data: {}",
            cap.data
        );
        assert!(cap.data.contains("plain body"));
        assert!(cap.data.contains("<b>rich</b>"));
    }

    #[tokio::test]
    async fn send_with_attachment() {
        let (port, rx) = mock_smtp().await;
        let m = plaintext_mailer(port);
        m.send(Mail {
            to: vec!["x@y.z".into()],
            subject: "att".into(),
            text: Some("see attached".into()),
            attachments: vec![Attachment {
                filename: "hello.txt".into(),
                content_type: "text/plain".into(),
                bytes: b"file contents".to_vec(),
            }],
            ..Default::default()
        })
        .await
        .unwrap();
        let cap = rx.await.unwrap();
        assert!(cap.data.contains("multipart/mixed"), "data: {}", cap.data);
        assert!(cap.data.contains("hello.txt"), "data: {}", cap.data);
        assert!(cap.data.contains("text/plain"), "data: {}", cap.data);
    }

    #[tokio::test]
    async fn bcc_not_in_headers() {
        let (port, rx) = mock_smtp().await;
        let m = plaintext_mailer(port);
        m.send(Mail {
            to: vec!["x@y.z".into()],
            bcc: vec!["secret@hidden.z".into()],
            subject: "b".into(),
            text: Some("body".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let cap = rx.await.unwrap();
        // bcc must be an SMTP RCPT...
        assert!(
            cap.rcpts.iter().any(|r| r.contains("<secret@hidden.z>")),
            "rcpts: {:?}",
            cap.rcpts
        );
        // ...but never in the header block.
        assert!(
            !cap.data.contains("secret@hidden.z"),
            "bcc leaked into headers: {}",
            cap.data
        );
    }

    #[tokio::test]
    async fn from_override() {
        let (port, rx) = mock_smtp().await;
        let m = plaintext_mailer(port);
        m.send(Mail {
            to: vec!["x@y.z".into()],
            from: Some("c@d.e".into()),
            subject: "f".into(),
            text: Some("body".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let cap = rx.await.unwrap();
        assert!(cap.data.contains("From: c@d.e"), "data: {}", cap.data);
        assert!(!cap.data.contains("From: a@b.c"), "data: {}", cap.data);
    }

    #[tokio::test]
    async fn send_failure_maps_error() {
        let (port, rx) = mock_smtp_inner(true).await;
        let m = plaintext_mailer(port);
        let err = m
            .send(Mail {
                to: vec!["x@y.z".into()],
                subject: "x".into(),
                text: Some("body".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, MailerError::Send(_)), "got {err:?}");
        let _ = rx.await;
    }

    #[tokio::test]
    async fn no_body_is_build_error() {
        // No transport contact needed; build fails first.
        let m = Mailer::connect(MailerConfig {
            host: "127.0.0.1".into(),
            port: Some(2525),
            from: "a@b.c".into(),
            security: Security::Plaintext,
            ..Default::default()
        })
        .unwrap();
        let err = m
            .send(Mail {
                to: vec!["x@y.z".into()],
                subject: "x".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, MailerError::Build(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn concurrent_sends_share_arc() {
        let (port, rx) = mock_smtp_n(2).await;
        let m = Arc::new(plaintext_mailer(port));
        let m1 = m.clone();
        let m2 = m.clone();
        let f1 = m1.send(Mail {
            to: vec!["one@y.z".into()],
            subject: "one".into(),
            text: Some("first".into()),
            ..Default::default()
        });
        let f2 = m2.send(Mail {
            to: vec!["two@y.z".into()],
            subject: "two".into(),
            text: Some("second".into()),
            ..Default::default()
        });
        let (r1, r2) = tokio::join!(f1, f2);
        r1.unwrap();
        r2.unwrap();
        let caps = rx.await.unwrap();
        assert_eq!(caps.len(), 2);
    }
}
