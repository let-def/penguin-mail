//! An in-memory JMAP server for tests and the demo, as the JMAP adapter
//! sees one through `JmapApi`. It keeps mailboxes, messages with their
//! raw bytes, a change log that `Email/changes` reads, and what was sent.
//! A test changes mail as another device would with `set_keyword` and
//! `move_to`, and reads back what the adapter did.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mailrs_domain::EpochMillis;
use mailrs_jmap::{
    Changed, Email, EmailAddress, Got, Identity, JmapError, Mailbox, Queried, SetAnswer, SetError, Submitted, Thread,
};
use serde_json::{Map, Value, json};

use crate::services::JmapApi;

type Answer<T> = Result<T, JmapError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Logged {
    Created,
    Updated,
    Destroyed,
}

/// One stored message: what `Email/get` answers, and its bytes.
#[derive(Debug, Clone)]
pub struct Stored {
    pub email: Email,
    pub raw: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct JmapState {
    pub mailboxes: Vec<Mailbox>,
    pub emails: BTreeMap<String, Stored>,
    pub identities: Vec<Identity>,
    /// What went out: the raw message and the identity it went as.
    pub sent: Vec<(Vec<u8>, String)>,
    state: u64,
    log: Vec<(u64, String, Logged)>,
    /// Changes before this state are no longer kept, and asking for them
    /// answers `cannotCalculateChanges`.
    pub forgotten: u64,
    next: u64,
}

impl JmapState {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    fn log(&mut self, id: &str, what: Logged) {
        self.state += 1;
        self.log.push((self.state, id.to_string(), what));
    }

