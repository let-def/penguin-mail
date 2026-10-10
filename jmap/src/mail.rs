//! JMAP Mail (RFC 8621) and submission as the adapter uses them: the
//! objects it reads, and one call for each thing it asks. Every call goes
//! to the account the session names for mail, and a `get` of more ids
//! than the server takes at once goes in several requests.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::client::{Invocation, Jmap};
use crate::JmapError;
use crate::session::{MAIL, SUBMISSION};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// An IANA mailbox role, lower case: `inbox`, `sent`, `drafts`,
    /// `trash`, `junk`, `archive`, `all`, `important`.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub sort_order: u32,
    #[serde(default)]
    pub total_emails: u64,
    #[serde(default)]
    pub unread_emails: u64,
    #[serde(default)]
    pub total_threads: u64,
    #[serde(default)]
    pub unread_threads: u64,
    #[serde(default = "yes")]
    pub is_subscribed: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct EmailAddress {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: String,
}

/// The properties [`Email`] reads, asked for by name so the server sends
/// nothing else.
pub const EMAIL_PROPERTIES: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "mailboxIds",
    "keywords",
    "size",
    "receivedAt",
    "messageId",
    "inReplyTo",
    "references",
    "from",
    "to",
    "cc",
    "subject",
    "sentAt",
    "hasAttachment",
    "preview",
    "header:List-Unsubscribe:asText",
    "header:List-Unsubscribe-Post:asText",
];

/// What the list and the store need of a message. The body comes as the
/// raw message, by `blob_id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Email {
    pub id: String,
    #[serde(default)]
    pub blob_id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub mailbox_ids: BTreeMap<String, bool>,
    #[serde(default)]
    pub keywords: BTreeMap<String, bool>,
    #[serde(default)]
    pub size: u64,
    /// RFC 3339, in UTC.
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub message_id: Option<Vec<String>>,
    #[serde(default)]
    pub in_reply_to: Option<Vec<String>>,
    #[serde(default)]
    pub references: Option<Vec<String>>,
    #[serde(default)]
    pub from: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub to: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub cc: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub sent_at: Option<String>,
    #[serde(default)]
    pub has_attachment: bool,
    #[serde(default)]
    pub preview: String,
    #[serde(rename = "header:List-Unsubscribe:asText", default)]
    pub list_unsubscribe: Option<String>,
    #[serde(rename = "header:List-Unsubscribe-Post:asText", default)]
    pub list_unsubscribe_post: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub email_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub text_signature: String,
    #[serde(default)]
    pub html_signature: String,
    #[serde(default)]
    pub may_delete: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Got<T> {
    #[serde(default)]
    pub state: String,
    #[serde(default = "Vec::new")]
    pub list: Vec<T>,
    #[serde(default)]
    pub not_found: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Changed {
    pub old_state: String,
    pub new_state: String,
    #[serde(default)]
    pub has_more_changes: bool,
    #[serde(default)]
    pub created: Vec<String>,
    #[serde(default)]
    pub updated: Vec<String>,
    #[serde(default)]
    pub destroyed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queried {
    #[serde(default)]
    pub query_state: String,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub position: u64,
    #[serde(default)]
    pub total: Option<u64>,
}

/// Why the server refused one object of a `set`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SetError {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAnswer {
    #[serde(default)]
    pub new_state: Option<String>,
    #[serde(default)]
    pub created: Option<Map<String, Value>>,
    #[serde(default)]
    pub updated: Option<Map<String, Value>>,
    #[serde(default)]
    pub destroyed: Option<Vec<String>>,
    #[serde(default)]
    pub not_created: Option<BTreeMap<String, SetError>>,
    #[serde(default)]
    pub not_updated: Option<BTreeMap<String, SetError>>,
    #[serde(default)]
    pub not_destroyed: Option<BTreeMap<String, SetError>>,
}

impl SetAnswer {
    /// The server's id for the object made under `creation_id`.
    pub fn created_id(&self, creation_id: &str) -> Option<String> {
        self.created.as_ref()?.get(creation_id)?["id"]
            .as_str()
            .map(str::to_string)
    }

    /// Every object the server refused, and why.
    pub fn refused(&self) -> impl Iterator<Item = (&String, &SetError)> {
        [&self.not_created, &self.not_updated, &self.not_destroyed]
            .into_iter()
            .flatten()
            .flatten()
    }
}

/// A sent message: the copy the server filed, and its submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    pub email_id: String,
    pub thread_id: Option<String>,
}

