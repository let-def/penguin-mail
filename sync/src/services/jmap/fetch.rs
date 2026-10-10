//! Metadata, whole conversations, the window and the inbox check, raw
//! messages, drafts, and the structure and files of a large message.

use mailrs_domain::{Address, Memberships, MessageMeta, Role};
use mailrs_jmap::{Email, EmailAddress};
use mailrs_mime::{Part, Parts, content_id, numbered};
use serde_json::{Value, json};

use super::{JmapApi, JmapMail, backend};
use crate::BackendError;
use crate::api::DraftRef;
use crate::services::{Backfill, Found, RawMessage, RemoteRef, Want};

/// The most a whole-message fetch takes, as on the other adapters.
const RAW_FETCH_LIMIT: usize = 64 << 20;

/// The most one part fetch takes.
const PART_LIMIT: usize = 64 << 20;

/// The ids one query page asks for.
const PAGE: u64 = 200;

/// The most ids a window or inbox listing returns.
const MOST_IDS: usize = 20_000;

/// The most drafts a listing returns.
const MOST_DRAFTS: u64 = 500;

/// How many levels of parts a structure keeps, as the other readers do.
const MAX_DEPTH: usize = mailrs_mime::MAX_DEPTH;

fn address(address: &EmailAddress) -> Option<Address> {
    (!address.email.is_empty()).then(|| Address {
        name: address.name.clone().filter(|n| !n.is_empty()),
        email: address.email.clone(),
    })
}

fn addresses(list: &Option<Vec<EmailAddress>>) -> Vec<Address> {
    list.iter().flatten().filter_map(address).collect()
}

/// RFC 3339 as milliseconds since the epoch.
fn millis(at: &Option<String>) -> i64 {
    at.as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.timestamp_millis())
        .unwrap_or_default()
}

