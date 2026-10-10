//! Changes to mail: each message's operations as one `Email/set` patch,
//! messages in batches. A move replaces the message's whole set of
//! mailboxes, which is how a folder server files mail; adding a mailbox
//! or a keyword patches that one entry, so a change made on another
//! device to another keyword stands.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::{BATCH, JmapApi, JmapMail, backend};
use crate::services::{Relocated, Unapplied};
use crate::{BackendError, MailOp};

/// A name as one step of a JSON pointer (RFC 6901), as a patch path
/// needs it: a keyword may hold `/` or `~`.
fn step(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// The patch `ops` make to one message. `role_ids` gives the mailbox of
/// each role a move names.
pub(super) fn patch(ops: &[MailOp], role_id: impl Fn(mailrs_domain::Role) -> Option<String>) -> Result<Map<String, Value>, BackendError> {
    let mut whole: Option<BTreeMap<String, bool>> = None;
    let mut patch = Map::new();
    for op in ops {
        match op {
            MailOp::MoveToMailbox(id) => {
                patch.retain(|path, _| !path.starts_with("mailboxIds/"));
                whole = Some(BTreeMap::from([(id.clone(), true)]));
            }
            MailOp::MoveToRole(role) => {
                let id = role_id(*role).ok_or(BackendError::Unsupported)?;
                patch.retain(|path, _| !path.starts_with("mailboxIds/"));
                whole = Some(BTreeMap::from([(id, true)]));
            }
            MailOp::AddToMailbox(id) => match &mut whole {
                Some(set) => {
                    set.insert(id.clone(), true);
                }
                None => {
                    patch.insert(format!("mailboxIds/{}", step(id)), json!(true));
                }
            },
            MailOp::RemoveFromMailbox(id) => match &mut whole {
                Some(set) => {
                    set.remove(id);
                }
                None => {
                    patch.insert(format!("mailboxIds/{}", step(id)), Value::Null);
                }
            },
            MailOp::SetKeyword { keyword, on } => {
                let value = match on {
                    true => json!(true),
                    false => Value::Null,
                };
                patch.insert(format!("keywords/{}", step(keyword)), value);
            }
            // JMAP has no inbox categories.
            MailOp::SetCategory { .. } => {}
            MailOp::Destroy => return Err(BackendError::Unsupported),
        }
    }
    if let Some(set) = whole {
        patch.insert("mailboxIds".into(), json!(set));
    }
    Ok(patch)
}

impl<J: JmapApi> JmapMail<J> {
    pub(super) async fn write(&self, messages: &[String], ops: &[MailOp]) -> Result<Vec<Relocated>, Unapplied> {
        let refused = |taken: usize, error: BackendError| Unapplied {
            taken,
            error,
            relocated: Vec::new(),
        };
        if ops == [MailOp::Destroy] {
            return self.destroy(messages).await.map_err(|(taken, e)| refused(taken, e));
        }
        if ops.iter().any(|op| matches!(op, MailOp::MoveToRole(_))) {
            self.synced().await.map_err(|e| refused(0, e))?;
        }
        let patch = {
            let known = self.known();
            patch(ops, |role| known.roles.get(&role).cloned()).map_err(|e| refused(0, e))?
        };
        if patch.is_empty() {
            return Ok(Vec::new());
        }
        for (index, batch) in messages.chunks(BATCH).enumerate() {
            let before = index * BATCH;
            let update: Map<String, Value> = batch
                .iter()
                .map(|id| (id.clone(), Value::Object(patch.clone())))
                .collect();
            let answer = self
                .api()
                .set_emails(update, &[])
                .await
                .map_err(|e| refused(before, backend(e)))?;
            if let Some((at, error)) = first_refusal(batch, answer.not_updated.as_ref()) {
                return Err(refused(before + at, error));
            }
        }
        Ok(Vec::new())
    }

    async fn destroy(&self, messages: &[String]) -> Result<Vec<Relocated>, (usize, BackendError)> {
        for (index, batch) in messages.chunks(BATCH).enumerate() {
            let before = index * BATCH;
            let answer = self
                .api()
                .set_emails(Map::new(), batch)
                .await
                .map_err(|e| (before, backend(e)))?;
            if let Some((at, error)) = first_refusal(batch, answer.not_destroyed.as_ref()) {
                return Err((before + at, error));
            }
        }
        Ok(Vec::new())
    }
}

/// The first message of `batch` the server refused, by its place in the
/// batch, and why. A message already gone counts as done: there is
/// nothing left to change.
fn first_refusal(
    batch: &[String],
    refused: Option<&BTreeMap<String, mailrs_jmap::SetError>>,
) -> Option<(usize, BackendError)> {
    let refused = refused?;
    batch.iter().enumerate().find_map(|(at, id)| {
        let why = refused.get(id)?;
        match why.kind.as_str() {
            "notFound" => None,
            _ => Some((
                at,
                BackendError::Refused(why.description.clone().unwrap_or_else(|| why.kind.clone())),
            )),
        }
    })
}

#[cfg(test)]
mod tests {
    use mailrs_domain::Role;

    use super::*;

    fn roles(role: Role) -> Option<String> {
        (role == Role::Archive).then(|| "archive-id".into())
    }

    #[test]
    fn a_move_replaces_the_mailboxes_and_marks_patch_their_own_entry() {
        let ops = [
            MailOp::MoveToRole(Role::Archive),
            MailOp::SetKeyword { keyword: "$seen".into(), on: true },
            MailOp::SetKeyword { keyword: "$muted".into(), on: false },
        ];
        let patch = patch(&ops, roles).unwrap();
        assert_eq!(patch["mailboxIds"], json!({ "archive-id": true }));
        assert_eq!(patch["keywords/$seen"], json!(true));
        assert_eq!(patch["keywords/$muted"], Value::Null);
    }

    #[test]
    fn adding_a_mailbox_leaves_the_others() {
        let patch = patch(
            &[MailOp::AddToMailbox("a".into()), MailOp::RemoveFromMailbox("b".into())],
            roles,
        )
        .unwrap();
        assert_eq!(patch["mailboxIds/a"], json!(true));
        assert_eq!(patch["mailboxIds/b"], Value::Null);
        assert!(!patch.contains_key("mailboxIds"));
    }

    #[test]
    fn a_move_after_an_add_takes_the_add_into_the_new_set() {
        let patch = patch(
            &[MailOp::AddToMailbox("a".into()), MailOp::MoveToMailbox("b".into()), MailOp::AddToMailbox("c".into())],
            roles,
        )
        .unwrap();
        assert_eq!(patch.len(), 1, "{patch:?}");
        assert_eq!(patch["mailboxIds"], json!({ "b": true, "c": true }));
    }

    #[test]
    fn a_keyword_with_a_slash_is_escaped_in_its_path() {
        let patch = patch(&[MailOp::SetKeyword { keyword: "a/b~c".into(), on: true }], roles).unwrap();
        assert!(patch.contains_key("keywords/a~1b~0c"), "{patch:?}");
    }

    #[test]
    fn a_role_the_account_lacks_is_refused() {
        assert!(matches!(patch(&[MailOp::MoveToRole(Role::Junk)], roles), Err(BackendError::Unsupported)));
    }
}
