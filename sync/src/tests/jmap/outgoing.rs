//! Sending and drafts over JMAP.

use super::fastmail;
use crate::MailBackend;
use crate::services::IdentityService;

const RAW: &[u8] =
    b"From: Me <me@fastmail.com>\r\nTo: ann@example.com\r\nSubject: Hi\r\nMessage-ID: <abc@fastmail.example>\r\n\r\nHello\r\n";

#[tokio::test]
async fn a_message_goes_out_as_its_identity_and_lands_in_sent() {
    let h = fastmail().await;
    h.bootstrap().await;
    let mail = &h.sync.services().mail;
    let id = mail.send(RAW, None).await.unwrap();
    let sent = h.fake.email(&id).unwrap();
    assert_eq!(sent.mailbox_ids.keys().collect::<Vec<_>>(), ["mb-sent"]);
    assert_eq!(h.fake.with(|s| s.sent.clone()), [(RAW.to_vec(), "id-own".to_string())]);
    assert_eq!(mail.find_sent("abc@fastmail.example").await.unwrap(), Some(id));
}

#[tokio::test]
async fn saving_a_draft_twice_leaves_one_and_sending_it_moves_it_to_sent() {
    let h = fastmail().await;
    h.bootstrap().await;
    let mail = &h.sync.services().mail;
    let first = mail.save_draft(None, RAW, None).await.unwrap();
    let second = mail.save_draft(Some(&first.draft_id), RAW, None).await.unwrap();
    assert_ne!(first.draft_id, second.draft_id);
    let drafts = mail.list_drafts().await.unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].draft_id, second.draft_id);
    assert!(h.fake.email(&second.draft_id).unwrap().keywords.contains_key("$draft"));

    let sent = mail.send_draft(&second.draft_id).await.unwrap();
    assert_eq!(sent, second.draft_id);
    assert!(mail.list_drafts().await.unwrap().is_empty());
    let on_server = h.fake.email(&sent).unwrap();
    assert!(!on_server.keywords.contains_key("$draft"));
    assert_eq!(on_server.mailbox_ids.keys().collect::<Vec<_>>(), ["mb-sent"]);
}

#[tokio::test]
async fn the_account_sends_as_every_identity_the_server_offers() {
    let h = fastmail().await;
    h.fake.with(|s| {
        s.identities.insert(
            0,
            mailrs_jmap::Identity {
                id: "id-alias".into(),
                name: "Dana".into(),
                email: "dana@reyes.example".into(),
                text_signature: "— D".into(),
                ..Default::default()
            },
        )
    });
    let identities = h.sync.services().identities.identities().await.unwrap();
    assert_eq!(identities[0].email, "me@fastmail.com", "the account's own first");
    assert!(identities[0].default);
    assert_eq!(identities[1].signature, "— D");
}
