//! The account's address book over JMAP for Contacts: JSContact cards
//! read into the people the app shows, and the few fields a change names
//! written back.
//!
//! The sync token is JSON, `{"state": "<card state>"}`. A page token is
//! `{"list": <position>, "state": "<card state>"}` while the first round
//! lists every card, holding the state read before it started so nothing
//! changed during the round is lost, or `{"since": "<card state>"}` while a
//! round of changes has more to read.

use mailrs_gmail::{ConnectionsPage, ContactFields, Person};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{JmapApi, JmapMail, backend};
use crate::BackendError;
use crate::services::ContactsService;

/// The cards one page reads.
const PAGE: u64 = 200;

/// The photo size past which the adapter stops reading.
const PHOTO_LIMIT: usize = 4 * 1024 * 1024;

/// The name `Person::photo_url` carries; only this adapter reads it.
const PHOTO_PREFIX: &str = "jmap:blob/";

#[derive(Serialize, Deserialize)]
struct SyncToken {
    state: String,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PageToken {
    List { list: u64, state: String },
    Since { since: String },
}

fn lost<T>(parsed: serde_json::Result<T>) -> Result<T, BackendError> {
    parsed.map_err(|_| BackendError::StateLost)
}

/// The entries of a JSContact map, such as `emails`, in the card's order
/// of preference: a lower `pref` first, one without a `pref` last, and
/// by key between equals so the order holds from one read to the next.
fn by_preference(map: &Value) -> Vec<&Value> {
    let mut entries: Vec<(&String, &Value)> = map.as_object().into_iter().flatten().collect();
    entries.sort_by_key(|(key, entry)| (entry["pref"].as_u64().unwrap_or(u64::MAX), (*key).clone()));
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// A card's name: the full name it carries, or its parts joined.
fn name_of(card: &Value) -> Option<String> {
    let name = &card["name"];
    if let Some(full) = name["full"].as_str().map(str::trim).filter(|f| !f.is_empty()) {
        return Some(full.to_string());
    }
    let parts: Vec<&str> = name["components"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["kind"].as_str() != Some("separator"))
        .filter_map(|c| c["value"].as_str())
        .filter(|v| !v.trim().is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

pub(super) fn person_of(card: &Value) -> Person {
    let photo = by_preference(&card["media"])
        .into_iter()
        .filter(|m| m["kind"].as_str() == Some("photo"))
        .find_map(|m| m["blobId"].as_str());
    Person {
        resource: card["id"].as_str().unwrap_or_default().to_string(),
        name: name_of(card),
        emails: by_preference(&card["emails"])
            .into_iter()
            .filter_map(|e| e["address"].as_str())
            .map(str::to_string)
            .collect(),
        photo_url: photo.map(|blob| format!("{PHOTO_PREFIX}{blob}")),
        organization: by_preference(&card["organizations"])
            .into_iter()
            .find_map(|o| o["name"].as_str())
            .map(str::to_string),
        phone: by_preference(&card["phones"])
            .into_iter()
            .find_map(|p| p["number"].as_str())
            .map(str::to_string),
    }
}

/// The card properties `fields` sets. Each list a change gives replaces
/// the card's whole list, as the People API's does; a field left `None`
/// stays out, so the card keeps what it had.
pub(super) fn properties_of(fields: &ContactFields) -> Map<String, Value> {
    let mut card = Map::new();
    if let Some(name) = &fields.name {
        card.insert("name".into(), json!({ "@type": "Name", "full": name }));
    }
    let numbered = |prefix: &str, values: &[String], field: &str, kind: &str| -> Value {
        Value::Object(
            values
                .iter()
                .enumerate()
                .map(|(i, value)| {
                    let mut entry = json!({ "@type": kind, field: value });
                    if i == 0 {
                        entry["pref"] = json!(1);
                    }
                    (format!("{prefix}{}", i + 1), entry)
                })
                .collect(),
        )
    };
    if let Some(emails) = &fields.emails {
        card.insert("emails".into(), numbered("e", emails, "address", "EmailAddress"));
    }
    if let Some(phones) = &fields.phones {
        card.insert("phones".into(), numbered("p", phones, "number", "Phone"));
    }
    if let Some(organization) = &fields.organization {
        let organizations = match organization.trim() {
            "" => json!({}),
            name => json!({ "o1": { "@type": "Organization", "name": name } }),
        };
        card.insert("organizations".into(), organizations);
    }
    card
}

impl<J: JmapApi> JmapMail<J> {
    /// The people of the cards `ids` names, and the ids the server no
    /// longer holds.
    async fn people(&self, ids: &[String]) -> Result<(Vec<Person>, Vec<String>), BackendError> {
        let got = self.api().cards(ids).await.map_err(backend)?;
        Ok((got.list.iter().map(person_of).collect(), got.not_found))
    }

    async fn changes_since(&self, since: String) -> Result<ConnectionsPage, BackendError> {
        let changed = self.api().card_changes(&since, PAGE).await.map_err(backend)?;
        let ids: Vec<String> = changed.created.into_iter().chain(changed.updated).collect();
        let (people, mut deleted) = self.people(&ids).await?;
        deleted.extend(changed.destroyed);
        let state = changed.new_state;
        let (next_page_token, next_sync_token) = match changed.has_more_changes {
            true => (Some(json!(PageToken::Since { since: state }).to_string()), None),
            false => (None, Some(json!(SyncToken { state }).to_string())),
        };
        Ok(ConnectionsPage { people, deleted, next_page_token, next_sync_token })
    }

    async fn list_from(&self, position: u64, state: String) -> Result<ConnectionsPage, BackendError> {
        let ids = self.api().query_cards(position, PAGE).await.map_err(backend)?.ids;
        let (people, _) = self.people(&ids).await?;
        let (next_page_token, next_sync_token) = match (ids.len() as u64) < PAGE {
            true => (None, Some(json!(SyncToken { state }).to_string())),
            false => (Some(json!(PageToken::List { list: position + PAGE, state }).to_string()), None),
        };
        Ok(ConnectionsPage { people, deleted: Vec::new(), next_page_token, next_sync_token })
    }

    /// The address book a new card goes in: the server's default, or its
    /// first.
    async fn default_book(&self) -> Result<String, BackendError> {
        let books = self.api().address_books().await.map_err(backend)?.list;
        books
            .iter()
            .find(|b| b.is_default)
            .or(books.first())
            .map(|b| b.id.clone())
            .ok_or(BackendError::Unsupported)
    }

    async fn person(&self, id: &str) -> Result<Person, BackendError> {
        let (mut people, _) = self.people(&[id.to_string()]).await?;
        people.pop().ok_or(BackendError::NotFound)
    }
}

impl<J: JmapApi> ContactsService for JmapMail<J> {
    async fn connections(
        &self,
        page_token: Option<&str>,
        sync_token: Option<&str>,
    ) -> Result<ConnectionsPage, BackendError> {
        match (page_token, sync_token) {
            (Some(page), _) => match lost(serde_json::from_str::<PageToken>(page))? {
                PageToken::List { list, state } => self.list_from(list, state).await,
                PageToken::Since { since } => self.changes_since(since).await,
            },
            (None, Some(sync)) => self.changes_since(lost(serde_json::from_str::<SyncToken>(sync))?.state).await,
            (None, None) => {
                // The state comes first, so a card changed while the
                // listing runs comes back with the next round of changes.
                let state = self.api().card_state().await.map_err(backend)?;
                self.list_from(0, state).await
            }
        }
    }

    async fn contact_photo(&self, url: &str) -> Result<Vec<u8>, BackendError> {
        let blob = url.strip_prefix(PHOTO_PREFIX).ok_or(BackendError::NotFound)?;
        self.api().card_blob(blob, PHOTO_LIMIT).await.map_err(backend)
    }

    async fn create_contact(&self, fields: &ContactFields) -> Result<Person, BackendError> {
        let book = self.default_book().await?;
        let mut card = properties_of(fields);
        card.insert("@type".into(), json!("Card"));
        card.insert("version".into(), json!("1.0"));
        card.insert("addressBookIds".into(), json!({ book: true }));
        let mut create = Map::new();
        create.insert("n".into(), Value::Object(card));
        let answer = self.api().set_cards(create, Map::new()).await.map_err(backend)?;
        if let Some((_, why)) = answer.refused().next() {
            return Err(BackendError::Refused(why.description.clone().unwrap_or_else(|| why.kind.clone())));
        }
        let id = answer.created_id("n").ok_or(BackendError::NotFound)?;
        self.person(&id).await
    }

    async fn update_contact(&self, resource: &str, fields: &ContactFields) -> Result<Person, BackendError> {
        let mut update = Map::new();
        update.insert(resource.to_string(), Value::Object(properties_of(fields)));
        let answer = self.api().set_cards(Map::new(), update).await.map_err(backend)?;
        if let Some((_, why)) = answer.refused().next() {
            return Err(match why.kind.as_str() {
                "notFound" => BackendError::NotFound,
                _ => BackendError::Refused(why.description.clone().unwrap_or_else(|| why.kind.clone())),
            });
        }
        self.person(resource).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_reads_as_the_person_the_app_shows() {
        let card = json!({
            "id": "k1",
            "name": { "components": [
                { "kind": "given", "value": "Ann" },
                { "kind": "separator", "value": " " },
                { "kind": "surname", "value": "Lee" }
            ]},
            "emails": {
                "work": { "address": "ann@work.example", "pref": 2 },
                "home": { "address": "ann@home.example", "pref": 1 },
                "old": { "address": "ann@old.example" }
            },
            "phones": { "p": { "number": "+351 900 000 000" } },
            "organizations": { "o": { "name": "Kites Ltd" } },
            "media": { "m": { "kind": "photo", "blobId": "B9" } }
        });
        let person = person_of(&card);
        assert_eq!(person.name.as_deref(), Some("Ann Lee"));
        assert_eq!(person.emails, ["ann@home.example", "ann@work.example", "ann@old.example"]);
        assert_eq!(person.phone.as_deref(), Some("+351 900 000 000"));
        assert_eq!(person.organization.as_deref(), Some("Kites Ltd"));
        assert_eq!(person.photo_url.as_deref(), Some("jmap:blob/B9"));
    }

    #[test]
    fn a_full_name_wins_over_its_parts() {
        let card = json!({ "id": "k", "name": { "full": "Dr Ann Lee", "components": [{ "kind": "given", "value": "Ann" }] } });
        assert_eq!(person_of(&card).name.as_deref(), Some("Dr Ann Lee"));
        assert_eq!(person_of(&json!({ "id": "k" })).name, None);
    }

    #[test]
    fn a_change_writes_only_the_fields_it_names() {
        let fields = ContactFields {
            emails: Some(vec!["a@x".into(), "b@x".into()]),
            organization: Some(String::new()),
            ..ContactFields::default()
        };
        let card = properties_of(&fields);
        assert!(!card.contains_key("name") && !card.contains_key("phones"));
        assert_eq!(card["emails"]["e1"]["address"], "a@x");
        assert_eq!(card["emails"]["e1"]["pref"], 1);
        assert_eq!(card["emails"]["e2"]["address"], "b@x");
        assert_eq!(card["organizations"], json!({}), "an empty company takes it away");
    }
}
