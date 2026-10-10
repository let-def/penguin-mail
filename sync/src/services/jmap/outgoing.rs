//! Sending and drafts. A message goes out as one of the identities the
//! server lets the account send as, the one its From address names; the
//! server files it as a draft, submits it, and moves it to Sent once it
//! went. A draft is a message in Drafts carrying `$draft`, and each save
//! replaces it with a new one.

use mailrs_domain::Role;
use mailrs_domain::mailbox::keyword::{DRAFT, SEEN};
use mailrs_jmap::Identity;

use super::{JmapApi, JmapMail, backend};
use crate::BackendError;
use crate::api::SavedDraft;

/// The identity that sends as `from`: the one with that address, or a
/// wildcard one for its domain (`*@example.com`, which Fastmail offers),
/// or the account's own when `from` names none of them.
pub(super) fn identity_for<'a>(identities: &'a [Identity], from: Option<&str>, own: &str) -> Option<&'a Identity> {
    let address = |wanted: &str| {
        let wanted = wanted.to_lowercase();
        identities.iter().find(|i| i.email.to_lowercase() == wanted)
    };
    let from = from.map(str::to_lowercase);
    let wildcard = || {
        let domain = from.as_deref()?.rsplit_once('@')?.1.to_string();
        address(&format!("*@{domain}"))
    };
    from.as_deref()
        .and_then(address)
        .or_else(wildcard)
        .or_else(|| address(own))
        .or_else(|| identities.first())
}

impl<J: JmapApi> JmapMail<J> {
    async fn identity_id(&self, raw: &[u8]) -> Result<String, BackendError> {
        let identities = self.api().identities().await.map_err(backend)?;
        let from = mailrs_mime::summary(raw).from.map(|a| a.email);
        identity_for(&identities, from.as_deref(), &self.settings().address)
            .map(|i| i.id.clone())
            .ok_or_else(|| BackendError::Refused("the server offers no address to send from".into()))
    }

    pub(super) async fn send_raw(&self, raw: &[u8]) -> Result<String, BackendError> {
        let identity = self.identity_id(raw).await?;
        let drafts = self.role_id(Role::Drafts).await?;
        let sent = self.role_id(Role::Sent).await?;
        let submitted = self
            .api()
            .send(raw.to_vec(), &identity, &drafts, &sent)
            .await
            .map_err(backend)?;
        Ok(submitted.email_id)
    }

    pub(super) async fn save(&self, draft_id: Option<&str>, raw: &[u8]) -> Result<SavedDraft, BackendError> {
        let drafts = self.role_id(Role::Drafts).await?;
        let id = self
            .api()
            .import(raw.to_vec(), &[&drafts], &[DRAFT, SEEN])
            .await
            .map_err(backend)?;
        if let Some(old) = draft_id.filter(|old| *old != id) {
            // The old copy may already be gone; the new one stands either way.
            if let Err(err) = self.api().set_emails(Default::default(), &[old.to_string()]).await {
                tracing::warn!(error = %err, "could not delete the draft's earlier copy");
            }
        }
        let threads = self.api().email_threads(std::slice::from_ref(&id)).await.map_err(backend)?;
        let thread_id = threads
            .list
            .first()
            .map(|e| e.thread_id.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| id.clone());
        Ok(SavedDraft {
            draft_id: id.clone(),
            message_id: id,
            thread_id,
        })
    }

    /// Sends the draft as the server holds it. The sent message is the
    /// draft itself, moved to Sent, so it keeps its id.
    pub(super) async fn send_saved(&self, draft_id: &str) -> Result<String, BackendError> {
        let raw = self
            .raws(&[draft_id.to_string()])
            .await?
            .pop()
            .ok_or(BackendError::NotFound)?;
        let identity = self.identity_id(&raw.bytes).await?;
        let drafts = self.role_id(Role::Drafts).await?;
        let sent = self.role_id(Role::Sent).await?;
        self.api()
            .submit(draft_id, &identity, &drafts, &sent)
            .await
            .map_err(backend)?;
        Ok(draft_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(id: &str, email: &str) -> Identity {
        Identity {
            id: id.into(),
            email: email.into(),
            ..Identity::default()
        }
    }

    #[test]
    fn a_message_goes_out_as_the_identity_its_from_names() {
        let all = [
            identity("own", "dana@fastmail.com"),
            identity("alias", "dana@reyes.example"),
            identity("any", "*@shop.example"),
        ];
        let pick = |from| identity_for(&all, from, "dana@fastmail.com").map(|i| i.id.as_str());
        assert_eq!(pick(Some("Dana@Reyes.example")), Some("alias"));
        assert_eq!(pick(Some("orders@shop.example")), Some("any"), "a wildcard covers its domain");
        assert_eq!(pick(Some("dana@elsewhere.example")), Some("own"));
        assert_eq!(pick(None), Some("own"));
        assert_eq!(identity_for(&[], Some("a@b"), "a@b"), None);
    }
}
