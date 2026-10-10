//! The JMAP client for one account: it reads the session, sends method
//! calls in batches, and moves blobs. The token goes only to the URLs the
//! session names, and only over HTTPS (`session::secure`). A request
//! whose answer carries another session state drops the session it knew,
//! so the next request reads it again.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

use crate::JmapError;
use crate::session::{self, CORE, Session};

/// The most one JSON answer may take. A page of a few hundred messages'
/// metadata is a few megabytes; this stops a broken server from filling
/// memory.
const JSON_LIMIT: usize = 32 << 20;

/// What an error body may take; problem details are a few hundred bytes.
const ERROR_LIMIT: usize = 64 << 10;

/// One method call: its name, such as `Email/get`, and its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub name: &'static str,
    pub args: Value,
}

impl Invocation {
    pub fn new(name: &'static str, args: Value) -> Invocation {
        Invocation { name, args }
    }
}

/// A blob the server took, as `Email/import` and `Email/set` name it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Uploaded {
    pub blob_id: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    method_responses: Vec<(String, Value, String)>,
    #[serde(default)]
    session_state: Option<String>,
}

/// The client. Clones share the session it read.
#[derive(Clone)]
pub struct Jmap {
    http: reqwest::Client,
    session_url: Url,
    token: Arc<str>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
}

impl std::fmt::Debug for Jmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jmap")
            .field("session_url", &self.session_url.as_str())
            .finish_non_exhaustive()
    }
}

impl Jmap {
    /// A client for the session at `session_url`, which carries `token`
    /// as a bearer token, such as a Fastmail API token.
    pub fn new(session_url: &str, token: &str) -> Result<Jmap, JmapError> {
        let session_url = session::secure(session_url)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| JmapError::Network(e.to_string()))?;
        Ok(Jmap {
            http,
            session_url,
            token: Arc::from(token),
            session: Arc::default(),
        })
    }

    /// The session, read once and kept until an answer says it changed.
    pub async fn session(&self) -> Result<Arc<Session>, JmapError> {
        if let Some(known) = self.known() {
            return Ok(known);
        }
        let response = self
            .http
            .get(self.session_url.clone())
            .bearer_auth(&*self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(network)?;
        let session: Session = json_of(response).await?;
        // Every URL the session names has to pass before any is used.
        session.api()?;
        let session = Arc::new(session);
        *self.lock() = Some(Arc::clone(&session));
        Ok(session)
    }

    fn known(&self) -> Option<Arc<Session>> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<Session>>> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends `calls` in one request using the `using` capabilities, core
    /// included, and answers each call in order: its arguments, or the
    /// method error it met. A failure of the request as a whole is the
    /// outer error.
    pub async fn request(
        &self,
        using: &[&str],
        calls: Vec<Invocation>,
    ) -> Result<Vec<Result<Value, JmapError>>, JmapError> {
        let session = self.session().await?;
        let limit = session.limits().max_calls_in_request.max(1);
        if calls.len() > limit {
            return Err(JmapError::Method {
                kind: "requestTooLarge".into(),
                description: format!("{} calls, the server takes {limit}", calls.len()),
            });
        }
        let mut capabilities: Vec<&str> = vec![CORE];
        capabilities.extend(using.iter().filter(|c| **c != CORE));
        let method_calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| json!([call.name, call.args, format!("c{index}")]))
            .collect();
        let body = json!({ "using": capabilities, "methodCalls": method_calls });
        let response = self
            .http
            .post(session.api()?)
            .bearer_auth(&*self.token)
            .json(&body)
            .send()
            .await
            .map_err(network)?;
        let answer: Answer = json_of(response).await?;
        if answer
            .session_state
            .as_deref()
            .is_some_and(|state| state != session.state)
        {
            *self.lock() = None;
        }
        let mut results: Vec<Result<Value, JmapError>> =
            vec![Err(JmapError::Decode("no answer".into())); calls.len()];
        for (name, args, id) in answer.method_responses {
            let Some(index) = id.strip_prefix('c').and_then(|n| n.parse::<usize>().ok()) else {
                continue;
            };
            let Some(slot) = results.get_mut(index) else {
                continue;
            };
            // A call can answer more than once, as `Email/set` with
            // `onSuccessUpdateEmail` does; the first answer is the call's.
            if slot.is_ok() {
                continue;
            }
            *slot = match name.as_str() {
                "error" => {
                    let kind = args["type"].as_str().unwrap_or("serverFail");
                    let description = args["description"].as_str().unwrap_or("");
                    Err(JmapError::method(kind, description))
                }
                _ => Ok(args),
            };
        }
        Ok(results)
    }

    /// One method call, its answer read as `T`.
    pub async fn call<T: DeserializeOwned>(
        &self,
        using: &[&str],
        name: &'static str,
        args: Value,
    ) -> Result<T, JmapError> {
        let answer = self
            .request(using, vec![Invocation::new(name, args)])
            .await?
            .pop()
            .unwrap_or_else(|| Err(JmapError::Decode("no answer".into())))?;
        serde_json::from_value(answer).map_err(|e| JmapError::Decode(format!("{name}: {e}")))
    }

    /// The bytes of blob `blob_id`, refused past `limit`.
    pub async fn download(
        &self,
        account_id: &str,
        blob_id: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<u8>, JmapError> {
        let session = self.session().await?;
        let url = session.download(account_id, blob_id, kind, "blob")?;
        let response = self
            .http
            .get(url)
            .bearer_auth(&*self.token)
            .send()
            .await
            .map_err(network)?;
        if !response.status().is_success() {
            return Err(error_of(response).await);
        }
        read_limited(response, limit).await
    }

    /// Uploads `bytes` as a blob of type `kind`.
    pub async fn upload(
        &self,
        account_id: &str,
        bytes: Vec<u8>,
        kind: &str,
    ) -> Result<Uploaded, JmapError> {
        let session = self.session().await?;
        let limit = session.limits().max_size_upload;
        if bytes.len() as u64 > limit {
            return Err(JmapError::TooLarge { limit: limit as usize });
        }
        let response = self
            .http
            .post(session.upload(account_id)?)
            .bearer_auth(&*self.token)
            .header("Content-Type", kind)
            .body(bytes)
            .send()
            .await
            .map_err(network)?;
        json_of(response).await
    }
}

fn network(err: reqwest::Error) -> JmapError {
    JmapError::Network(err.to_string())
}

async fn json_of<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, JmapError> {
    if !response.status().is_success() {
        return Err(error_of(response).await);
    }
    let bytes = read_limited(response, JSON_LIMIT).await?;
    serde_json::from_slice(&bytes).map_err(|e| JmapError::Decode(e.to_string()))
}

/// The error a failed response stands for: 401 is a token the server no
/// longer takes, 429 and 503 a request to slow down, anything else the
/// problem details the server sent (RFC 8620 section 3.6.1).
async fn error_of(response: reqwest::Response) -> JmapError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    match status {
        401 => return JmapError::Unauthorized,
        404 => return JmapError::NotFound,
        429 | 503 => return JmapError::Throttled { retry_after },
        _ => {}
    }
    let body = read_limited(response, ERROR_LIMIT).await.unwrap_or_default();
    let problem: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let detail = problem["detail"]
        .as_str()
        .or(problem["type"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(200).collect());
    JmapError::Http { status, detail }
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, JmapError> {
    if response.content_length().is_some_and(|n| n as usize > limit) {
        return Err(JmapError::TooLarge { limit });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if bytes.len() + chunk.len() > limit {
            return Err(JmapError::TooLarge { limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
