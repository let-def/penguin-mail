//! Mail groups: putting mail in a group by name, or taking it out of every
//! group. A message belongs to one group at a time, so joining one leaves
//! any other. Gmail keeps a group as a label named after it; every other
//! account keeps it as a keyword, which an IMAP server that stores any
//! keyword shares with the person's other devices and any other server
//! keeps on this computer.

use std::collections::BTreeSet;

use mailrs_domain::{AccountId, MailSet, Membership, Target, mail_group};
use mailrs_store::threads;

use super::{MailActions, label_id};
use crate::{Accounts, MailBackend, SyncError, TriageAction};

impl<A: Accounts> MailActions<A> {
    /// The spelling the store already holds for the group `name` among the
    /// targets' accounts, so "q3 launch" joins "Q3 Launch" rather than
    /// starting a second group; `name` cleaned up when no group matches.
    pub(super) async fn group_spelling(&self, targets: &[Target], name: &str) -> Option<String> {
        let name = mail_group::clean(name)?;
        let accounts: Vec<AccountId> = targets
            .iter()
            .map(|t| t.account_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let known = self
            .db
            .read(move |c| threads::group_names(c, &accounts))
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "could not read the mail groups");
                Vec::new()
            });
        Some(
            known
                .into_iter()
                .find(|k| mail_group::same(k, &name))
                .unwrap_or(name),
        )
    }

    /// The change that puts `target` in the group `name` and out of every
    /// other, or with `None` out of every group. `None` when the target
    /// already stands where the change would leave it.
    pub(super) async fn regroup(
        &self,
        target: &Target,
        name: Option<&str>,
    ) -> Result<Option<TriageAction>, String> {
        let account_id = target.account_id;
        let services = self
            .accounts
            .services(account_id)
            .ok_or_else(|| SyncError::UnknownAccount(account_id).to_string())?;
        let (thread, only) = (target.thread_id.clone(), target.message_id.clone());
        let held = self
            .db
            .read(move |c| threads::group_memberships(c, account_id, &thread, only.as_deref()))
            .await
            .map_err(|e| e.to_string())?;
        let labels = services.mail.capabilities().labels;
        let add = match name {
            None => None,
            Some(name) if labels => {
                let label = mail_group::label_of(name);
                let id = label_id(self.accounts.as_ref(), &self.db, account_id, &label, true)
                    .await
                    .map_err(|e| e.to_string())?;
                Some(Membership::Mailbox(id))
            }
            Some(name) => Some(Membership::Keyword(mail_group::keyword_of(name))),
        };
        let set_of = |m: &Membership| match m {
            Membership::Mailbox(id) => services.mail.set_of(id),
            Membership::Keyword(k) => MailSet::Keyword(k.clone()),
            Membership::Category(c) => MailSet::Category(c.clone()),
        };
        let remove: Vec<MailSet> = held
            .iter()
            .filter(|(membership, _)| Some(membership) != add.as_ref())
            .map(|(membership, _)| set_of(membership))
            .collect();
        // A whole thread gains the group on every message, so a message
        // without it still needs the change even when another has it.
        let add: Vec<MailSet> = add.iter().map(set_of).collect();
        if add.is_empty() && remove.is_empty() {
            return Ok(None);
        }
        Ok(Some(TriageAction::Relabel { add, remove }))
    }
}