    pub fn role(&self, role: &str) -> String {
        self.mailboxes
            .iter()
            .find(|m| m.role.as_deref() == Some(role))
            .map(|m| m.id.clone())
            .unwrap_or_default()
    }
}

pub struct FakeJmap {
    state: Mutex<JmapState>,
}

impl Default for FakeJmap {
    fn default() -> Self {
        FakeJmap::new("me@fastmail.com")
    }
}

impl FakeJmap {
    /// A server holding the standard mailboxes and one identity, `own`.
    pub fn new(own: &str) -> FakeJmap {
        let mailbox = |id: &str, name: &str, role: &str| Mailbox {
            id: id.into(),
            name: name.into(),
            role: Some(role.into()),
            is_subscribed: true,
            ..Mailbox::default()
        };
        let state = JmapState {
            mailboxes: vec![
                mailbox("mb-inbox", "Inbox", "inbox"),
                mailbox("mb-sent", "Sent", "sent"),
                mailbox("mb-drafts", "Drafts", "drafts"),
                mailbox("mb-trash", "Trash", "trash"),
                mailbox("mb-junk", "Spam", "junk"),
                mailbox("mb-archive", "Archive", "archive"),
            ],
            identities: vec![Identity {
                id: "id-own".into(),
                name: "Me".into(),
                email: own.into(),
                ..Identity::default()
            }],
            ..JmapState::default()
        };
        FakeJmap {
            state: Mutex::new(state),
        }
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut JmapState) -> T) -> T {
        f(&mut self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, JmapState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Files `raw` in the mailbox with `role` with `keywords`, received at
    /// `at`, as mail arriving does. Answers its id.
    pub fn deliver(&self, role: &str, raw: &[u8], keywords: &[&str], at: EpochMillis) -> String {
        let mut state = self.lock();
        let mailbox = state.role(role);
        file(&mut state, raw, &[&mailbox], keywords, at)
    }

    /// Sets or clears a keyword, as another device would.
    pub fn set_keyword(&self, id: &str, keyword: &str, on: bool) {
        let mut state = self.lock();
        if let Some(stored) = state.emails.get_mut(id) {
            match on {
                true => stored.email.keywords.insert(keyword.into(), true),
                false => stored.email.keywords.remove(keyword),
            };
            state.log(id, Logged::Updated);
        }
    }

    /// Moves a message into the mailbox with `role`, as another device
    /// would.
    pub fn move_to(&self, id: &str, role: &str) {
        let mut state = self.lock();
        let mailbox = state.role(role);
        if let Some(stored) = state.emails.get_mut(id) {
            stored.email.mailbox_ids = BTreeMap::from([(mailbox, true)]);
            state.log(id, Logged::Updated);
        }
    }

    /// Deletes a message for good, as another device would.
    pub fn remove(&self, id: &str) {
        let mut state = self.lock();
        if state.emails.remove(id).is_some() {
            state.log(id, Logged::Destroyed);
        }
    }

    /// The message `id` as the server holds it.
    pub fn email(&self, id: &str) -> Option<Email> {
        self.lock().emails.get(id).map(|s| s.email.clone())
    }
}

/// Stores `raw` as a new message and logs it.
fn file(state: &mut JmapState, raw: &[u8], mailboxes: &[&str], keywords: &[&str], at: EpochMillis) -> String {
    let summary = mailrs_mime::summary(raw);
    let id = state.id("e");
    let address = |a: &mailrs_domain::Address| EmailAddress {
        name: a.name.clone(),
        email: a.email.clone(),
    };
    // A reply joins the thread of the message it names.
    let parent = summary.in_reply_to.clone().or_else(|| summary.references.last().cloned());
    let thread = parent
        .and_then(|p| {
            state
                .emails
                .values()
                .find(|s| s.email.message_id.iter().flatten().any(|m| format!("<{m}>") == p || *m == p))
                .map(|s| s.email.thread_id.clone())
        })
        .unwrap_or_else(|| format!("t{id}"));
    let received = chrono::DateTime::from_timestamp_millis(at)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let email = Email {
        id: id.clone(),
        blob_id: format!("B{id}"),
        thread_id: thread,
        mailbox_ids: mailboxes.iter().map(|m| (m.to_string(), true)).collect(),
        keywords: keywords.iter().map(|k| (k.to_string(), true)).collect(),
        size: raw.len() as u64,
        received_at: Some(received),
        message_id: summary
            .message_id
            .map(|m| vec![m.trim_matches(['<', '>']).to_string()]),
        from: summary.from.as_ref().map(|a| vec![address(a)]),
        to: Some(summary.to.iter().map(address).collect()),
        cc: Some(summary.cc.iter().map(address).collect()),
        subject: Some(summary.subject),
        has_attachment: summary.has_files,
        preview: summary.snippet,
        list_unsubscribe: summary.list_unsubscribe,
        list_unsubscribe_post: summary.list_unsubscribe_post,
        ..Email::default()
    };
    state.emails.insert(id.clone(), Stored { email, raw: raw.to_vec() });
    state.log(&id, Logged::Created);
    id
}

/// A patch path's last step, with RFC 6901's escapes undone.
fn unescape(step: &str) -> String {
    step.replace("~1", "/").replace("~0", "~")
}

/// Whether `email` matches `filter`, for the conditions the adapter sends.
fn matches(email: &Stored, filter: &Value) -> bool {
    let e = &email.email;
    if let Some(operator) = filter["operator"].as_str() {
        let conditions = filter["conditions"].as_array().cloned().unwrap_or_default();
        let mut each = conditions.iter().map(|c| matches(email, c));
        return match operator {
            "AND" => each.all(|m| m),
            "OR" => each.any(|m| m),
            _ => !each.any(|m| m),
        };
    }
    let Some(conditions) = filter.as_object() else {
        return true;
    };
    let contains = |haystack: &str, needle: &Value| {
        haystack.to_lowercase().contains(&needle.as_str().unwrap_or_default().to_lowercase())
    };
    let people = |list: &Option<Vec<EmailAddress>>| {
        list.iter()
            .flatten()
            .map(|a| format!("{} {}", a.name.clone().unwrap_or_default(), a.email))
            .collect::<Vec<_>>()
            .join(" ")
    };
    conditions.iter().all(|(name, value)| match name.as_str() {
        "inMailbox" => e.mailbox_ids.contains_key(value.as_str().unwrap_or_default()),
        "after" => e.received_at.as_deref().unwrap_or_default() >= value.as_str().unwrap_or_default(),
        "before" => e.received_at.as_deref().unwrap_or_default() < value.as_str().unwrap_or_default(),
        "hasKeyword" => e.keywords.contains_key(value.as_str().unwrap_or_default()),
        "notKeyword" => !e.keywords.contains_key(value.as_str().unwrap_or_default()),
        "hasAttachment" => e.has_attachment == value.as_bool().unwrap_or_default(),
        "minSize" => e.size >= value.as_u64().unwrap_or_default(),
        "from" => contains(&people(&e.from), value),
        "to" => contains(&people(&e.to), value),
        "subject" => contains(e.subject.as_deref().unwrap_or_default(), value),
        "text" => {
            let all = format!("{} {} {}", e.subject.clone().unwrap_or_default(), e.preview, people(&e.from));
            contains(&all, value)
        }
        "header" => {
            let wanted = value[1].as_str().unwrap_or_default().trim_matches(['<', '>']);
            e.message_id.iter().flatten().any(|m| m == wanted)
        }
        _ => false,
    })
}

impl JmapApi for FakeJmap {
    async fn mailboxes(&self) -> Answer<Got<Mailbox>> {
        Ok(Got {
            state: "m".into(),
            list: self.lock().mailboxes.clone(),
            not_found: Vec::new(),
        })
    }

    async fn set_mailboxes(&self, args: Value) -> Answer<SetAnswer> {
        let mut state = self.lock();
        let mut answer = SetAnswer::default();
        if let Some(create) = args["create"].as_object() {
            let mut created = Map::new();
            for (key, made) in create {
                let id = state.id("mb");
                state.mailboxes.push(Mailbox {
                    id: id.clone(),
                    name: made["name"].as_str().unwrap_or_default().into(),
                    parent_id: made["parentId"].as_str().map(str::to_string),
                    is_subscribed: true,
                    ..Mailbox::default()
                });
                created.insert(key.clone(), json!({ "id": id }));
            }
            answer.created = Some(created);
        }
        if let Some(update) = args["update"].as_object() {
            for (id, patch) in update {
                if let Some(mailbox) = state.mailboxes.iter_mut().find(|m| &m.id == id) {
                    if let Some(name) = patch["name"].as_str() {
                        mailbox.name = name.into();
                    }
                    if patch.get("parentId").is_some() {
                        mailbox.parent_id = patch["parentId"].as_str().map(str::to_string);
                    }
                }
            }
        }
        if let Some(destroy) = args["destroy"].as_array() {
            for id in destroy.iter().filter_map(Value::as_str) {
                state.mailboxes.retain(|m| m.id != id);
                let inside: Vec<String> = state
                    .emails
                    .iter()
                    .filter(|(_, s)| s.email.mailbox_ids.contains_key(id))
                    .map(|(e, _)| e.clone())
                    .collect();
                for email in inside {
                    state.emails.remove(&email);
                    state.log(&email, Logged::Destroyed);
                }
            }
        }
        Ok(answer)
    }

    async fn email_state(&self) -> Answer<String> {
        Ok(self.lock().state.to_string())
    }

    async fn email_changes(&self, since: &str, most: u64) -> Answer<Changed> {
        let state = self.lock();
        let since: u64 = since.parse().map_err(|_| JmapError::StateLost)?;
        if since < state.forgotten {
            return Err(JmapError::StateLost);
        }
        let later: Vec<&(u64, String, Logged)> = state.log.iter().filter(|(at, _, _)| *at > since).collect();
        let taken = &later[..later.len().min(most as usize)];
        let mut changed = Changed {
            old_state: since.to_string(),
            new_state: taken.last().map_or(since, |(at, _, _)| *at).to_string(),
            has_more_changes: taken.len() < later.len(),
            ..Changed::default()
        };
        for (_, id, what) in taken {
            let list = match what {
                Logged::Created => &mut changed.created,
                Logged::Updated => &mut changed.updated,
                Logged::Destroyed => &mut changed.destroyed,
            };
            if !list.contains(id) {
                list.push(id.clone());
            }
        }
        changed.updated.retain(|id| !changed.created.contains(id));
        Ok(changed)
    }

    async fn query_emails(&self, filter: Value, position: u64, limit: u64) -> Answer<Queried> {
        let state = self.lock();
        let mut found: Vec<&Stored> = state.emails.values().filter(|s| matches(s, &filter)).collect();
        found.sort_by(|a, b| b.email.received_at.cmp(&a.email.received_at).then(b.email.id.cmp(&a.email.id)));
        Ok(Queried {
            query_state: "q".into(),
            ids: found
                .into_iter()
                .skip(position as usize)
                .take(limit as usize)
                .map(|s| s.email.id.clone())
                .collect(),
            position,
            total: None,
        })
    }

    async fn emails(&self, ids: &[String]) -> Answer<Got<Email>> {
        let state = self.lock();
        let mut got = Got {
            state: state.state.to_string(),
            list: Vec::new(),
            not_found: Vec::new(),
        };
        for id in ids {
            match state.emails.get(id) {
                Some(stored) => got.list.push(stored.email.clone()),
                None => got.not_found.push(id.clone()),
            }
        }
        Ok(got)
    }

    async fn email_threads(&self, ids: &[String]) -> Answer<Got<Email>> {
        self.emails(ids).await
    }

    async fn threads(&self, ids: &[String]) -> Answer<Got<Thread>> {
        let state = self.lock();
        let mut got = Got {
            state: "t".into(),
            list: Vec::new(),
            not_found: Vec::new(),
        };
        for id in ids {
            let mut emails: Vec<&Stored> = state.emails.values().filter(|s| &s.email.thread_id == id).collect();
            emails.sort_by(|a, b| a.email.received_at.cmp(&b.email.received_at));
            match emails.is_empty() {
                true => got.not_found.push(id.clone()),
                false => got.list.push(Thread {
                    id: id.clone(),
                    email_ids: emails.iter().map(|s| s.email.id.clone()).collect(),
                }),
            }
        }
        Ok(got)
    }

    /// One text part holding what the message's body reads as.
    async fn structure(&self, id: &str) -> Answer<Value> {
        let state = self.lock();
        let stored = state.emails.get(id).ok_or(JmapError::NotFound)?;
        let body = mailrs_mime::read(&stored.raw);
        Ok(json!({
            "id": id,
            "headers": [{ "name": "Subject", "value": stored.email.subject }],
            "bodyStructure": { "partId": "1", "blobId": stored.email.blob_id, "type": "text/plain" },
            "bodyValues": { "1": { "value": body.text.unwrap_or_default(), "isTruncated": false } }
        }))
    }

    async fn part(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        self.raw(blob_id, limit).await
    }

    async fn set_emails(&self, update: Map<String, Value>, destroy: &[String]) -> Answer<SetAnswer> {
        let mut state = self.lock();
        let mut answer = SetAnswer::default();
        let not_found = || SetError {
            kind: "notFound".into(),
            description: None,
        };
        for (id, patch) in update {
            let Some(stored) = state.emails.get_mut(&id) else {
                answer.not_updated.get_or_insert_default().insert(id, not_found());
                continue;
            };
            let email = &mut stored.email;
            for (path, value) in patch.as_object().into_iter().flatten() {
                if path == "mailboxIds" {
                    email.mailbox_ids = serde_json::from_value(value.clone()).unwrap_or_default();
                } else if let Some(mailbox) = path.strip_prefix("mailboxIds/") {
                    match value.as_bool() {
                        Some(true) => email.mailbox_ids.insert(unescape(mailbox), true),
                        _ => email.mailbox_ids.remove(&unescape(mailbox)),
                    };
                } else if let Some(keyword) = path.strip_prefix("keywords/") {
                    match value.as_bool() {
                        Some(true) => email.keywords.insert(unescape(keyword), true),
                        _ => email.keywords.remove(&unescape(keyword)),
                    };
                }
            }
            state.log(&id, Logged::Updated);
        }
        for id in destroy {
            if state.emails.remove(id).is_some() {
                state.log(id, Logged::Destroyed);
                answer.destroyed.get_or_insert_default().push(id.clone());
            } else {
                answer.not_destroyed.get_or_insert_default().insert(id.clone(), not_found());
            }
        }
        Ok(answer)
    }

    async fn import(&self, raw: Vec<u8>, mailbox_ids: &[&str], keywords: &[&str]) -> Answer<String> {
        let now = chrono::Utc::now().timestamp_millis();
        Ok(file(&mut self.lock(), &raw, mailbox_ids, keywords, now))
    }

    async fn raw(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        let state = self.lock();
        let stored = state
            .emails
            .values()
            .find(|s| s.email.blob_id == blob_id)
            .ok_or(JmapError::NotFound)?;
        match stored.raw.len() > limit {
            true => Err(JmapError::TooLarge { limit }),
            false => Ok(stored.raw.clone()),
        }
    }

    async fn identities(&self) -> Answer<Vec<Identity>> {
        Ok(self.lock().identities.clone())
    }

    async fn send(&self, raw: Vec<u8>, identity_id: &str, _drafts: &str, sent: &str) -> Answer<Submitted> {
        let mut state = self.lock();
        if !state.identities.iter().any(|i| i.id == identity_id) {
            return Err(JmapError::method("forbiddenFrom", "no such identity"));
        }
        let now = chrono::Utc::now().timestamp_millis();
        let id = file(&mut state, &raw, &[sent], &["$seen"], now);
        let thread_id = state.emails.get(&id).map(|s| s.email.thread_id.clone());
        state.sent.push((raw, identity_id.to_string()));
        Ok(Submitted { email_id: id, thread_id })
    }

    async fn submit(&self, email_id: &str, identity_id: &str, drafts: &str, sent: &str) -> Answer<()> {
        let mut state = self.lock();
        let stored = state.emails.get_mut(email_id).ok_or(JmapError::NotFound)?;
        stored.email.mailbox_ids.remove(drafts);
        stored.email.mailbox_ids.insert(sent.to_string(), true);
        stored.email.keywords.remove("$draft");
        let raw = stored.raw.clone();
        state.log(email_id, Logged::Updated);
        state.sent.push((raw, identity_id.to_string()));
        Ok(())
    }
}
