//! The JMAP session (RFC 8620 section 2): where to send calls, where blobs
//! come from and go to, which account serves mail, and the limits every
//! request keeps to.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::JmapError;

/// The capabilities Penguin Mail uses, by their URNs.
pub const CORE: &str = "urn:ietf:params:jmap:core";
pub const MAIL: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";

/// Fastmail's session resource.
pub const FASTMAIL_SESSION: &str = "https://api.fastmail.com/jmap/session";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(default)]
    pub capabilities: HashMap<String, Value>,
    #[serde(default)]
    pub accounts: HashMap<String, Account>,
    #[serde(default)]
    pub primary_accounts: HashMap<String, String>,
    #[serde(default)]
    pub username: String,
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    #[serde(default)]
    pub event_source_url: Option<String>,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_read_only: bool,
}

/// The core capability's limits. A server that leaves one out is taken
/// at RFC 8620's floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    #[serde(default = "Limits::floor_calls")]
    pub max_calls_in_request: usize,
    #[serde(default = "Limits::floor_objects")]
    pub max_objects_in_get: usize,
    #[serde(default = "Limits::floor_objects")]
    pub max_objects_in_set: usize,
    #[serde(default = "Limits::floor_upload")]
    pub max_size_upload: u64,
}

impl Limits {
    fn floor_calls() -> usize {
        16
    }
    fn floor_objects() -> usize {
        500
    }
    fn floor_upload() -> u64 {
        50_000_000
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_calls_in_request: Limits::floor_calls(),
            max_objects_in_get: Limits::floor_objects(),
            max_objects_in_set: Limits::floor_objects(),
            max_size_upload: Limits::floor_upload(),
        }
    }
}

impl Session {
    /// The account that serves `capability`, as the session names it.
    pub fn account_for(&self, capability: &'static str) -> Result<&str, JmapError> {
        self.primary_accounts
            .get(capability)
            .map(String::as_str)
            .ok_or(JmapError::Missing(capability))
    }

    pub fn limits(&self) -> Limits {
        self.capabilities
            .get(CORE)
            .and_then(|core| serde_json::from_value(core.clone()).ok())
            .unwrap_or_default()
    }

    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.contains_key(capability)
    }

    /// The URL to fetch blob `blob_id` from, from the session's template.
    pub fn download(&self, account_id: &str, blob_id: &str, kind: &str, name: &str) -> Result<Url, JmapError> {
        let filled = self
            .download_url
            .replace("{accountId}", &encode(account_id))
            .replace("{blobId}", &encode(blob_id))
            .replace("{type}", &encode(kind))
            .replace("{name}", &encode(name));
        secure(&filled)
    }

    /// The URL to upload a blob to for `account_id`.
    pub fn upload(&self, account_id: &str) -> Result<Url, JmapError> {
        secure(&self.upload_url.replace("{accountId}", &encode(account_id)))
    }

    pub fn api(&self) -> Result<Url, JmapError> {
        secure(&self.api_url)
    }
}

/// `url`, when the token may go there: over HTTPS, or to this computer,
/// which is how a test server listens. The session names these URLs, and
/// the server that answered the session already holds the token, so only
/// the scheme is checked.
pub fn secure(url: &str) -> Result<Url, JmapError> {
    let parsed = Url::parse(url).map_err(|e| JmapError::Decode(format!("bad URL {url}: {e}")))?;
    let local = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if local => Ok(parsed),
        _ => Err(JmapError::Insecure(parsed.host_str().unwrap_or(url).to_string())),
    }
}

/// A value for a URL template, with everything but unreserved characters
/// escaped (RFC 6570 simple expansion).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fastmail() -> Session {
        serde_json::from_value(serde_json::json!({
            "capabilities": {
                CORE: { "maxCallsInRequest": 50, "maxObjectsInGet": 4096, "maxObjectsInSet": 4096,
                        "maxSizeUpload": 250000000 },
                MAIL: {},
                SUBMISSION: {}
            },
            "accounts": { "u1": { "name": "dana@fastmail.com", "isPersonal": true, "isReadOnly": false } },
            "primaryAccounts": { MAIL: "u1", SUBMISSION: "u1" },
            "username": "dana@fastmail.com",
            "apiUrl": "https://api.fastmail.com/jmap/api/",
            "downloadUrl": "https://www.fastmailusercontent.com/jmap/download/{accountId}/{blobId}/{name}?type={type}",
            "uploadUrl": "https://api.fastmail.com/jmap/upload/{accountId}/",
            "eventSourceUrl": "https://api.fastmail.com/jmap/event/",
            "state": "abc"
        }))
        .unwrap()
    }

    #[test]
    fn the_session_names_the_mail_account_and_its_limits() {
        let session = fastmail();
        assert_eq!(session.account_for(MAIL), Ok("u1"));
        assert_eq!(session.limits().max_objects_in_get, 4096);
        assert_eq!(session.limits().max_calls_in_request, 50);
        assert_eq!(
            session.account_for("urn:ietf:params:jmap:calendars"),
            Err(JmapError::Missing("urn:ietf:params:jmap:calendars"))
        );
    }

    #[test]
    fn a_server_that_states_no_limits_gets_the_floor() {
        let mut session = fastmail();
        session.capabilities.insert(CORE.into(), serde_json::json!({}));
        assert_eq!(session.limits(), Limits::default());
    }

    #[test]
    fn blob_urls_fill_the_template_escaped() {
        let url = fastmail().download("u1", "G a/b", "message/rfc822", "mail.eml").unwrap();
        assert_eq!(
            url.as_str(),
            "https://www.fastmailusercontent.com/jmap/download/u1/G%20a%2Fb/mail.eml?type=message%2Frfc822"
        );
        assert_eq!(
            fastmail().upload("u1").unwrap().as_str(),
            "https://api.fastmail.com/jmap/upload/u1/"
        );
    }

    #[test]
    fn the_token_goes_over_https_or_to_this_computer_only() {
        assert!(secure("https://api.fastmail.com/jmap/api/").is_ok());
        assert!(secure("http://127.0.0.1:8080/jmap").is_ok());
        assert_eq!(
            secure("http://api.example.com/jmap"),
            Err(JmapError::Insecure("api.example.com".into()))
        );
    }
}