/// `days` before now, as JMAP reads a time.
pub(super) fn days_ago(days: i64) -> String {
    let since = chrono::Utc::now() - chrono::Duration::days(days.max(1));
    since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn remote_ref(email: &Email) -> RemoteRef {
    RemoteRef {
        id: email.id.clone(),
        thread_id: match email.thread_id.as_str() {
            "" => email.id.clone(),
            thread => thread.to_string(),
        },
    }
}

impl<J: JmapApi> JmapMail<J> {
    pub(super) fn meta_of(&self, email: &Email) -> MessageMeta {
        let mailboxes: Vec<String> = email
            .mailbox_ids
            .iter()
            .filter(|(_, on)| **on)
            .map(|(id, _)| id.clone())
            .collect();
        let roles: Vec<Role> = {
            let known = self.known();
            known
                .roles
                .iter()
                .filter(|(_, id)| mailboxes.contains(id))
                .map(|(role, _)| *role)
                .collect()
        };
        // JMAP compares keywords without case and Fastmail sends them in
        // lower case; the store keeps them so.
        let keywords = email
            .keywords
            .iter()
            .filter(|(_, on)| **on)
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        MessageMeta {
            account_id: 0,
            id: email.id.clone(),
            thread_id: remote_ref(email).thread_id,
            rfc822_msgid: email.message_id.iter().flatten().next().map(|m| content_id(m)),
            from: email.from.iter().flatten().find_map(address),
            to: addresses(&email.to),
            cc: addresses(&email.cc),
            subject: email.subject.clone().unwrap_or_default(),
            date: millis(&email.received_at),
            snippet: email.preview.split_whitespace().collect::<Vec<_>>().join(" "),
            size: i64::try_from(email.size).unwrap_or(i64::MAX),
            has_attachments: email.has_attachment,
            held: Memberships {
                mailboxes,
                keywords,
                categories: Vec::new(),
            },
            roles,
            list_unsubscribe: email
                .list_unsubscribe
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            one_click: email
                .list_unsubscribe_post
                .as_deref()
                .is_some_and(|v| v.to_ascii_lowercase().contains("one-click")),
        }
    }

    pub(super) async fn fetch_metas(&self, wants: Vec<Want>) -> Result<Found, BackendError> {
        // A message's role comes from the mailbox listing.
        self.synced().await?;
        let ids: Vec<String> = wants.into_iter().map(|w| w.id).collect();
        let got = self.api().emails(&ids).await.map_err(backend)?;
        Ok(Found {
            metas: got.list.iter().map(|e| self.meta_of(e)).collect(),
            gone: got.not_found,
            ..Found::default()
        })
    }

    pub(super) async fn fetch_threads(&self, threads: Vec<String>) -> Result<Found, BackendError> {
        self.synced().await?;
        let got = self.api().threads(&threads).await.map_err(backend)?;
        let mut found = Found {
            gone_threads: got.not_found,
            ..Found::default()
        };
        for thread in got.list {
            let emails = self.api().emails(&thread.email_ids).await.map_err(backend)?;
            if emails.list.is_empty() {
                found.gone_threads.push(thread.id);
                continue;
            }
            let metas: Vec<MessageMeta> = emails.list.iter().map(|e| self.meta_of(e)).collect();
            found.metas.extend(metas.iter().cloned());
            found.whole.push(metas);
        }
        Ok(found)
    }

    pub(super) async fn raws(&self, ids: &[String]) -> Result<Vec<RawMessage>, BackendError> {
        let got = self.api().emails(ids).await.map_err(backend)?;
        let mut raws = Vec::with_capacity(ids.len());
        // In the order asked for, as the trait says.
        for id in ids {
            let Some(email) = got.list.iter().find(|e| &e.id == id) else {
                return Err(BackendError::NotFound);
            };
            let bytes = self
                .api()
                .raw(&email.blob_id, RAW_FETCH_LIMIT)
                .await
                .map_err(backend)?;
            raws.push(RawMessage { id: id.clone(), bytes });
        }
        Ok(raws)
    }

    /// The refs of the messages `filter` matches, newest first, from
    /// `position`, one page.
    async fn page_of(&self, filter: &Value, position: u64) -> Result<Vec<RemoteRef>, BackendError> {
        let ids = self
            .api()
            .query_emails(filter.clone(), position, PAGE)
            .await
            .map_err(backend)?
            .ids;
        let threads = self.api().email_threads(&ids).await.map_err(backend)?;
        // The query's order, which the get does not keep.
        Ok(ids
            .iter()
            .filter_map(|id| threads.list.iter().find(|e| &e.id == id).map(remote_ref))
            .collect())
    }

    /// Every message `filter` matches, up to [`MOST_IDS`]. `None` when
    /// there are more.
    async fn all_of(&self, filter: Value) -> Result<Option<Vec<RemoteRef>>, BackendError> {
        let mut all = Vec::new();
        loop {
            let page = self.page_of(&filter, all.len() as u64).await?;
            let last = (page.len() as u64) < PAGE;
            all.extend(page);
            if all.len() > MOST_IDS {
                return Ok(None);
            }
            if last {
                return Ok(Some(all));
            }
        }
    }

    pub(super) async fn backfill_page(&self, days: i64, cursor: Option<&str>) -> Result<Backfill, BackendError> {
        let inbox = self.role_id(Role::Inbox).await?;
        let position = match cursor {
            None => 0,
            Some(cursor) => cursor.parse::<u64>().map_err(|_| BackendError::StateLost)?,
        };
        let filter = json!({ "operator": "OR", "conditions": [
            { "after": days_ago(days) },
            { "inMailbox": inbox },
        ]});
        let refs = self.page_of(&filter, position).await?;
        let next = ((refs.len() as u64) == PAGE).then(|| (position + PAGE).to_string());
        Ok(Backfill { refs, next })
    }

    pub(super) async fn window_refs(&self, days: i64, mailbox: Option<&str>) -> Result<Vec<RemoteRef>, BackendError> {
        let mut filter = json!({ "after": days_ago(days) });
        if let Some(mailbox) = mailbox {
            filter["inMailbox"] = json!(mailbox);
        }
        // A window larger than this keeps what it listed; the next look
        // carries on from the feed.
        Ok(self.all_of(filter).await?.unwrap_or_default())
    }

    pub(super) async fn inbox_refs(&self) -> Result<Vec<RemoteRef>, BackendError> {
        let inbox = self.role_id(Role::Inbox).await?;
        self.all_of(json!({ "inMailbox": inbox }))
            .await?
            .ok_or(BackendError::Unsupported)
    }

    pub(super) async fn sent_with(&self, message_id: &str) -> Result<Option<String>, BackendError> {
        let wanted = format!("<{}>", content_id(message_id));
        let found = self
            .api()
            .query_emails(json!({ "header": ["Message-ID", wanted] }), 0, 1)
            .await
            .map_err(backend)?;
        Ok(found.ids.into_iter().next())
    }

    pub(super) async fn drafts(&self) -> Result<Vec<DraftRef>, BackendError> {
        let drafts = self.role_id(Role::Drafts).await?;
        let found = self
            .api()
            .query_emails(json!({ "inMailbox": drafts }), 0, MOST_DRAFTS)
            .await
            .map_err(backend)?;
        Ok(found
            .ids
            .into_iter()
            .map(|id| DraftRef {
                draft_id: id.clone(),
                message_id: id,
            })
            .collect())
    }

    pub(super) async fn structure_of(&self, id: &str) -> Result<Parts, BackendError> {
        let email = self.api().structure(id).await.map_err(backend)?;
        Ok(parts_of(&email))
    }

    pub(super) async fn part_of(&self, id: &str, path: &str) -> Result<Vec<u8>, BackendError> {
        let email = self.api().structure(id).await.map_err(backend)?;
        let blob = blob_at(&email["bodyStructure"], root_path(&email["bodyStructure"]), path, 0)
            .ok_or(BackendError::NotFound)?;
        self.api().part(&blob, PART_LIMIT).await.map_err(backend)
    }
}

/// A message's headers and parts from the `headers`, `bodyStructure` and
/// `bodyValues` of an `Email/get`, numbered as IMAP numbers them. A text
/// part's bytes come from its body value, which the server already turned
/// into UTF-8, so the part says so.
pub(super) fn parts_of(email: &Value) -> Parts {
    let headers = email["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| Some((h["name"].as_str()?.to_string(), h["value"].as_str()?.trim().to_string())))
        .collect();
    let structure = &email["bodyStructure"];
    let mut incomplete = false;
    let root = part(structure, root_path(structure), &email["bodyValues"], 0, &mut incomplete);
    Parts { headers, root, incomplete }
}

/// A multipart root goes unnumbered, as IMAP numbers it; a single part
/// is part 1.
fn root_path(structure: &Value) -> String {
    match structure["subParts"].is_array() {
        true => String::new(),
        false => "1".into(),
    }
}

fn part(node: &Value, path: String, values: &Value, depth: usize, incomplete: &mut bool) -> Part {
    let mime_type = node["type"].as_str().unwrap_or("text/plain").to_ascii_lowercase();
    let value = node["partId"].as_str().map(|id| &values[id]);
    let data = value
        .and_then(|v| v["value"].as_str())
        .map(|text| text.as_bytes().to_vec());
    if value.is_some_and(|v| v["isTruncated"].as_bool() == Some(true)) {
        *incomplete = true;
    }
    let children = match depth < MAX_DEPTH {
        true => node["subParts"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, child)| part(child, numbered(&path, i), values, depth + 1, incomplete))
            .collect(),
        false => Vec::new(),
    };
    Part {
        path,
        charset: match data {
            Some(_) => Some("utf-8".into()),
            None => node["charset"].as_str().map(str::to_string),
        },
        protocol: None,
        smime_type: None,
        filename: node["name"].as_str().map(str::to_string),
        content_id: node["cid"].as_str().map(content_id),
        subject: None,
        attachment: node["disposition"].as_str() == Some("attachment"),
        size: node["size"].as_i64().unwrap_or_default(),
        data,
        children,
        mime_type,
    }
}

/// The blob of the part at `wanted`, walking from `node` at `path`.
fn blob_at(node: &Value, path: String, wanted: &str, depth: usize) -> Option<String> {
    if path == wanted {
        return node["blobId"].as_str().map(str::to_string);
    }
    if depth >= MAX_DEPTH {
        return None;
    }
    node["subParts"]
        .as_array()?
        .iter()
        .enumerate()
        .find_map(|(i, child)| blob_at(child, numbered(&path, i), wanted, depth + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email() -> Value {
        json!({
            "headers": [{ "name": "Subject", "value": " Report" }],
            "bodyStructure": { "type": "multipart/mixed", "subParts": [
                { "type": "multipart/alternative", "subParts": [
                    { "partId": "1", "type": "text/plain", "charset": "iso-8859-1", "size": 5 },
                    { "partId": "2", "type": "text/html", "size": 12 }
                ]},
                { "partId": "3", "blobId": "B3", "type": "application/pdf", "name": "report.pdf",
                  "disposition": "attachment", "size": 4000000 },
                { "partId": "4", "blobId": "B4", "type": "image/png", "cid": "<logo@x>", "size": 9 }
            ]},
            "bodyValues": {
                "1": { "value": "Héllo", "isTruncated": false },
                "2": { "value": "<p>Héllo</p>", "isTruncated": false }
            }
        })
    }

    #[test]
    fn a_structure_numbers_its_parts_as_imap_does_and_carries_the_texts() {
        let parts = parts_of(&email());
        assert_eq!(parts.headers, [("Subject".into(), "Report".into())]);
        assert_eq!(parts.root.path, "");
        let text = &parts.root.children[0].children[0];
        assert_eq!(text.path, "1.1");
        assert_eq!(text.data.as_deref(), Some("Héllo".as_bytes()));
        assert_eq!(text.charset.as_deref(), Some("utf-8"), "the server decoded it");
        let file = &parts.root.children[1];
        assert_eq!((file.path.as_str(), file.attachment, file.data.is_none()), ("2", true, true));
        assert_eq!(parts.root.children[2].content_id.as_deref(), Some("logo@x"));
        assert!(!parts.incomplete);
        let body = mailrs_mime::body(&parts);
        assert!(body.html.unwrap_or_default().contains("Héllo"));
    }

    #[test]
    fn a_part_is_found_by_its_path_for_its_blob() {
        let email = email();
        let structure = &email["bodyStructure"];
        assert_eq!(blob_at(structure, root_path(structure), "2", 0).as_deref(), Some("B3"));
        assert_eq!(blob_at(structure, root_path(structure), "9", 0), None);
    }

    #[test]
    fn a_truncated_text_marks_the_parts_incomplete() {
        let email = json!({
            "bodyStructure": { "partId": "1", "type": "text/plain" },
            "bodyValues": { "1": { "value": "Hel", "isTruncated": true } }
        });
        let parts = parts_of(&email);
        assert_eq!(parts.root.path, "1");
        assert!(parts.incomplete);
    }
}
