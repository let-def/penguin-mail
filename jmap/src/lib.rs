//! JMAP (RFC 8620), JMAP Mail (RFC 8621) and JMAP for Contacts (RFC 9610)
//! as Penguin Mail uses them: the session, batched method calls, blobs,
//! and the calls the JMAP adapter in `mailrs-sync` makes. Fastmail is the
//! server it is written against; any server that follows the RFCs should
//! do.

mod client;
pub mod contacts;
mod error;
pub mod mail;
pub mod session;

pub use client::{Invocation, Jmap, Uploaded};
pub use contacts::{AddressBook, CARD_PROPERTIES, CONTACTS};
pub use error::JmapError;
pub use mail::{
    Changed, EMAIL_PROPERTIES, Email, EmailAddress, Got, Identity, Mailbox, Queried, SetAnswer,
    SetError, Submitted, Thread,
};
pub use session::{FASTMAIL_SESSION, Limits, Session};
