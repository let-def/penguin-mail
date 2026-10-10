//! The JMAP adapter: a Fastmail account's mail over JMAP (RFC 8620 and
//! 8621). `J` is the client, the real `mailrs_jmap::Jmap` or `FakeJmap`.
//! A message keeps its id when it moves, so the store's id is the
//! server's and no remote ref is kept. The feed is `Email/changes`, and
//! each changed message comes back whole, with every mailbox it is in and
//! every keyword it carries. JMAP stores any keyword, so topics reach the
//! person's other devices. Mailboxes are folders here, one per message,
//! as Fastmail's own interface files mail.

mod api;
mod feed;
mod fetch;
mod mailboxes;
mod outgoing;
mod search;
mod writes;

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use mailrs_domain::mailbox::keyword::{ANSWERED, DRAFT, FLAGGED, MUTED, SEEN};
use mailrs_domain::{MailSet, RemoteMailbox, Role, topic};
use mailrs_gmail::LabelColor;
use mailrs_jmap::JmapError;
use mailrs_mime::Parts;

pub use api::JmapApi;

use super::{
    Backfill, Changes, Found, IdentityService, KeywordsPage, MailBackend, MailCapabilities, RawMessage, Relocated,
    RemoteRef, SearchQuery, SendAsAddress, SyncState, Unapplied, Want,
};
use crate::api::{DraftRef, SavedDraft};
use crate::{BackendError, MailOp};

/// The most messages one `Email/set` names. Fastmail takes thousands; a
/// smaller batch keeps one refusal from failing a whole archive.
const BATCH: usize = 500;

/// What a JMAP account's mail service can do.
const JMAP: MailCapabilities = MailCapabilities {
    labels: false,
    server_threads: true,
    files_sent_mail: true,
    categories: false,
    delete_forever: true,
    batch_limit: BATCH,
    keywords: &[SEEN, FLAGGED, ANSWERED, DRAFT, MUTED, topic::FAMILY],
    native_search: false,
    renames: false,
    restates: true,
    tags: false,
    focus: false,
    local_mailboxes: false,
    files_muted_replies: false,
};

/// What the adapter needs to know about its account.
#[derive(Debug, Clone)]
pub struct JmapSettings {
    pub address: String,
    /// Who runs the server: "Fastmail".
    pub provider_name: String,
    pub window_days: i64,
}

/// What the adapter learned from its last mailbox listing. Its size
/// follows the account's mailboxes, never its mail.
#[derive(Debug, Default)]
pub(super) struct Known {
    /// A role to its mailbox's id.
    pub roles: BTreeMap<Role, String>,
    /// A mailbox's id to its path, `Parent/Child`.
    pub names: HashMap<String, String>,
    /// A mailbox's id to the conversations the server says it holds.
    pub totals: HashMap<String, u64>,
    pub listed: bool,
}

pub struct JmapMail<J> {
    api: Arc<J>,
    settings: Arc<JmapSettings>,
    known: Arc<Mutex<Known>>,
}

// By hand: a derive would ask for `J: Clone`, and the client is shared.
impl<J> Clone for JmapMail<J> {
    fn clone(&self) -> Self {
        JmapMail {
            api: Arc::clone(&self.api),
            settings: Arc::clone(&self.settings),
            known: Arc::clone(&self.known),
        }
    }
}

/// JMAP's errors as the backend errors every provider shares.
pub(crate) fn backend(err: JmapError) -> BackendError {
    match err {
        JmapError::Unauthorized => BackendError::NeedsReauth,
        JmapError::Network(detail) => BackendError::Offline(detail),
        JmapError::Throttled { retry_after } => BackendError::RateLimited(retry_after),
        JmapError::NotFound => BackendError::NotFound,
        JmapError::StateLost => BackendError::StateLost,
        JmapError::StateMismatch => BackendError::Changed,
        JmapError::Missing(_) => BackendError::Unsupported,
        other => BackendError::Refused(other.to_string()),
    }
}

