//! The address book over JMAP for Contacts.

use mailrs_gmail::ContactFields;
use serde_json::json;

use super::fastmail;
use crate::BackendError;
use crate::services::ContactsService;

fn card(name: &str, email: &str) -> serde_json::Value {
    json!({
        "@type": "Card",
        "addressBookIds": { "ab-default": true },
        "name": { "full": name },
        "emails": { "e": { "address": email } }
    })
}

#[tokio::test]
async fn the_first_read_lists_every_card_and_later_reads_bring_changes() {
    let h = fastmail().await;
    h.fake.put_card("k1", card("Ann Lee", "ann@example.com"));
    h.fake.put_card("k2", card("Bo Ng", "bo@example.com"));
    let contacts = h.sync.services().contacts.clone().unwrap();

    let first = contacts.connections(None, None).await.unwrap();
    let mut names: Vec<_> = first.people.iter().filter_map(|p| p.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["Ann Lee", "Bo Ng"]);
    assert!(first.next_page_token.is_none());
    let sync = first.next_sync_token.expect("a sync token");

    h.fake.put_card("k1", card("Ann Lee-Silva", "ann@example.com"));
    h.fake.remove_card("k2");
    h.fake.put_card("k3", card("Cy Oak", "cy@example.com"));
    let changed = contacts.connections(None, Some(&sync)).await.unwrap();
    let mut names: Vec<_> = changed.people.iter().filter_map(|p| p.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["Ann Lee-Silva", "Cy Oak"]);
    assert_eq!(changed.deleted, ["k2"]);

    let quiet = contacts
        .connections(None, changed.next_sync_token.as_deref())
        .await
        .unwrap();
    assert!(quiet.people.is_empty() && quiet.deleted.is_empty());
}

#[tokio::test]
async fn a_contact_added_here_goes_in_the_default_address_book_and_an_edit_keeps_the_rest() {
    let h = fastmail().await;
    let contacts = h.sync.services().contacts.clone().unwrap();
    let added = contacts
        .create_contact(&ContactFields {
            name: Some("Dana Reyes".into()),
            emails: Some(vec!["dana@reyes.example".into()]),
            ..ContactFields::default()
        })
        .await
        .unwrap();
    assert_eq!(added.name.as_deref(), Some("Dana Reyes"));
    let stored = h.fake.with(|s| s.cards[&added.resource].clone());
    assert_eq!(stored["addressBookIds"], json!({ "ab-default": true }));
    assert_eq!(stored["@type"], "Card");

    let changed = contacts
        .update_contact(&added.resource, &ContactFields { phones: Some(vec!["+351 1".into()]), ..ContactFields::default() })
        .await
        .unwrap();
    assert_eq!(changed.phone.as_deref(), Some("+351 1"));
    assert_eq!(changed.name.as_deref(), Some("Dana Reyes"), "the name stays");
    assert_eq!(changed.emails, ["dana@reyes.example"]);
}

#[tokio::test]
async fn a_photo_comes_from_its_blob() {
    let h = fastmail().await;
    let mut ann = card("Ann Lee", "ann@example.com");
    ann["media"] = json!({ "m": { "kind": "photo", "blobId": "P1" } });
    h.fake.put_card("k1", ann);
    h.fake.with(|s| s.blobs.insert("P1".into(), b"png".to_vec()));
    let contacts = h.sync.services().contacts.clone().unwrap();
    let people = contacts.connections(None, None).await.unwrap().people;
    let url = people[0].photo_url.clone().expect("a photo");
    assert_eq!(contacts.contact_photo(&url).await.unwrap(), b"png");
}

#[tokio::test]
async fn a_token_without_contacts_says_the_server_cannot() {
    let h = fastmail().await;
    h.fake.with(|s| s.no_contacts = true);
    let contacts = h.sync.services().contacts.clone().unwrap();
    assert!(matches!(contacts.connections(None, None).await, Err(BackendError::Unsupported)));
}
