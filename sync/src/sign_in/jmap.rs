//! Signing in to a JMAP account with an API token, as Fastmail gives one
//! to apps it has not approved for OAuth. Reading the session checks the
//! token and says which address it belongs to, and the token has to reach
//! both mail and sending.

use std::sync::Arc;

use mailrs_domain::translate::{fill, gettext};
use mailrs_domain::{Account, AccountState, EpochMillis, Provider};
use mailrs_jmap::session::{MAIL, SUBMISSION};
use mailrs_jmap::{Jmap, JmapError};
use mailrs_store::{Db, StoreError, accounts};

use super::taken;
use crate::passwords::{PasswordError, PasswordStore};

/// A JMAP account a person is signing in to: who serves it, where its
/// session is, and the token. It has no `Debug`, so the token cannot
/// reach a log line.
pub struct NewJmap {
    pub provider_name: String,
    pub session_url: String,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum JmapSignInError {
    /// The server did not take the token.
    #[error("{}", fill(&gettext("{provider} did not accept the token. Check that you copied all of it."), &[("provider", .0)]))]
    Refused(String),
    /// The token reaches the account, but not its mail or not sending.
    #[error("{}", fill(&gettext("This token cannot read and send mail. Make a new one in {provider} with access to email and to sending email."), &[("provider", .0)]))]
    Scope(String),
    #[error("{}", fill(&gettext("Could not reach {provider}: {reason}"), &[("provider", .provider), ("reason", .reason)]))]
    Unreachable { provider: String, reason: String },
    /// The address belongs to an account another provider serves.
    #[error("{}", taken(.address, .provider))]
    Taken { address: String, provider: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Password(#[from] PasswordError),
}

/// The address `new`'s token signs in as, read from the session, once the
/// token proves to reach mail and sending.
pub async fn check_jmap(new: &NewJmap) -> Result<String, JmapSignInError> {
    let provider = new.provider_name.clone();
    let unreachable = |err: JmapError| JmapSignInError::Unreachable {
        provider: provider.clone(),
        reason: err.to_string(),
    };
    let client = Jmap::new(&new.session_url, &new.token).map_err(unreachable)?;
    let session = match client.session().await {
        Ok(session) => session,
        Err(JmapError::Unauthorized) => return Err(JmapSignInError::Refused(provider)),
        Err(err) => return Err(unreachable(err)),
    };
    if session.account_for(MAIL).is_err() || session.account_for(SUBMISSION).is_err() {
        return Err(JmapSignInError::Scope(provider));
    }
    let address = session.username.trim().to_lowercase();
    match address.contains('@') {
        true => Ok(address),
        false => Err(unreachable(JmapError::Decode("the session names no address".into()))),
    }
}

/// Keeps a JMAP account whose token worked: adds it, or finds the one
/// already here for `address`, with its session URL and its token. An
/// account signing in again keeps its mail and stops needing a sign-in.
/// As with the other providers, a new row goes again when the keyring
/// refuses the token, so nothing is left that could never start.
pub async fn jmap_signed_in<P: PasswordStore + 'static>(
    db: &Db,
    tokens: Arc<P>,
    new: NewJmap,
    address: &str,
    now: EpochMillis,
) -> Result<Account, JmapSignInError> {
    let NewJmap {
        provider_name,
        session_url,
        token,
    } = new;
    let email = address.to_string();
    let before = db.read(move |c| accounts::account_by_email(c, &email)).await?;
    if let Some(held) = &before {
        if held.provider != Provider::Jmap {
            return Err(JmapSignInError::Taken {
                address: address.to_string(),
                provider: held.provider_name().to_string(),
            });
        }
        save(Arc::clone(&tokens), held.id, token.clone()).await?;
    }
    let email = address.to_string();
    let again = before.as_ref().map(|held| held.state);
    let added = db
        .write(move |c| {
            let Some(id) = accounts::insert_jmap_account(c, &email, &provider_name, &session_url, now)? else {
                let held = accounts::account_by_email(c, &email)?;
                return Ok(Err(held.map(|a| a.provider_name().to_string())));
            };
            if again == Some(AccountState::NeedsReauth) {
                accounts::set_state(c, id, AccountState::Ok)?;
            }
            Ok(Ok(id))
        })
        .await?;
    let id = added.map_err(|provider| JmapSignInError::Taken {
        address: address.to_string(),
        provider: provider.unwrap_or_default(),
    })?;
    if before.is_none()
        && let Err(err) = save(Arc::clone(&tokens), id, token).await
    {
        db.write(move |c| accounts::delete_account(c, id)).await?;
        return Err(err);
    }
    let address = address.to_string();
    db.read(move |c| accounts::account_by_email(c, &address))
        .await?
        .ok_or(JmapSignInError::Store(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows)))
}

async fn save<P: PasswordStore + 'static>(tokens: Arc<P>, id: i64, token: String) -> Result<(), JmapSignInError> {
    tokio::task::spawn_blocking(move || tokens.save(id, &token))
        .await
        .map_err(|err| PasswordError::Keyring(err.to_string()))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::passwords::MemoryPasswords;