impl Jmap {
    async fn mail_account(&self) -> Result<String, JmapError> {
        Ok(self.session().await?.account_for(MAIL)?.to_string())
    }

    async fn mail_call<T: DeserializeOwned>(
        &self,
        name: &'static str,
        mut args: Value,
    ) -> Result<T, JmapError> {
        args["accountId"] = json!(self.mail_account().await?);
        self.call(&[MAIL], name, args).await
    }

    /// Every mailbox.
    pub async fn mailboxes(&self) -> Result<Got<Mailbox>, JmapError> {
        self.mail_call("Mailbox/get", json!({ "ids": null })).await
    }

    /// What changed among the mailboxes since `since`.
    pub async fn mailbox_changes(&self, since: &str) -> Result<Changed, JmapError> {
        self.mail_call("Mailbox/changes", json!({ "sinceState": since }))
            .await
    }

    /// Creates, changes or destroys mailboxes, as `args` (`create`,
    /// `update`, `destroy`) say.
    pub async fn set_mailboxes(&self, args: Value) -> Result<SetAnswer, JmapError> {
        self.mail_call("Mailbox/set", args).await
    }

    /// The ids of the messages `filter` matches, newest first, from
    /// `position`, at most `limit`.
    pub async fn query_emails(
        &self,
        filter: Value,
        position: u64,
        limit: u64,
    ) -> Result<Queried, JmapError> {
        self.mail_call(
            "Email/query",
            json!({
                "filter": filter,
                "sort": [{ "property": "receivedAt", "isAscending": false }],
                "position": position,
                "limit": limit,
                "calculateTotal": false
            }),
        )
        .await
    }

    /// The messages `ids` names, with [`EMAIL_PROPERTIES`], in as many
    /// requests as the server's limit needs and with every get of one
    /// request batched. The ones the server no longer holds come back
    /// in `not_found`.
    pub async fn emails(&self, ids: &[String]) -> Result<Got<Email>, JmapError> {
        self.get_many("Email/get", ids, Some(EMAIL_PROPERTIES)).await
    }

    /// The thread of each message `ids` names, and nothing else of it,
    /// for a listing that wants ids.
    pub async fn email_threads(&self, ids: &[String]) -> Result<Got<Email>, JmapError> {
        self.get_many("Email/get", ids, Some(&["id", "threadId"])).await
    }

    /// The parts of message `id` and the text of its text parts, without
    /// the files: for a message too large to fetch whole.
    pub async fn structure(&self, id: &str) -> Result<Value, JmapError> {
        let answer: Got<Value> = self
            .mail_call(
                "Email/get",
                json!({
                    "ids": [id],
                    "properties": ["id", "headers", "bodyStructure", "bodyValues"],
                    "bodyProperties": ["partId", "blobId", "size", "name", "type", "charset",
                                       "disposition", "cid", "subParts"],
                    "fetchAllBodyValues": true,
                    "maxBodyValueBytes": 4_000_000
                }),
            )
            .await?;
        answer.list.into_iter().next().ok_or(JmapError::NotFound)
    }

    /// The bytes of a part of a message, by the part's blob.
    pub async fn part(&self, blob_id: &str, limit: usize) -> Result<Vec<u8>, JmapError> {
        let account = self.mail_account().await?;
        self.download(&account, blob_id, "application/octet-stream", limit)
            .await
    }

