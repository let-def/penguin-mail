//! The change feed: `Email/changes` from the state the store keeps. Each
//! created or changed message comes back whole, so it becomes a gain of
//! every mailbox and keyword it has, a loss of every listed mailbox and
//! every keyword the server stores that it lacks, and the topics it
//! carries whole, from which the engine takes the ones it lost.

use std::collections::BTreeSet;

use mailrs_domain::{Membership, topic};
use mailrs_jmap::Email;
use serde::{Deserialize, Serialize};

use super::{JMAP, JmapApi, JmapMail, backend};
use crate::BackendError;
use crate::services::{Changes, RemoteChange, SyncState};

/// The most changes one `Email/changes` answer names.
const CHANGES_PER_CALL: u64 = 500;

/// The most calls one look makes; the next look carries on from where
/// this one stopped.
const CALLS_PER_LOOK: usize = 10;

/// Where the feed stands, in the account's sync state.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct JmapState {
    pub email: String,
}

impl JmapState {
    fn read(state: &SyncState) -> Result<JmapState, BackendError> {
        serde_json::from_str(state.as_str()).map_err(|_| BackendError::StateLost)
    }

    fn written(&self) -> SyncState {
        SyncState::new(serde_json::to_string(self).unwrap_or_default())
    }
}

impl<J: JmapApi> JmapMail<J> {
    pub(super) async fn feed(&self, since: Option<&SyncState>) -> Result<Changes, BackendError> {
        self.synced().await?;
        let Some(since) = since else {
            let email = self.api().email_state().await.map_err(backend)?;
            return Ok(Changes {
                changes: Vec::new(),
                state: JmapState { email }.written(),
            });
        };
        let mut state = JmapState::read(since)?;
        let mut touched: BTreeSet<String> = BTreeSet::new();
        let mut destroyed: BTreeSet<String> = BTreeSet::new();
        for _ in 0..CALLS_PER_LOOK {
            let changed = self
                .api()
                .email_changes(&state.email, CHANGES_PER_CALL)
                .await
                .map_err(backend)?;
            touched.extend(changed.created.into_iter().chain(changed.updated));
            destroyed.extend(changed.destroyed);
            state.email = changed.new_state;
            if !changed.has_more_changes {
                break;
            }
        }
        // A message made and destroyed within the look is gone.
        touched.retain(|id| !destroyed.contains(id));
        let ids: Vec<String> = touched.into_iter().collect();
        let got = self.api().emails(&ids).await.map_err(backend)?;
        let mut changes: Vec<RemoteChange> = destroyed
            .into_iter()
            .chain(got.not_found)
            .map(|id| RemoteChange::Deleted { id })
            .collect();
        for email in &got.list {
            changes.extend(self.restated(email));
        }
        Ok(Changes {
            changes,
            state: state.written(),
        })
    }

    /// `email` as it stands now.
    fn restated(&self, email: &Email) -> Vec<RemoteChange> {
        let meta = self.meta_of(email);
        let held = &meta.held;
        let mut gained: Vec<Membership> = held.mailboxes.iter().cloned().map(Membership::Mailbox).collect();
        gained.extend(held.keywords.iter().cloned().map(Membership::Keyword));
        let mut lost: Vec<Membership> = self
            .known()
            .names
            .keys()
            .filter(|id| !held.mailboxes.contains(id))
            .cloned()
            .map(Membership::Mailbox)
            .collect();
        lost.extend(
            JMAP.keywords
                .iter()
                .filter(|k| **k != topic::FAMILY && !held.keywords.iter().any(|h| h == *k))
                .map(|k| Membership::Keyword(k.to_string())),
        );
        let topics: Vec<String> = held
            .keywords
            .iter()
            .filter(|k| topic::name_of_keyword(k).is_some())
            .cloned()
            .collect();
        let mut changes = vec![RemoteChange::Gained {
            id: meta.id.clone(),
            thread_id: meta.thread_id.clone(),
            memberships: gained,
        }];
        if !lost.is_empty() {
            changes.push(RemoteChange::Lost {
                id: meta.id.clone(),
                memberships: lost,
            });
        }
        changes.push(RemoteChange::TopicsCarried {
            id: meta.id,
            keywords: topics,
        });
        changes
    }
}