/// The role an IANA mailbox role names, for the ones the store knows.
pub(super) fn role_of(name: &str) -> Option<Role> {
    Some(match name {
        "inbox" => Role::Inbox,
        "sent" => Role::Sent,
        "drafts" => Role::Drafts,
        "trash" => Role::Trash,
        "junk" => Role::Junk,
        "archive" => Role::Archive,
        "all" => Role::All,
        "important" => Role::Important,
        _ => return None,
    })
}

impl<J: JmapApi> JmapMail<J> {
    pub fn new(api: Arc<J>, settings: JmapSettings) -> Self {
        JmapMail {
            api,
            settings: Arc::new(settings),
            known: Arc::new(Mutex::new(Known::default())),
        }
    }

    pub(super) fn api(&self) -> &J {
        &self.api
    }

    pub(super) fn settings(&self) -> &JmapSettings {
        &self.settings
    }

    pub(super) fn known(&self) -> MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lists the mailboxes once, so roles and names are known before a
    /// message names a mailbox.
    pub(super) async fn synced(&self) -> Result<(), BackendError> {
        if !self.known().listed {
            self.list_mailboxes().await?;
        }
        Ok(())
    }

    /// The id of the mailbox with `role`, listing the mailboxes first if
    /// none were listed yet.
    pub(super) async fn role_id(&self, role: Role) -> Result<String, BackendError> {
        self.synced().await?;
        self.known().roles.get(&role).cloned().ok_or(BackendError::Unsupported)
    }
}

impl<J: JmapApi> MailBackend for JmapMail<J> {
    fn capabilities(&self) -> MailCapabilities {
        JMAP
    }

    fn provider_name(&self) -> &str {
        &self.settings.provider_name
    }

    fn person_waiting(&self) -> bool {
        false
    }

    async fn stand_by(&self, wait: Duration) {
        tokio::time::sleep(wait).await;
    }

    fn made_by_person(&self, id: &str) -> bool {
        !self.known().roles.values().any(|r| r == id)
    }

    async fn mailboxes(&self) -> Result<Vec<RemoteMailbox>, BackendError> {
        self.list_mailboxes().await
    }

    async fn changes(&self, since: Option<&SyncState>) -> Result<Changes, BackendError> {
        self.feed(since).await
    }

    async fn create_mailbox(&self, name: &str) -> Result<RemoteMailbox, BackendError> {
        self.make_mailbox(name).await
    }

    async fn rename_mailbox(&self, id: &str, name: &str) -> Result<RemoteMailbox, BackendError> {
        self.rename(id, name).await
    }

    async fn delete_mailbox(&self, id: &str) -> Result<(), BackendError> {
        self.remove_mailbox(id).await
    }

    /// JMAP keeps no colour for a mailbox.
    async fn set_mailbox_color(&self, _: &str, _: &LabelColor) -> Result<RemoteMailbox, BackendError> {
        Err(BackendError::Unsupported)
    }

    async fn mailbox_threads(&self, id: &str) -> Result<u64, BackendError> {
        self.synced().await?;
        self.known().totals.get(id).copied().ok_or(BackendError::NotFound)
    }

    /// The feed covers every mailbox, so there is nothing to follow.
    fn follow(&self, _: &str) -> bool {
        false
    }

    fn set_window_open(&self, _: bool) {}

    fn poll_interval(&self) -> Option<Duration> {
        None
    }

    /// Push over EventSource comes later; until then the looks are all
    /// there is.
    async fn watch(&self) {
        std::future::pending::<()>().await;
    }

    /// The feed restates every changed message whole, so there is nothing
    /// to page.
    async fn keywords_in(&self, _: &str, _: u32, _: RangeInclusive<u32>) -> Result<KeywordsPage, BackendError> {
        Ok(KeywordsPage::default())
    }

    async fn uidvalidity(&self, _: &str) -> Result<Option<u32>, BackendError> {
        Ok(None)
    }

