use anyhow::Result;
use reqwest::{StatusCode, header::RETRY_AFTER};
use serde_json::Value;
use std::{error::Error, fmt, time::Duration};

pub const DEFAULT_BASE: &str = "https://api.typesafe.ai";
pub const DEFAULT_MODEL: &str = "jev-latest";
const ENDPOINT: &str = "/v1/systemone";
/// One attempt plus five retries, matching the documented backoff policy.
const MAX_ATTEMPTS: usize = 6;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum ApiError {
    Auth(String),
    Http(StatusCode, String),
    Transport(String),
    InvalidResponse(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(body) => {
                write!(
                    f,
                    "API rejected the credentials: {body}; run jev login or set JEV_API_KEY"
                )
            }
            Self::Http(s, b) => write!(f, "API {s}: {b}"),
            Self::Transport(e) => write!(f, "API transport error: {e}"),
            Self::InvalidResponse(e) => write!(f, "Invalid API response: {e}"),
        }
    }
}

impl Error for ApiError {}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub model: String,
    /// JSON object mapping question id to its typed answer.
    pub answers: Value,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    key: String,
    base: String,
}

impl Client {
    pub fn new(key: String, base: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            key,
            base: base.trim_end_matches('/').into(),
        })
    }

    /// Cheap real request used by `jev login` to verify the key.
    pub async fn verify(&self) -> Result<()> {
        self.evaluate(&serde_json::json!({
            "state": "login verification",
            "model": DEFAULT_MODEL,
            "questions": {
                "ok": {"type": "noul", "instructions": "Is this sentence in English?"}
            }
        }))
        .await
        .map(|_| ())
    }

    pub async fn evaluate(&self, body: &Value) -> Result<Evaluation> {
        let payload = serde_json::to_vec(body)?;
        for attempt in 0..MAX_ATTEMPTS {
            let sent = self
                .http
                .post(format!("{}{ENDPOINT}", self.base))
                .bearer_auth(&self.key)
                .header("content-type", "application/json")
                .body(payload.clone())
                .send()
                .await;
            match sent {
                Ok(r) if r.status().is_success() => {
                    let bytes = r
                        .bytes()
                        .await
                        .map_err(|e| ApiError::Transport(e.to_string()))?;
                    let value: Value = serde_json::from_slice(&bytes)
                        .map_err(|e| ApiError::InvalidResponse(e.to_string()))?;
                    return parse_evaluation(&value);
                }
                Ok(r) => {
                    let status = r.status();
                    let header_retry_secs = r
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_retry_after_seconds);
                    let text = r.text().await.unwrap_or_default();
                    let server_retry_ms =
                        body_retry_ms(&text).or_else(|| header_retry_secs.map(|s| s * 1000));
                    if matches!(status.as_u16(), 401 | 403) {
                        return Err(ApiError::Auth(trim_error_body(&text, &status)).into());
                    }
                    if !retryable_status(status) || attempt + 1 == MAX_ATTEMPTS {
                        return Err(ApiError::Http(status, trim_error_body(&text, &status)).into());
                    }
                    let delay = backoff_ms(attempt, server_retry_ms);
                    crate::diagnostics::log(format_args!(
                        "API retry {}/{} after {delay}ms",
                        attempt + 1,
                        MAX_ATTEMPTS - 1
                    ));
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                Err(e) => {
                    if attempt + 1 == MAX_ATTEMPTS {
                        return Err(ApiError::Transport(e.to_string()).into());
                    }
                    let delay = backoff_ms(attempt, None);
                    crate::diagnostics::log(format_args!(
                        "Network retry {}/{} after {delay}ms",
                        attempt + 1,
                        MAX_ATTEMPTS - 1
                    ));
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
        unreachable!()
    }
}

fn parse_evaluation(value: &Value) -> Result<Evaluation> {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::InvalidResponse("missing model".into()))?
        .to_string();
    let Some(answers) = value.get("answers") else {
        return Err(ApiError::InvalidResponse("missing answers".into()).into());
    };
    if !answers.is_object() {
        return Err(ApiError::InvalidResponse("answers is not an object".into()).into());
    }
    let usage = Usage {
        input_tokens: value
            .get("usage")
            .and_then(|u| u.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("usage")
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    Ok(Evaluation {
        model,
        answers: answers.clone(),
        usage,
    })
}

fn trim_error_body(body: &str, status: &StatusCode) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        format!("empty body (HTTP {status})")
    } else {
        trimmed.chars().take(400).collect()
    }
}

/// 500ms * 2^attempt, capped at 8s; a server-provided value wins when present.
fn backoff_ms(attempt: usize, server_ms: Option<u64>) -> u64 {
    server_ms.unwrap_or_else(|| (500u64 << attempt).min(8000))
}

fn body_retry_ms(text: &str) -> Option<u64> {
    let body: Value = serde_json::from_str(text).ok()?;
    body.get("retry_after_ms")
        .and_then(Value::as_u64)
        .or_else(|| {
            body.get("error")
                .and_then(|e| e.get("retry_after_ms"))
                .and_then(Value::as_u64)
        })
}

fn parse_retry_after_seconds(value: &str) -> Option<u64> {
    value.parse().ok().or_else(|| {
        httpdate::parse_http_date(value).ok().map(|t| {
            t.duration_since(std::time::SystemTime::now())
                .unwrap_or_default()
                .as_secs()
        })
    })
}

pub fn retryable_status(s: StatusCode) -> bool {
    matches!(s.as_u16(), 429 | 500 | 502 | 503 | 504 | 529)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn client(server: &MockServer) -> Client {
        Client::new("test-key".into(), &server.uri()).unwrap()
    }

    fn body() -> Value {
        serde_json::json!({
            "state": "help, my payouts fail",
            "model": DEFAULT_MODEL,
            "questions": {"urgent": {"type": "noul", "instructions": "Is this urgent?"}}
        })
    }

    #[tokio::test]
    async fn success_parses_model_answers_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "jev-latest",
                "answers": {"urgent": {"type": "noul", "noul": 0.92}},
                "usage": {"input_tokens": 312, "output_tokens": 48}
            })))
            .mount(&server)
            .await;
        let evaluation = client(&server).evaluate(&body()).await.unwrap();
        assert_eq!(evaluation.model, "jev-latest");
        assert_eq!(evaluation.answers["urgent"]["noul"], 0.92);
        assert_eq!(evaluation.usage.input_tokens, 312);
        assert_eq!(evaluation.usage.output_tokens, 48);
        let sent: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["model"], "jev-latest");
        assert_eq!(sent["questions"]["urgent"]["type"], "noul");
    }

    #[tokio::test]
    async fn retries_on_429_and_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(|_: &wiremock::Request| {
                static COUNT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                if COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(429)
                        .set_body_json(serde_json::json!({"retry_after_ms": 0}))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "model": "jev-latest",
                        "answers": {"urgent": {"type": "noul", "noul": 0.5}},
                        "usage": {"input_tokens": 10, "output_tokens": 1}
                    }))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let evaluation = client(&server).evaluate(&body()).await.unwrap();
        assert_eq!(evaluation.usage.input_tokens, 10);
    }

    #[tokio::test]
    async fn auth_failure_is_immediate_and_actionable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
            .expect(1)
            .mount(&server)
            .await;
        let error = client(&server).evaluate(&body()).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("jev login"), "{text}");
    }

    #[tokio::test]
    async fn validation_error_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_string("{\"error\":\"questions.urgent: missing type\"}"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = client(&server).evaluate(&body()).await.unwrap_err();
        assert!(format!("{error:#}").contains("422"));
    }

    #[tokio::test]
    async fn invalid_success_payload_is_typed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let error = client(&server).evaluate(&body()).await.unwrap_err();
        assert!(error.downcast_ref::<ApiError>().is_some());
    }

    #[test]
    fn backoff_schedule_and_server_override() {
        assert_eq!(backoff_ms(0, None), 500);
        assert_eq!(backoff_ms(1, None), 1000);
        assert_eq!(backoff_ms(2, None), 2000);
        assert_eq!(backoff_ms(10, None), 8000);
        assert_eq!(backoff_ms(1, Some(50)), 50);
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(
            StatusCode::from_u16(529).expect("529 is a valid status code")
        ));
        assert!(!retryable_status(StatusCode::UNPROCESSABLE_ENTITY));
    }
}
