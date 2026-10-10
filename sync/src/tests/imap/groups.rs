//! Mail groups on an IMAP account: a keyword per group, which a server
//! that takes any keyword shares with the person's other devices.

use std::collections::HashMap;
use std::sync::Arc;

use mailrs_domain::{Target, mail_group};
use mailrs_store::threads;

use super::{days_ago, message};
use crate::fake::FakeImap;
use crate::tests::{Connected, ImapHarness, fake_settings, imap_harness_on};
use crate::{History, MailAction, MailActions};

/// An account on `imap` holding one message from Ann in the Inbox, with
/// the thread it sits in.
async fn one_message_on(imap: FakeImap) -> (ImapHarness, String) {
    imap.deliver_flagged("INBOX", &message("a", "Kites", ""), &[], days_ago(1));
    let h = imap_harness_on(imap, fake_settings()).await;
    h.bootstrap().await;
    let thread = h.thread_of("INBOX/1001/1").await.expect("threaded");
    (h, thread)
}

fn actions(h: &ImapHarness) -> MailActions<Connected> {
    MailActions::new(
        Arc::new(Connected(HashMap::from([(h.account_id, Arc::clone(&h.sync))]))),
        h.db.clone(),
        crate::OneClick::Fake(Arc::default()),
    )
}

/// The groups the stored row of `thread` names.
async fn groups(h: &ImapHarness, thread: &str) -> Vec<String> {
    let (account_id, thread) = (h.account_id, thread.to_string());
    h.db.read(move |c| threads::get_thread(c, account_id, &thread))
        .await
        .unwrap()
        .map(|row| row.groups)
        .unwrap_or_default()
}

fn server_flags(h: &ImapHarness) -> Vec<String> {
    h.imap.message("INBOX", 1).unwrap().flags.into_iter().collect()
}

async fn group(h: &ImapHarness, actions: &MailActions<Connected>, thread: &str, name: Option<&str>) {
    let target = Target::thread(h.account_id, thread);
    let outcome = actions
        .run(&[target], MailAction::Group(name.map(str::to_string)), History::Record)
        .await;
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
}

#[tokio::test]
async fn a_group_reaches_the_server_and_a_second_one_replaces_it() {
    let (h, thread) = one_message_on(FakeImap::new()).await;
    let actions = actions(&h);

    group(&h, &actions, &thread, Some("Q3 Launch")).await;
    assert_eq!(groups(&h, &thread).await, ["Q3 Launch"]);
    let q3 = mail_group::keyword_of("Q3 Launch");
    assert!(server_flags(&h).contains(&q3), "{:?}", server_flags(&h));

    group(&h, &actions, &thread, Some("Café")).await;
    assert_eq!(groups(&h, &thread).await, ["Café"]);
    let flags = server_flags(&h);
    assert!(flags.contains(&mail_group::keyword_of("Café")), "{flags:?}");
    assert!(!flags.contains(&q3), "{flags:?}");

    actions.undo().await.expect("an undo");
    assert_eq!(groups(&h, &thread).await, ["Q3 Launch"]);

    group(&h, &actions, &thread, None).await;
    assert!(groups(&h, &thread).await.is_empty());
    assert!(!server_flags(&h).iter().any(|f| f.starts_with("group:")), "{:?}", server_flags(&h));
}

#[tokio::test]
async fn a_name_joins_the_group_it_matches_ignoring_case() {
    let (h, thread) = one_message_on(FakeImap::new()).await;
    let actions = actions(&h);
    h.imap.remote_flag("INBOX", 1, &mail_group::keyword_of("Q3 Launch"), true);
    h.sync.incremental().await.unwrap();
    assert_eq!(groups(&h, &thread).await, ["Q3 Launch"]);

    group(&h, &actions, &thread, Some("  q3   launch ")).await;

    assert_eq!(groups(&h, &thread).await, ["Q3 Launch"]);
    assert_eq!(
        server_flags(&h).iter().filter(|f| f.starts_with("group:")).count(),
        1,
        "{:?}",
        server_flags(&h)
    );
}

#[tokio::test]
async fn a_group_another_device_takes_off_leaves_this_one_too() {
    let (h, thread) = one_message_on(FakeImap::new()).await;
    let actions = actions(&h);
    group(&h, &actions, &thread, Some("Q3")).await;

    h.imap.remote_flag("INBOX", 1, &mail_group::keyword_of("Q3"), false);
    h.sync.incremental().await.unwrap();

    assert!(groups(&h, &thread).await.is_empty());
}

#[tokio::test]
async fn a_server_that_refuses_keywords_keeps_the_group_on_this_computer() {
    let imap = FakeImap::new();
    imap.with(|s| {
        if let Some(inbox) = s.mailboxes.get_mut("INBOX") {
            inbox.permanent_flags = ["\\Seen", "\\Flagged", "\\Answered", "\\Draft", "\\Deleted"]
                .map(String::from)
                .to_vec();
        }
    });
    let (h, thread) = one_message_on(imap).await;
    let actions = actions(&h);

    group(&h, &actions, &thread, Some("Q3")).await;

    assert_eq!(groups(&h, &thread).await, ["Q3"]);
    assert!(!server_flags(&h).iter().any(|f| f.starts_with("group:")));
    h.imap.remote_flag("INBOX", 1, "\\Seen", true);
    h.sync.incremental().await.unwrap();
    assert_eq!(groups(&h, &thread).await, ["Q3"], "a sync leaves it alone");
}