    async fn keywords_stored(&self, _: &str) -> Result<&'static [&'static str], BackendError> {
        Ok(JMAP.keywords)
    }

    async fn backfill(&self, days: i64, cursor: Option<&str>) -> Result<Backfill, BackendError> {
        self.backfill_page(days, cursor).await
    }

    async fn window_ids(&self, days: i64, mailbox: Option<&str>) -> Result<Vec<RemoteRef>, BackendError> {
        self.window_refs(days, mailbox).await
    }

    async fn inbox_ids(&self) -> Result<Vec<RemoteRef>, BackendError> {
        self.inbox_refs().await
    }

    async fn search(&self, query: &SearchQuery, limit: usize) -> Result<Vec<RemoteRef>, BackendError> {
        self.find(query, limit).await
    }

    async fn find_sent(&self, message_id: &str) -> Result<Option<String>, BackendError> {
        self.sent_with(message_id).await
    }

    async fn fetch(&self, wants: Vec<Want>) -> Result<Found, BackendError> {
        self.fetch_metas(wants).await
    }

    async fn fetch_whole(&self, threads: Vec<String>) -> Result<Found, BackendError> {
        self.fetch_threads(threads).await
    }

    async fn fetch_raw(&self, ids: &[String]) -> Result<Vec<RawMessage>, BackendError> {
        self.raws(ids).await
    }

    async fn append(&self, raw: &[u8], mailbox: &str) -> Result<String, BackendError> {
        self.api.import(raw.to_vec(), &[mailbox], &[SEEN]).await.map_err(backend)
    }

    async fn fetch_structure(&self, id: &str) -> Result<Parts, BackendError> {
        self.structure_of(id).await
    }

    async fn fetch_part(&self, id: &str, path: &str) -> Result<Vec<u8>, BackendError> {
        self.part_of(id, path).await
    }

    fn mailbox_for(&self, role: Role) -> Option<String> {
        self.known().roles.get(&role).cloned()
    }

    fn set_of(&self, id: &str) -> MailSet {
        let role = self.known().roles.iter().find(|(_, mailbox)| *mailbox == id).map(|(r, _)| *r);
        match role {
            Some(role) => MailSet::Role(role),
            None => MailSet::Mailbox(id.to_string()),
        }
    }

    async fn apply(&self, messages: &[String], ops: &[MailOp]) -> Result<Vec<Relocated>, Unapplied> {
        self.write(messages, ops).await
    }

    async fn send(&self, raw: &[u8], _thread_id: Option<&str>) -> Result<String, BackendError> {
        self.send_raw(raw).await
    }

    /// The thread id is the server's to choose, so it is not used.
    async fn save_draft(
        &self,
        draft_id: Option<&str>,
        raw: &[u8],
        _thread_id: Option<&str>,
    ) -> Result<SavedDraft, BackendError> {
        self.save(draft_id, raw).await
    }

    async fn send_draft(&self, draft_id: &str) -> Result<String, BackendError> {
        self.send_saved(draft_id).await
    }

    async fn delete_draft(&self, draft_id: &str) -> Result<(), BackendError> {
        self.api.set_emails(Default::default(), &[draft_id.to_string()]).await.map_err(backend)?;
        Ok(())
    }

    async fn list_drafts(&self) -> Result<Vec<DraftRef>, BackendError> {
        self.drafts().await
    }
}

impl<J: JmapApi> IdentityService for JmapMail<J> {
    /// Every identity the server lets the account send as, the account's
    /// own address first.
    async fn identities(&self) -> Result<Vec<SendAsAddress>, BackendError> {
        let mut identities = self.api.identities().await.map_err(backend)?;
        let own = self.settings.address.to_lowercase();
        identities.sort_by_key(|i| i.email.to_lowercase() != own);
        Ok(identities
            .into_iter()
            .map(|i| SendAsAddress {
                default: i.email.to_lowercase() == own,
                name: Some(i.name).filter(|n| !n.is_empty()),
                signature: i.text_signature,
                email: i.email,
            })
            .collect())
    }
}
