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
        let weak = Rc::downgrade(self);
        self.list.connect_topics(move |recent| {
            if let Some(win) = weak.upgrade() {
                win.conversation.set_recent_topics(&recent);
            }
        });
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
        if name.is_none() {
            self.list.leaving_topic(&self.list.selected_rows());
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
        if topic.is_none() {
            self.list.leaving_topic(&rows);
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
}
