//! The JMAP adapter over `FakeJmap`, through the same engine a real
//! Fastmail account runs.

mod contacts;
mod mail;
mod outgoing;

use std::collections::HashMap;
use std::sync::Arc;

use mailrs_domain::{ChangeEvent, EpochMillis, Memberships};
use mailrs_store::{Db, accounts, messages};

use crate::fake::{FakeJmap, raw_message};
use crate::tests::Connected;
use crate::{AccountServices, AccountSync, MailActions, now_millis};

pub(crate) struct Fastmail {
    pub fake: Arc<FakeJmap>,
    pub sync: Arc<AccountSync>,
    pub db: Db,
    pub events: async_channel::Receiver<ChangeEvent>,
    pub account_id: i64,
    _dir: tempfile::TempDir,
}

pub(crate) async fn fastmail() -> Fastmail {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("mail.db")).unwrap();
    let account_id = db
        .write(|c| accounts::insert_account(c, "me@fastmail.com", 0))
        .await
        .unwrap();
    let fake = Arc::new(FakeJmap::new("me@fastmail.com"));
    let (sender, events) = async_channel::unbounded();
    let services = AccountServices::fake_jmap(db.clone(), account_id, Arc::clone(&fake), "me@fastmail.com");
    let sync = Arc::new(AccountSync::new(account_id, services, db.clone(), sender));
    Fastmail { fake, sync, db, events, account_id, _dir: dir }
}

/// `days` days ago.
pub(crate) fn days_ago(days: i64) -> EpochMillis {
    now_millis() - days * 24 * 60 * 60 * 1000
}

impl Fastmail {
    /// Delivers a message from Ann to the inbox, `days` days old.
    pub fn deliver(&self, id: &str, subject: &str, days: i64, keywords: &[&str]) -> String {
        let at = days_ago(days);
        self.fake.deliver("inbox", &raw_message(id, subject, at, None), keywords, at)
    }

    pub async fn bootstrap(&self) {
        self.sync.bootstrap().await.unwrap();
        self.drain();
    }

    /// One look at the feed, as the engine's loop makes.
    pub async fn look(&self) {
        self.sync.incremental().await.unwrap();
    }

    pub async fn stored(&self, id: &str) -> bool {
        let (account, wanted) = (self.account_id, vec![id.to_string()]);
        self.db
            .read(move |c| messages::existing_ids(c, account, &wanted))
            .await
            .unwrap()
            .contains(id)
    }

    pub async fn held(&self, id: &str) -> Memberships {
        let (account, wanted) = (self.account_id, vec![id.to_string()]);
        self.db
            .read(move |c| messages::memberships_of(c, account, &wanted))
            .await
            .unwrap()
            .remove(id)
            .unwrap_or_default()
    }

    pub async fn thread_of(&self, id: &str) -> String {
        let (account, id) = (self.account_id, id.to_string());
        self.db
            .read(move |c| messages::thread_id_of(c, account, &id))
            .await
            .unwrap()
            .expect("stored")
    }

    pub fn actions(&self) -> MailActions<Connected> {
        MailActions::new(
            Arc::new(Connected(HashMap::from([(self.account_id, Arc::clone(&self.sync))]))),
            self.db.clone(),
            crate::OneClick::Fake(Arc::default()),
        )
    }

    pub fn drain(&self) -> Vec<ChangeEvent> {
        std::iter::from_fn(|| self.events.try_recv().ok()).collect()
    }
}
