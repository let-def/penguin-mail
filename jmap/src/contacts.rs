//! JMAP for Contacts (RFC 9610): address books, and contact cards in
//! JSContact (RFC 9553). A card stays JSON here: the adapter reads the
//! few fields the app shows and writes only the ones a change names, so a
//! field this client does not know survives an edit.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::JmapError;
use crate::client::Jmap;
use crate::mail::{Changed, Got, Queried, SetAnswer};

pub const CONTACTS: &str = "urn:ietf:params:jmap:contacts";

/// The card properties the adapter reads.
pub const CARD_PROPERTIES: &[&str] = &[
    "id",
    "addressBookIds",
    "name",
    "emails",
    "phones",
    "organizations",
    "media",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressBook {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
}

impl Jmap {
    async fn contacts_account(&self) -> Result<String, JmapError> {
        Ok(self.session().await?.account_for(CONTACTS)?.to_string())
    }

    async fn contacts_call<T: serde::de::DeserializeOwned>(
        &self,
        name: &'static str,
        mut args: Value,
    ) -> Result<T, JmapError> {
        args["accountId"] = json!(self.contacts_account().await?);
        self.call(&[CONTACTS], name, args).await
    }

    pub async fn address_books(&self) -> Result<Got<AddressBook>, JmapError> {
        self.contacts_call("AddressBook/get", json!({ "ids": null })).await
    }

    /// Where the cards stand now, for a feed that starts here.
    pub async fn card_state(&self) -> Result<String, JmapError> {
        let got: Got<Value> = self.contacts_call("ContactCard/get", json!({ "ids": [] })).await?;
        Ok(got.state)
    }

    pub async fn card_changes(&self, since: &str, most: u64) -> Result<Changed, JmapError> {
        self.contacts_call(
            "ContactCard/changes",
            json!({ "sinceState": since, "maxChanges": most }),
        )
        .await
    }

    /// One page of every card's id.
    pub async fn query_cards(&self, position: u64, limit: u64) -> Result<Queried, JmapError> {
        self.contacts_call(
            "ContactCard/query",
            json!({ "position": position, "limit": limit, "calculateTotal": false }),
        )
        .await
    }

    /// The cards `ids` names, with [`CARD_PROPERTIES`].
    pub async fn cards(&self, ids: &[String]) -> Result<Got<Value>, JmapError> {
        if ids.is_empty() {
            return Ok(Got::default());
        }
        self.contacts_call(
            "ContactCard/get",
            json!({ "ids": ids, "properties": CARD_PROPERTIES }),
        )
        .await
    }

    /// Creates cards and changes others, as `create` and `update` say.
    pub async fn set_cards(
        &self,
        create: Map<String, Value>,
        update: Map<String, Value>,
    ) -> Result<SetAnswer, JmapError> {
        let mut args = json!({});
        if !create.is_empty() {
            args["create"] = Value::Object(create);
        }
        if !update.is_empty() {
            args["update"] = Value::Object(update);
        }
        self.contacts_call("ContactCard/set", args).await
    }

    /// The bytes of a contact's photo, by its blob.
    pub async fn card_blob(&self, blob_id: &str, limit: usize) -> Result<Vec<u8>, JmapError> {
        let account = self.contacts_account().await?;
        self.download(&account, blob_id, "application/octet-stream", limit)
            .await
    }
}
