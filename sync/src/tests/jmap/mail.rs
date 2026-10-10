//! Reading mail and keeping it in step over JMAP.

use mailrs_domain::mailbox::keyword::{FLAGGED, SEEN};
use mailrs_domain::{MailSet, Role, Target, topic};
use mailrs_store::threads;

use super::fastmail;
use crate::{History, MailAction, MailBackend, TriageAction};

#[tokio::test]
async fn the_first_sync_lists_the_mailboxes_and_stores_the_window() {
    let h = fastmail().await;
    let fresh = h.deliver("a", "Kites", 1, &[]);
    let read = h.deliver("b", "Lunch", 2, &[SEEN]);
    let old = h.deliver("c", "Ancient", 400, &[SEEN]);
    h.bake_archive(&old);
    h.bootstrap().await;

    assert!(h.stored(&fresh).await && h.stored(&read).await);
    assert!(!h.stored(&old).await, "outside the window and not in the inbox");
    let held = h.held(&read).await;
    assert_eq!(held.keywords, [SEEN]);
    assert_eq!(held.mailboxes, ["mb-inbox"]);
    let mail = &h.sync.services().mail;
    assert_eq!(mail.mailbox_for(Role::Archive).as_deref(), Some("mb-archive"));
    assert_eq!(mail.set_of("mb-inbox"), MailSet::Role(Role::Inbox));
}

#[tokio::test]
async fn changes_made_elsewhere_reach_the_store() {
    let h = fastmail().await;
    let kites = h.deliver("a", "Kites", 1, &[]);
    let lunch = h.deliver("b", "Lunch", 1, &[SEEN]);
    h.bootstrap().await;

    let arrived = h.deliver("c", "New", 0, &[]);
    h.fake.set_keyword(&kites, SEEN, true);
    h.fake.set_keyword(&kites, FLAGGED, true);
    h.fake.set_keyword(&lunch, SEEN, false);
    h.fake.move_to(&lunch, "archive");
    h.look().await;

    assert!(h.stored(&arrived).await);
    let held = h.held(&kites).await;
    assert!(held.keywords.contains(&SEEN.into()) && held.keywords.contains(&FLAGGED.into()), "{held:?}");
    let held = h.held(&lunch).await;
    assert!(held.keywords.is_empty(), "{held:?}");
    assert_eq!(held.mailboxes, ["mb-archive"]);

    h.fake.set_keyword(&kites, FLAGGED, false);
    h.fake.remove(&arrived);
    h.look().await;
    assert!(!h.held(&kites).await.keywords.contains(&FLAGGED.into()));
    assert!(!h.stored(&arrived).await);
}

#[tokio::test]
async fn a_feed_the_server_forgot_lists_the_mail_again() {
    let h = fastmail().await;
    h.deliver("a", "Kites", 1, &[]);
    h.bootstrap().await;
    let later = h.deliver("b", "Later", 0, &[]);
    h.fake.with(|s| s.forgotten = u64::MAX);
    h.sync.incremental().await.unwrap();
    assert!(h.stored(&later).await);
}

#[tokio::test]
async fn archiving_moves_the_message_on_the_server() {
    let h = fastmail().await;
    let kites = h.deliver("a", "Kites", 1, &[]);
    h.bootstrap().await;
    let thread = h.thread_of(&kites).await;
    h.actions()
        .run(
            &[Target::thread(h.account_id, &thread)],
            MailAction::Triage(TriageAction::Archive),
            History::Record,
        )
        .await;
    let on_server = h.fake.email(&kites).unwrap();
    assert_eq!(on_server.mailbox_ids.keys().collect::<Vec<_>>(), ["mb-archive"]);
    assert_eq!(h.held(&kites).await.mailboxes, ["mb-archive"]);
}

#[tokio::test]
async fn a_topic_is_a_keyword_every_device_shares() {
    let h = fastmail().await;
    let kites = h.deliver("a", "Kites", 1, &[]);
    h.bootstrap().await;
    let thread = h.thread_of(&kites).await;
    let topics = || async {
        let (account, thread) = (h.account_id, thread.clone());
        h.db.read(move |c| threads::get_thread(c, account, &thread))
            .await
            .unwrap()
            .unwrap()
            .topics
    };

    let outcome = h
        .actions()
        .run(
            &[Target::thread(h.account_id, &thread)],
            MailAction::Topic(Some("Q3 Launch".into())),
            History::Record,
        )
        .await;
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
    let keyword = topic::keyword_of("Q3 Launch");
    assert!(h.fake.email(&kites).unwrap().keywords.contains_key(&keyword));
    assert_eq!(topics().await, ["Q3 Launch"]);

    h.fake.set_keyword(&kites, &keyword, false);
    h.look().await;
    assert!(topics().await.is_empty(), "taken off on another device");

    h.fake.set_keyword(&kites, &topic::keyword_of("Invoices"), true);
    h.look().await;
    assert_eq!(topics().await, ["Invoices"], "put on on another device");
}

#[tokio::test]
async fn a_search_goes_to_the_server_as_a_filter() {
    let h = fastmail().await;
    let lunch = h.deliver("a", "Lunch on Friday", 1, &[]);
    h.deliver("b", "Invoice", 1, &[]);
    h.bootstrap().await;
    let query = mailrs_domain::query::Query::term(mailrs_domain::query::Term::Subject("lunch".into()));
    let found = h
        .sync
        .services()
        .mail
        .search(&crate::SearchQuery::Tree(query), 10)
        .await
        .unwrap();
    assert_eq!(found.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), [lunch]);
}

#[tokio::test]
async fn a_large_message_opens_from_its_structure() {
    let h = fastmail().await;
    let kites = h.deliver("a", "Kites", 1, &[]);
    h.bootstrap().await;
    let parts = h.sync.services().mail.fetch_structure(&kites).await.unwrap();
    let body = mailrs_mime::body(&parts);
    assert!(body.text.unwrap_or_default().contains("Body of a"));
}

impl super::Fastmail {
    /// Files message `id` in the Archive, out of the inbox.
    fn bake_archive(&self, id: &str) {
        self.fake.move_to(id, "archive");
    }
}