    async fn server(primary: serde_json::Value, status: u16) -> (MockServer, NewJmap) {
        let server = MockServer::start().await;
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/session"))
            .and(header("Authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "capabilities": {}, "accounts": {}, "primaryAccounts": primary,
                "username": "Dana@Fastmail.com",
                "apiUrl": format!("{base}/api/"), "downloadUrl": format!("{base}/d"),
                "uploadUrl": format!("{base}/u"), "state": "s"
            })))
            .mount(&server)
            .await;
        let new = NewJmap {
            provider_name: "Fastmail".into(),
            session_url: format!("{base}/session"),
            token: "tok".into(),
        };
        (server, new)
    }

    #[tokio::test]
    async fn a_token_that_reaches_mail_and_sending_names_its_address() {
        let (_server, new) = server(json!({ MAIL: "u1", SUBMISSION: "u1" }), 200).await;
        assert_eq!(check_jmap(&new).await.unwrap(), "dana@fastmail.com");
    }

    #[tokio::test]
    async fn a_token_without_sending_or_a_refused_one_says_why() {
        let (_server, new) = server(json!({ MAIL: "u1" }), 200).await;
        assert!(matches!(check_jmap(&new).await, Err(JmapSignInError::Scope(_))));
        let (_server, new) = server(json!({}), 401).await;
        assert!(matches!(check_jmap(&new).await, Err(JmapSignInError::Refused(p)) if p == "Fastmail"));
    }

    fn new() -> NewJmap {
        NewJmap {
            provider_name: "Fastmail".into(),
            session_url: mailrs_jmap::FASTMAIL_SESSION.into(),
            token: "tok".into(),
        }
    }

    #[tokio::test]
    async fn signing_in_keeps_the_account_and_its_token() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let tokens = Arc::new(MemoryPasswords::default());
        let account = jmap_signed_in(&db, Arc::clone(&tokens), new(), "dana@fastmail.com", 0).await.unwrap();
        assert_eq!(account.provider, Provider::Jmap);
        assert_eq!(tokens.load(account.id).unwrap().as_deref(), Some("tok"));

        let id = account.id;
        db.write(move |c| accounts::set_state(c, id, AccountState::NeedsReauth)).await.unwrap();
        let again = NewJmap { token: "tok2".into(), ..new() };
        let account = jmap_signed_in(&db, Arc::clone(&tokens), again, "dana@fastmail.com", 1).await.unwrap();
        assert_eq!(account.id, id);
        assert_eq!(account.state, AccountState::Ok);
        assert_eq!(tokens.load(id).unwrap().as_deref(), Some("tok2"));
    }

    #[tokio::test]
    async fn an_address_an_imap_account_holds_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        db.write(|c| accounts::insert_imap_account(c, "dana@fastmail.com", "Fastmail", 0)).await.unwrap();
        let tokens = Arc::new(MemoryPasswords::default());
        let refused = jmap_signed_in(&db, Arc::clone(&tokens), new(), "dana@fastmail.com", 0).await;
        assert!(matches!(refused, Err(JmapSignInError::Taken { .. })));
    }
}
