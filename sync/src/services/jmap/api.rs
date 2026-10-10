//! JMAP as the JMAP adapter uses it. A trait, so the adapter runs over
//! the real client or over `FakeJmap` alike, as the Microsoft adapter runs
//! over `GraphApi`. Each method is one call of `mailrs_jmap::Jmap`, named
//! and shaped as it names it.

use mailrs_jmap::{
    AddressBook, Changed, Email, Got, Identity, Jmap, JmapError, Mailbox, Queried, SetAnswer, Submitted, Thread,
};
use serde_json::{Map, Value};

type Answer<T> = Result<T, JmapError>;

pub trait JmapApi: Send + Sync + 'static {
    fn mailboxes(&self) -> impl Future<Output = Answer<Got<Mailbox>>> + Send;
    fn set_mailboxes(&self, args: Value) -> impl Future<Output = Answer<SetAnswer>> + Send;
    fn email_state(&self) -> impl Future<Output = Answer<String>> + Send;
    fn email_changes(&self, since: &str, most: u64) -> impl Future<Output = Answer<Changed>> + Send;
    fn query_emails(&self, filter: Value, position: u64, limit: u64) -> impl Future<Output = Answer<Queried>> + Send;
    fn emails(&self, ids: &[String]) -> impl Future<Output = Answer<Got<Email>>> + Send;
    fn email_threads(&self, ids: &[String]) -> impl Future<Output = Answer<Got<Email>>> + Send;
    fn threads(&self, ids: &[String]) -> impl Future<Output = Answer<Got<Thread>>> + Send;
    fn structure(&self, id: &str) -> impl Future<Output = Answer<Value>> + Send;
    fn part(&self, blob_id: &str, limit: usize) -> impl Future<Output = Answer<Vec<u8>>> + Send;
    fn set_emails(&self, update: Map<String, Value>, destroy: &[String]) -> impl Future<Output = Answer<SetAnswer>> + Send;
    fn import(&self, raw: Vec<u8>, mailbox_ids: &[&str], keywords: &[&str]) -> impl Future<Output = Answer<String>> + Send;
    fn raw(&self, blob_id: &str, limit: usize) -> impl Future<Output = Answer<Vec<u8>>> + Send;
    fn identities(&self) -> impl Future<Output = Answer<Vec<Identity>>> + Send;
    fn send(&self, raw: Vec<u8>, identity_id: &str, drafts: &str, sent: &str) -> impl Future<Output = Answer<Submitted>> + Send;
    fn submit(&self, email_id: &str, identity_id: &str, drafts: &str, sent: &str) -> impl Future<Output = Answer<()>> + Send;

    fn address_books(&self) -> impl Future<Output = Answer<Got<AddressBook>>> + Send;
    fn card_state(&self) -> impl Future<Output = Answer<String>> + Send;
    fn card_changes(&self, since: &str, most: u64) -> impl Future<Output = Answer<Changed>> + Send;
    fn query_cards(&self, position: u64, limit: u64) -> impl Future<Output = Answer<Queried>> + Send;
    fn cards(&self, ids: &[String]) -> impl Future<Output = Answer<Got<Value>>> + Send;
    fn set_cards(&self, create: Map<String, Value>, update: Map<String, Value>) -> impl Future<Output = Answer<SetAnswer>> + Send;
    fn card_blob(&self, blob_id: &str, limit: usize) -> impl Future<Output = Answer<Vec<u8>>> + Send;
}

impl JmapApi for Jmap {
    async fn mailboxes(&self) -> Answer<Got<Mailbox>> {
        Jmap::mailboxes(self).await
    }
    async fn set_mailboxes(&self, args: Value) -> Answer<SetAnswer> {
        Jmap::set_mailboxes(self, args).await
    }
    async fn email_state(&self) -> Answer<String> {
        Jmap::email_state(self).await
    }
    async fn email_changes(&self, since: &str, most: u64) -> Answer<Changed> {
        Jmap::email_changes(self, since, most).await
    }
    async fn query_emails(&self, filter: Value, position: u64, limit: u64) -> Answer<Queried> {
        Jmap::query_emails(self, filter, position, limit).await
    }
    async fn emails(&self, ids: &[String]) -> Answer<Got<Email>> {
        Jmap::emails(self, ids).await
    }
    async fn email_threads(&self, ids: &[String]) -> Answer<Got<Email>> {
        Jmap::email_threads(self, ids).await
    }
    async fn threads(&self, ids: &[String]) -> Answer<Got<Thread>> {
        Jmap::threads(self, ids).await
    }
    async fn structure(&self, id: &str) -> Answer<Value> {
        Jmap::structure(self, id).await
    }
    async fn part(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        Jmap::part(self, blob_id, limit).await
    }
    async fn set_emails(&self, update: Map<String, Value>, destroy: &[String]) -> Answer<SetAnswer> {
        Jmap::set_emails(self, update, destroy).await
    }
    async fn import(&self, raw: Vec<u8>, mailbox_ids: &[&str], keywords: &[&str]) -> Answer<String> {
        Jmap::import(self, raw, mailbox_ids, keywords).await
    }
    async fn raw(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        Jmap::raw(self, blob_id, limit).await
    }
    async fn identities(&self) -> Answer<Vec<Identity>> {
        Jmap::identities(self).await
    }
    async fn send(&self, raw: Vec<u8>, identity_id: &str, drafts: &str, sent: &str) -> Answer<Submitted> {
        Jmap::send(self, raw, identity_id, drafts, sent).await
    }
    async fn submit(&self, email_id: &str, identity_id: &str, drafts: &str, sent: &str) -> Answer<()> {
        Jmap::submit(self, email_id, identity_id, drafts, sent).await
    }
    async fn address_books(&self) -> Answer<Got<AddressBook>> {
        Jmap::address_books(self).await
    }
    async fn card_state(&self) -> Answer<String> {
        Jmap::card_state(self).await
    }
    async fn card_changes(&self, since: &str, most: u64) -> Answer<Changed> {
        Jmap::card_changes(self, since, most).await
    }
    async fn query_cards(&self, position: u64, limit: u64) -> Answer<Queried> {
        Jmap::query_cards(self, position, limit).await
    }
    async fn cards(&self, ids: &[String]) -> Answer<Got<Value>> {
        Jmap::cards(self, ids).await
    }
    async fn set_cards(&self, create: Map<String, Value>, update: Map<String, Value>) -> Answer<SetAnswer> {
        Jmap::set_cards(self, create, update).await
    }
    async fn card_blob(&self, blob_id: &str, limit: usize) -> Answer<Vec<u8>> {
        Jmap::card_blob(self, blob_id, limit).await
    }
}
