//! Warnings for a wait that goes on too long, to find what holds up an
//! account after a suspend. A diagnostic: each wait it watches says so in
//! the log every half minute until it ends, and once more when it ends.

use std::fmt::Display;
use std::time::Duration;

/// How long a wait may take before the log hears of it, and how often it
/// hears again.
const EVERY: Duration = Duration::from_secs(30);

/// Runs `work`, and warns while it takes longer than [`EVERY`]. `what`
/// names the wait; `holder` names who holds what it waits for, read
/// each time it warns. Needs a tokio runtime.
pub async fn watched<F, D>(what: &str, holder: impl Fn() -> D, work: F) -> F::Output
where
    F: Future,
    D: Display,
{
    tokio::pin!(work);
    let started = tokio::time::Instant::now();
    loop {
        match tokio::time::timeout(EVERY, &mut work).await {
            Ok(done) => {
                let waited = started.elapsed();
                if waited >= EVERY {
                    tracing::warn!(waited_secs = waited.as_secs(), "{what}: done at last");
                }
                return done;
            }
            Err(_) => tracing::warn!(
                waited_secs = started.elapsed().as_secs(),
                holder = %holder(),
                "{what}: still waiting"
            ),
        }
    }
}

/// [`watched`] with no holder to name.
pub async fn timed<F: Future>(what: &str, work: F) -> F::Output {
    watched(what, || "", work).await
}
