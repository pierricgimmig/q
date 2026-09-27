//! Blocking HTTP client for Orbit's `POST /api/events` and `GET /api/status`.

use std::time::Duration;

use serde::Serialize;

use crate::wire::{EventsBody, EventsSummary};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a request to Orbit failed. The bridge retries every variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrbitError {
    pub message: String,
}

impl std::fmt::Display for OrbitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OrbitError {}

pub struct OrbitClient {
    base: String,
    agent: ureq::Agent,
}

impl OrbitClient {
    /// `url` is the service root, for example `http://127.0.0.1:44766`. A
    /// trailing slash is removed; a missing scheme is an error.
    pub fn new(url: &str) -> Result<Self, OrbitError> {
        let base = url.trim().trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(OrbitError {
                message: format!("Orbit URL must start with http:// or https://: {url}"),
            });
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .build();
        Ok(Self { base, agent })
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    /// `GET /api/status`: true when the service answers with JSON.
    pub fn status(&self) -> Result<serde_json::Value, OrbitError> {
        let url = format!("{}/api/status", self.base);
        let response = self.agent.get(&url).call().map_err(|err| self.map(err))?;
        response.into_json().map_err(|err| OrbitError {
            message: format!("malformed /api/status reply from {}: {err}", self.base),
        })
    }

    /// `POST /api/events` with one batch.
    pub fn post_events(&self, body: &EventsBody) -> Result<EventsSummary, OrbitError> {
        self.post_json("/api/events", body)
    }

    fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, OrbitError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .post(&url)
            .send_json(body)
            .map_err(|err| self.map(err))?;
        response.into_json().map_err(|err| OrbitError {
            message: format!("malformed reply from {url}: {err}"),
        })
    }

    fn map(&self, error: ureq::Error) -> OrbitError {
        match error {
            ureq::Error::Status(code, response) => {
                let text = response.into_string().unwrap_or_default();
                let text = text.trim();
                let hint = if code == 404 {
                    " (this Orbit does not have POST /api/events; update it)"
                } else {
                    ""
                };
                OrbitError {
                    message: format!("{} returned HTTP {code}{hint}: {text}", self.base),
                }
            }
            ureq::Error::Transport(transport) => OrbitError {
                message: format!("cannot reach Orbit at {}: {transport}", self.base),
            },
        }
    }
}
