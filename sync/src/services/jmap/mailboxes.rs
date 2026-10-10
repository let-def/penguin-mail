//! Mailboxes: the listing, with each mailbox named by its path from the
//! top, and making, renaming and deleting a person's own.

use std::collections::{BTreeMap, HashMap};

use mailrs_domain::{MailboxKind, RemoteMailbox};
use mailrs_jmap::Mailbox;
use serde_json::{Value, json};

use super::{JmapApi, JmapMail, backend, role_of};
use crate::BackendError;

/// How deep a mailbox path goes before the listing stops climbing: a
/// parent loop on the server must not hang the listing.
const MOST_DEPTH: usize = 32;

/// Each mailbox's path, `Parent/Child`, from its parents.
fn paths(mailboxes: &[Mailbox]) -> HashMap<String, String> {
    let by_id: HashMap<&str, &Mailbox> = mailboxes.iter().map(|m| (m.id.as_str(), m)).collect();
    mailboxes
        .iter()
        .map(|mailbox| {
            let mut parts = vec![mailbox.name.as_str()];
            let mut parent = mailbox.parent_id.as_deref();
            while let Some(id) = parent {
                let Some(up) = by_id.get(id) else { break };
                if parts.len() == MOST_DEPTH {
                    break;
                }
                parts.push(up.name.as_str());
                parent = up.parent_id.as_deref();
            }
            parts.reverse();
            (mailbox.id.clone(), parts.join("/"))
        })
        .collect()
}

impl<J: JmapApi> JmapMail<J> {
    pub(super) async fn list_mailboxes(&self) -> Result<Vec<RemoteMailbox>, BackendError> {
        let got = self.api().mailboxes().await.map_err(backend)?;
        let names = paths(&got.list);
        let mut roles = BTreeMap::new();
        let mut totals = HashMap::new();
        let mut listed = Vec::with_capacity(got.list.len());
        for mailbox in &got.list {
            let role = mailbox.role.as_deref().and_then(role_of);
            if let Some(role) = role {
                roles.entry(role).or_insert_with(|| mailbox.id.clone());
            }
            totals.insert(mailbox.id.clone(), mailbox.total_threads);
            listed.push(RemoteMailbox {
                id: mailbox.id.clone(),
                name: names.get(&mailbox.id).cloned().unwrap_or_else(|| mailbox.name.clone()),
                kind: match role {
                    Some(_) => MailboxKind::System,
                    None => MailboxKind::Folder,
                },
                role,
                color: None,
                hidden: !mailbox.is_subscribed,
            });
        }
        let mut known = self.known();
        known.roles = roles;
        known.names = names;
        known.totals = totals;
        known.listed = true;
        Ok(listed)
    }

    /// The id of the mailbox at `path`, ignoring case.
    fn id_at(&self, path: &str) -> Option<String> {
        let wanted = path.to_lowercase();
        self.known()
            .names
            .iter()
            .find(|(_, name)| name.to_lowercase() == wanted)
            .map(|(id, _)| id.clone())
    }

    /// The parent a path names and the last part of it. A parent that is
    /// not there is refused, as the IMAP adapter refuses one.
    fn parent_of<'a>(&self, path: &'a str) -> Result<(Option<String>, &'a str), BackendError> {
        match path.rsplit_once('/') {
            None => Ok((None, path)),
            Some((parent, name)) => {
                let id = self.id_at(parent).ok_or(BackendError::NotFound)?;
                Ok((Some(id), name))
            }
        }
    }

    /// The listed mailbox `id`, after a write changed the listing.
    async fn relisted(&self, id: &str) -> Result<RemoteMailbox, BackendError> {
        self.list_mailboxes()
            .await?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or(BackendError::NotFound)
    }

    pub(super) async fn make_mailbox(&self, path: &str) -> Result<RemoteMailbox, BackendError> {
        self.synced().await?;
        let (parent, name) = self.parent_of(path.trim_matches('/'))?;
        let answer = self
            .api()
            .set_mailboxes(json!({ "create": { "n": { "name": name, "parentId": parent } } }))
            .await
            .map_err(backend)?;
        refused(&answer)?;
        let id = answer.created_id("n").ok_or(BackendError::NotFound)?;
        self.relisted(&id).await
    }

    pub(super) async fn rename(&self, id: &str, path: &str) -> Result<RemoteMailbox, BackendError> {
        self.synced().await?;
        let (parent, name) = self.parent_of(path.trim_matches('/'))?;
        let mut patch = serde_json::Map::new();
        patch.insert(id.to_string(), json!({ "name": name, "parentId": parent }));
        let answer = self
            .api()
            .set_mailboxes(json!({ "update": Value::Object(patch) }))
            .await
            .map_err(backend)?;
        refused(&answer)?;
        self.relisted(id).await
    }

    /// Deletes the mailbox and the mail in it, as deleting a folder does.
    pub(super) async fn remove_mailbox(&self, id: &str) -> Result<(), BackendError> {
        let answer = self
            .api()
            .set_mailboxes(json!({ "destroy": [id], "onDestroyRemoveEmails": true }))
            .await
            .map_err(backend)?;
        refused(&answer)?;
        self.list_mailboxes().await?;
        Ok(())
    }
}

/// The first refusal in a `set`, as an error.
fn refused(answer: &mailrs_jmap::SetAnswer) -> Result<(), BackendError> {
    match answer.refused().next() {
        Some((_, why)) => Err(BackendError::Refused(match &why.description {
            Some(description) => format!("{}: {description}", why.kind),
            None => why.kind.clone(),
        })),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(id: &str, name: &str, parent: Option<&str>) -> Mailbox {
        Mailbox {
            id: id.into(),
            name: name.into(),
            parent_id: parent.map(str::to_string),
            ..Mailbox::default()
        }
    }

    #[test]
    fn a_mailbox_is_named_by_its_path_from_the_top() {
        let names = paths(&[
            mailbox("w", "Work", None),
            mailbox("b", "Bugs", Some("w")),
            mailbox("o", "Old", Some("b")),
        ]);
        assert_eq!(names["o"], "Work/Bugs/Old");
        assert_eq!(names["w"], "Work");
    }

    #[test]
    fn a_parent_loop_on_the_server_does_not_hang_the_listing() {
        let names = paths(&[mailbox("a", "A", Some("b")), mailbox("b", "B", Some("a"))]);
        assert!(names["a"].split('/').count() <= MOST_DEPTH);
    }
}
