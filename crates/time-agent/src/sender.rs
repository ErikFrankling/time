//! Delivery of spooled events to an OTLP/HTTP endpoint.

use std::time::Duration;

use anyhow::Result;
use time_core::event::Envelope;
use time_core::otlp;

#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The receiver has the batch (some records may have been rejected as
    /// malformed; resending would not change that).
    Accepted,
    /// Try again later: network down, server busy, 5xx, 429.
    Retry(String),
    /// The receiver will never take this batch as sent: drop it, loudly.
    Refused(String),
}

pub trait Sender: Send {
    fn send(&mut self, batch: &[Envelope]) -> Delivery;
}

pub struct HttpSender {
    endpoint: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
}

impl HttpSender {
    pub fn new(endpoint: &str, token: Option<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            token,
            client,
        })
    }
}

impl Sender for HttpSender {
    fn send(&mut self, batch: &[Envelope]) -> Delivery {
        let body = otlp::encode(batch, env!("CARGO_PKG_VERSION"));
        let mut req = self.client.post(&self.endpoint).json(&body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        match req.send() {
            Err(e) => Delivery::Retry(e.to_string()),
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    Delivery::Accepted
                } else if status.as_u16() == 429
                    || status.is_server_error()
                    || status.as_u16() == 401
                {
                    // 401: a token being rotated should not cost data.
                    Delivery::Retry(format!("{status}: {}", resp.text().unwrap_or_default()))
                } else {
                    Delivery::Refused(format!("{status}: {}", resp.text().unwrap_or_default()))
                }
            }
        }
    }
}