    /// The threads `ids` names.
    pub async fn threads(&self, ids: &[String]) -> Result<Got<Thread>, JmapError> {
        self.get_many("Thread/get", ids, None).await
    }

    async fn get_many<T: DeserializeOwned>(
        &self,
        name: &'static str,
        ids: &[String],
        properties: Option<&[&str]>,
    ) -> Result<Got<T>, JmapError> {
        let mut all = Got {
            state: String::new(),
            list: Vec::new(),
            not_found: Vec::new(),
        };
        if ids.is_empty() {
            return Ok(all);
        }
        let session = self.session().await?;
        let account = session.account_for(MAIL)?.to_string();
        let limits = session.limits();
        let calls: Vec<Invocation> = ids
            .chunks(limits.max_objects_in_get.max(1))
            .map(|chunk| {
                let mut args = json!({ "accountId": account, "ids": chunk });
                if let Some(properties) = properties {
                    args["properties"] = json!(properties);
                }
                Invocation::new(name, args)
            })
            .collect();
        for batch in calls.chunks(limits.max_calls_in_request.max(1)) {
            for answer in self.request(&[MAIL], batch.to_vec()).await? {
                let got: Got<T> = serde_json::from_value(answer?)
                    .map_err(|e| JmapError::Decode(format!("{name}: {e}")))?;
                all.state = got.state;
                all.list.extend(got.list);
                all.not_found.extend(got.not_found);
            }
        }
        Ok(all)
    }

    /// Where the messages stand now, for a feed that starts here.
    pub async fn email_state(&self) -> Result<String, JmapError> {
        let got: Got<Email> = self.mail_call("Email/get", json!({ "ids": [] })).await?;
        Ok(got.state)
    }

    /// What changed among the messages since `since`, at most `most` ids
    /// in one answer; `has_more_changes` says to ask again from its new
    /// state.
    pub async fn email_changes(&self, since: &str, most: u64) -> Result<Changed, JmapError> {
        self.mail_call(
            "Email/changes",
            json!({ "sinceState": since, "maxChanges": most }),
        )
        .await
    }

    /// Changes messages, each by a patch of its `mailboxIds/…` and
    /// `keywords/…`, and destroys others.
    pub async fn set_emails(
        &self,
        update: Map<String, Value>,
        destroy: &[String],
    ) -> Result<SetAnswer, JmapError> {
        let mut args = json!({});
        if !update.is_empty() {
            args["update"] = Value::Object(update);
        }
        if !destroy.is_empty() {
            args["destroy"] = json!(destroy);
        }
        self.mail_call("Email/set", args).await
    }

    /// Files `raw` as a new message in `mailbox_ids` with `keywords`, and
    /// answers its id.
    pub async fn import(
        &self,
        raw: Vec<u8>,
        mailbox_ids: &[&str],
        keywords: &[&str],
    ) -> Result<String, JmapError> {
        let account = self.mail_account().await?;
        let blob = self.upload(&account, raw, "message/rfc822").await?;
        let answer: SetAnswer = self
            .mail_call(
                "Email/import",
                json!({ "emails": { "m": {
                    "blobId": blob.blob_id,
                    "mailboxIds": flags(mailbox_ids),
                    "keywords": flags(keywords),
                }}}),
            )
            .await?;
        refusal(&answer, "m")?;
        answer
            .created_id("m")
            .ok_or_else(|| JmapError::Decode("Email/import made no message".into()))
    }

    /// The raw bytes of message blob `blob_id`, refused past `limit`.
    pub async fn raw(&self, blob_id: &str, limit: usize) -> Result<Vec<u8>, JmapError> {
        let account = self.mail_account().await?;
        self.download(&account, blob_id, "message/rfc822", limit)
            .await
    }

    /// The addresses the account may send as.
    pub async fn identities(&self) -> Result<Vec<Identity>, JmapError> {
        let account = self.session().await?.account_for(SUBMISSION)?.to_string();
        let got: Got<Identity> = self
            .call(
                &[MAIL, SUBMISSION],
                "Identity/get",
                json!({ "accountId": account, "ids": null }),
            )
            .await?;
        Ok(got.list)
    }

