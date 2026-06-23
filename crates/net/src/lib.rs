//! Networking primitives for agent.d: HTTP client, WebSocket client, SMTP
//! mailer. Pure transport — no permission enforcement. The scripting layer
//! gates each on `net:<host>`.

pub mod http;
pub mod mailer;
pub mod ws;
