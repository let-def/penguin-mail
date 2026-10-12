//! Topics from the window: the Topic menu, Choose Topic, and rows
//! dropped on a bundle in the list.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use mailrs_domain::AccountId;
use mailrs_store::threads;

use super::press::{Press, Scope};
use super::reach::Reach;
use super::{MainWindow, Target};
use crate::ui::topic_picker;

impl MainWindow {
    /// Hooks the list's topics to the Topic menu and its bundles to drops,
    /// and adds the one topic action that takes a name.
    pub(super) fn install_topics(self: &Rc<Self>) {
        self.refresh_recent_topics();
        let weak = Rc::downgrade(self);
        self.list.connect_topic_drop(move |topic| {
            weak.upgrade().is_some_and(|win| win.drop_on_topic(topic))
        });
        let join = gio::SimpleAction::new("join-topic", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        join.connect_activate(move |_, parameter| {
            if let (Some(win), Some(topic)) =
                (weak.upgrade(), parameter.and_then(|p| p.get::<String>()))
                && win.runs_here("join-topic")
            {
                win.topic(Some(topic));
            }
        });
        self.actions.add_action(&join);
    }

    /// Puts what the conversation reaches in the topic `name`, or with
    /// `None` takes it out of every topic.
    pub(super) fn topic(self: &Rc<Self>, name: Option<String>) {
        match &name {
            Some(name) => self.used_topic(name),
            None => self.list.leaving_topic(&self.list.selected_rows()),
        }
        let view = Rc::clone(&self.conversation);
        self.press(&view, Press::Topic(name));
    }

    /// Asks which topic to put what the conversation reaches in, offering
    /// every topic the accounts know.
    pub(super) fn choose_topic(self: &Rc<Self>) {
        let accounts: Vec<AccountId> = self.accounts().iter().map(|a| a.id).collect();
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let known = this
                .core
                .read(move |c| threads::topic_names(c, &accounts))
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "could not read the topics");
                    Vec::new()
                });
            if let Some(name) = topic_picker::pick(&this.window, known).await {
                this.topic(Some(name));
            }
        });
    }

    /// Puts the dragged rows on `topic`, from a drop on its bundle, or
    /// with `None` takes them off their topics, from a drop outside every
    /// bundle. True when the drop was taken.
    fn drop_on_topic(self: &Rc<Self>, topic: Option<String>) -> bool {
        let rows = self.list.dragged();
        if rows.is_empty() {
            return false;
        }
        match &topic {
            Some(topic) => self.used_topic(topic),
            None => self.list.leaving_topic(&rows),
        }
        let targets: Vec<Target> = rows.iter().map(|row| Target::from_row(row)).collect();
        let open = self.conversation.read(|o| o.among(&targets)) == Some(true);
        let reach = Reach {
            targets,
            marks: Default::default(),
            muted: false,
            mailbox: self.shown(),
        };
        let view = Rc::clone(&self.conversation);
        self.press_on(&view, reach, Scope::Carried { open }, Press::Topic(topic))
    }

    /// Puts `topic` first among the topics used in this run, and offers it
    /// at once.
    fn used_topic(self: &Rc<Self>, topic: &str) {
        {
            let mut used = self.topics_used.borrow_mut();
            used.retain(|known| !mailrs_domain::topic::same(known, topic));
            used.insert(0, topic.to_string());
            used.truncate(RECENT_TOPICS);
        }
        self.refresh_recent_topics();
    }

    /// Fills Recent Topics again from the store: the topics used in this
    /// run first, then those on the newest mail of any account, on screen
    /// or not. Changes that arrive together read the store once.
    pub(super) fn refresh_recent_topics(self: &Rc<Self>) {
        if self.topics_asked.replace(true) {
            return;
        }
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            // A change from here on asks for a read of its own.
            this.topics_asked.set(false);
            let read = this.topics_read.get() + 1;
            this.topics_read.set(read);
            let accounts: Vec<AccountId> = this.accounts().iter().map(|a| a.id).collect();
            let stored = this
                .core
                .read(move |c| threads::recent_topics(c, &accounts, RECENT_TOPICS))
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "could not read the recent topics");
                    Vec::new()
                });
            if this.topics_read.get() != read {
                return;
            }
            let recent = recent_topics(&this.topics_used.borrow(), stored, RECENT_TOPICS);
            this.conversation.set_recent_topics(&recent);
        });
    }
}

/// How many topics Recent Topics offers by name.
const RECENT_TOPICS: usize = 5;

/// The topics Recent Topics offers: those `used` in this run first, then
/// the `stored` ones newest first, each once whatever its case, at most
/// `most`.
fn recent_topics(used: &[String], stored: Vec<String>, most: usize) -> Vec<String> {
    let mut recent: Vec<String> = used.to_vec();
    for topic in stored {
        if !recent.iter().any(|known| mailrs_domain::topic::same(known, &topic)) {
            recent.push(topic);
        }
    }
    recent.truncate(most);
    recent
}

#[cfg(test)]
mod tests {
    use super::recent_topics;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn topics_used_in_this_run_come_before_the_stores_each_once() {
        let used = names(&["Taxes", "q3 launch"]);
        let stored = names(&["Q3 Launch", "Invoices", "Taxes", "Trip"]);
        assert_eq!(
            recent_topics(&used, stored.clone(), 5),
            ["Taxes", "q3 launch", "Invoices", "Trip"]
        );
        assert_eq!(recent_topics(&used, stored.clone(), 3), ["Taxes", "q3 launch", "Invoices"]);
        assert_eq!(recent_topics(&[], stored, 2), ["Q3 Launch", "Invoices"]);
    }
}