    /// Sends `raw` as `identity_id`: files it in `drafts` as a draft, then
    /// submits it, and once the server sent it moves the copy to `sent`,
    /// read and no longer a draft. All in one request, so a refusal of the
    /// submission leaves the draft for the person to try again.
    pub async fn send(
        &self,
        raw: Vec<u8>,
        identity_id: &str,
        drafts: &str,
        sent: &str,
    ) -> Result<Submitted, JmapError> {
        let account = self.mail_account().await?;
        let blob = self.upload(&account, raw, "message/rfc822").await?;
        let calls = vec![
            Invocation::new(
                "Email/import",
                json!({
                    "accountId": account,
                    "emails": { "m": {
                        "blobId": blob.blob_id,
                        "mailboxIds": { drafts: true },
                        "keywords": { "$draft": true, "$seen": true },
                    }}
                }),
            ),
            Invocation::new(
                "EmailSubmission/set",
                json!({
                    "accountId": account,
                    "create": { "s": { "identityId": identity_id, "emailId": "#m" } },
                    "onSuccessUpdateEmail": { "#s": {
                        format!("mailboxIds/{drafts}"): null,
                        format!("mailboxIds/{sent}"): true,
                        "keywords/$draft": null,
                    }}
                }),
            ),
        ];
        let mut answers = self.request(&[MAIL, SUBMISSION], calls).await?.into_iter();
        let imported: SetAnswer = decode(answers.next(), "Email/import")?;
        refusal(&imported, "m")?;
        let email_id = imported
            .created_id("m")
            .ok_or_else(|| JmapError::Decode("Email/import made no message".into()))?;
        let submitted: SetAnswer = decode(answers.next(), "EmailSubmission/set")?;
        refusal(&submitted, "s")?;
        let thread_id = imported.created.as_ref().and_then(|c| c["m"]["threadId"].as_str().map(str::to_string));
        Ok(Submitted { email_id, thread_id })
    }

    /// Sends the draft `email_id` the server already holds, as
    /// [`Jmap::send`] does once it filed one.
    pub async fn submit(
        &self,
        email_id: &str,
        identity_id: &str,
        drafts: &str,
        sent: &str,
    ) -> Result<(), JmapError> {
        let account = self.mail_account().await?;
        let answer: SetAnswer = self
            .call(
                &[MAIL, SUBMISSION],
                "EmailSubmission/set",
                json!({
                    "accountId": account,
                    "create": { "s": { "identityId": identity_id, "emailId": email_id } },
                    "onSuccessUpdateEmail": { "#s": {
                        format!("mailboxIds/{drafts}"): null,
                        format!("mailboxIds/{sent}"): true,
                        "keywords/$draft": null,
                    }}
                }),
            )
            .await?;
        refusal(&answer, "s")
    }
}

/// `{ "a": true, "b": true }` for a JMAP set of ids or keywords.
fn flags(names: &[&str]) -> Value {
    Value::Object(names.iter().map(|n| (n.to_string(), Value::Bool(true))).collect())
}

fn decode<T: DeserializeOwned>(answer: Option<Result<Value, JmapError>>, name: &str) -> Result<T, JmapError> {
    let value = answer.unwrap_or_else(|| Err(JmapError::Decode(format!("{name}: no answer"))))?;
    serde_json::from_value(value).map_err(|e| JmapError::Decode(format!("{name}: {e}")))
}

/// The refusal of the object made under `creation_id`, as an error.
fn refusal(answer: &SetAnswer, creation_id: &str) -> Result<(), JmapError> {
    match answer.not_created.as_ref().and_then(|refused| refused.get(creation_id)) {
        Some(refused) => Err(JmapError::method(
            &refused.kind,
            refused.description.as_deref().unwrap_or(""),
        )),
        None => Ok(()),
    }
}
