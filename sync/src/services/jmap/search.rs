//! Searching with `Email/query`: a query tree becomes a JMAP filter, one
//! condition for each term and an operator for each `And`, `Or` and
//! `Not`. What a person types arrives already parsed into a tree; the
//! words alone go to the server's own full-text search.

use std::collections::{BTreeMap, HashMap};

use chrono::{Local, NaiveDate, TimeZone, Utc};
use mailrs_domain::mailbox::keyword::{FLAGGED, SEEN};
use mailrs_domain::query::{Query, Term};
use mailrs_domain::{MailSet, Role};
use serde_json::{Value, json};

use super::{JmapApi, JmapMail, backend};
use crate::BackendError;
use crate::services::{RemoteRef, SearchQuery};

/// A term JMAP has no condition for, such as an inbox category.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Unsayable;

/// What a filter needs to know of the account: the mailbox of each role,
/// and each mailbox's path for a smart mailbox that names one.
pub(super) struct Places<'a> {
    pub roles: &'a BTreeMap<Role, String>,
    pub names: &'a HashMap<String, String>,
}

/// Local midnight of `day`, as JMAP reads a time.
fn midnight(day: NaiveDate) -> String {
    let local = day.and_hms_opt(0, 0, 0).and_then(|t| Local.from_local_datetime(&t).earliest());
    local
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| day.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(super) fn filter_of(query: &Query, places: &Places, today: NaiveDate) -> Result<Value, Unsayable> {
    let all = |queries: &[Query], operator: &str| -> Result<Value, Unsayable> {
        let conditions = queries
            .iter()
            .map(|q| filter_of(q, places, today))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "operator": operator, "conditions": conditions }))
    };
    match query {
        // An empty `And` holds for every message, which an empty filter
        // says; JMAP leaves an empty operator's meaning open.
        Query::And(queries) if queries.is_empty() => Ok(json!({})),
        Query::And(queries) => all(queries, "AND"),
        Query::Or(queries) => all(queries, "OR"),
        Query::Not(inner) => all(std::slice::from_ref(inner), "NOT"),
        Query::Term(term) => term_of(term, places, today),
    }
}

fn term_of(term: &Term, places: &Places, today: NaiveDate) -> Result<Value, Unsayable> {
    Ok(match term {
        Term::From(who) => json!({ "from": who }),
        Term::To(who) => json!({ "to": who }),
        Term::Subject(words) => json!({ "subject": words }),
        Term::Words(words) => json!({ "text": words }),
        Term::Since(day) => json!({ "after": midnight(*day) }),
        Term::Before(day) => json!({ "before": midnight(*day) }),
        Term::NewerThan(days) => {
            json!({ "after": midnight(today - chrono::Days::new(u64::from(*days))) })
        }
        Term::HasAttachment => json!({ "hasAttachment": true }),
        Term::Unread => json!({ "notKeyword": SEEN }),
        Term::Flagged => json!({ "hasKeyword": FLAGGED }),
        Term::Larger(bytes) => json!({ "minSize": bytes.saturating_add(1) }),
        Term::In(set) => match set {
            MailSet::Role(role) => json!({ "inMailbox": places.roles.get(role).ok_or(Unsayable)? }),
            MailSet::Mailbox(id) => json!({ "inMailbox": id }),
            MailSet::Keyword(keyword) => json!({ "hasKeyword": keyword }),
            MailSet::Unseen => json!({ "notKeyword": SEEN }),
            MailSet::Category(_) => return Err(Unsayable),
        },
        Term::MailboxNamed(name) => {
            let wanted = name.to_lowercase();
            let id = places
                .names
                .iter()
                .find(|(_, path)| path.to_lowercase() == wanted)
                .map(|(id, _)| id)
                .ok_or(Unsayable)?;
            json!({ "inMailbox": id })
        }
    })
}

impl<J: JmapApi> JmapMail<J> {
    pub(super) async fn find(&self, query: &SearchQuery, limit: usize) -> Result<Vec<RemoteRef>, BackendError> {
        self.synced().await?;
        let filter = match query {
            // The account reads no syntax of its own; text that comes
            // unparsed is words to look for.
            SearchQuery::Native(text) => json!({ "text": text }),
            SearchQuery::Tree(tree) => {
                let known = self.known();
                let places = Places {
                    roles: &known.roles,
                    names: &known.names,
                };
                filter_of(tree, &places, Local::now().date_naive()).map_err(|_| BackendError::Unsupported)?
            }
        };
        let limit = limit.clamp(1, 1000) as u64;
        let found = self.api().query_emails(filter, 0, limit).await.map_err(backend)?;
        let threads = self.api().email_threads(&found.ids).await.map_err(backend)?;
        Ok(found
            .ids
            .iter()
            .filter_map(|id| {
                let email = threads.list.iter().find(|e| &e.id == id)?;
                Some(RemoteRef {
                    id: id.clone(),
                    thread_id: email.thread_id.clone(),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> (BTreeMap<Role, String>, HashMap<String, String>) {
        (
            BTreeMap::from([(Role::Inbox, "mb-in".to_string())]),
            HashMap::from([("mb-w".to_string(), "Work/Bugs".to_string())]),
        )
    }

    #[test]
    fn a_tree_becomes_jmap_operators_and_conditions() {
        let (roles, names) = places();
        let places = Places { roles: &roles, names: &names };
        let query = Query::And(vec![
            Query::term(Term::From("ann".into())),
            Query::Or(vec![Query::term(Term::Unread), Query::term(Term::Flagged)]),
            Query::Not(Box::new(Query::is_in(MailSet::Role(Role::Inbox)))),
            Query::term(Term::MailboxNamed("work/bugs".into())),
            Query::term(Term::Larger(1000)),
        ]);
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        assert_eq!(
            filter_of(&query, &places, today).unwrap(),
            json!({ "operator": "AND", "conditions": [
                { "from": "ann" },
                { "operator": "OR", "conditions": [{ "notKeyword": "$seen" }, { "hasKeyword": "$flagged" }] },
                { "operator": "NOT", "conditions": [{ "inMailbox": "mb-in" }] },
                { "inMailbox": "mb-w" },
                { "minSize": 1001 }
            ]})
        );
    }

    #[test]
    fn a_term_jmap_cannot_say_refuses_the_search() {
        let (roles, names) = places();
        let places = Places { roles: &roles, names: &names };
        let today = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        for term in [
            Term::In(MailSet::Category("CATEGORY_SOCIAL".into())),
            Term::In(MailSet::Role(Role::Junk)),
            Term::MailboxNamed("Nowhere".into()),
        ] {
            assert_eq!(filter_of(&Query::term(term), &places, today), Err(Unsayable));
        }
        assert_eq!(filter_of(&Query::And(Vec::new()), &places, today), Ok(json!({})));
    }

    #[test]
    fn a_day_starts_at_local_midnight() {
        let (roles, names) = places();
        let places = Places { roles: &roles, names: &names };
        let day = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let since = filter_of(&Query::term(Term::Since(day)), &places, day).unwrap();
        let at = chrono::DateTime::parse_from_rfc3339(since["after"].as_str().unwrap()).unwrap();
        assert_eq!(at.with_timezone(&Local).date_naive(), day);
    }
}
