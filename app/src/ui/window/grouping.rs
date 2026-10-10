//! Mail groups from the window: the Group menu, Choose Group, and rows
//! dropped on a bundle in the list.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use mailrs_domain::AccountId;
use mailrs_store::threads;

use super::press::{Press, Scope};
use super::reach::Reach;
use super::{MainWindow, Target};
use crate::ui::group_picker;

impl MainWindow {
    /// Hooks the list's groups to the Group menu and its bundles to drops,
    /// and adds the one group action that takes a name.
    pub(super) fn install_groups(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.list.connect_groups(move |recent| {
            if let Some(win) = weak.upgrade() {
                win.conversation.set_recent_groups(&recent);
            }
        });
        let weak = Rc::downgrade(self);
        self.list.connect_group_drop(move |group| {
            weak.upgrade().is_some_and(|win| win.drop_on_group(group))
        });
        let join = gio::SimpleAction::new("join-group", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        join.connect_activate(move |_, parameter| {
            if let (Some(win), Some(group)) =
                (weak.upgrade(), parameter.and_then(|p| p.get::<String>()))
                && win.runs_here("join-group")
            {
                win.group(Some(group));
            }
        });
        self.actions.add_action(&join);
    }

    /// Puts what the conversation reaches in the group `name`, or with
    /// `None` takes it out of every group.
    pub(super) fn group(self: &Rc<Self>, name: Option<String>) {
        let view = Rc::clone(&self.conversation);
        self.press(&view, Press::Group(name));
    }

    /// Asks which group to put what the conversation reaches in, offering
    /// every group the accounts know.
    pub(super) fn choose_group(self: &Rc<Self>) {
        let accounts: Vec<AccountId> = self.accounts().iter().map(|a| a.id).collect();
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            let known = this
                .core
                .read(move |c| threads::group_names(c, &accounts))
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "could not read the mail groups");
                    Vec::new()
                });
            if let Some(name) = group_picker::pick(&this.window, known).await {
                this.group(Some(name));
            }
        });
    }

    /// Puts the rows dragged onto a bundle in its group. True when the
    /// drop was taken.
    fn drop_on_group(self: &Rc<Self>, group: String) -> bool {
        let rows = self.list.dragged();
        if rows.is_empty() {
            return false;
        }
        let targets: Vec<Target> = rows.iter().map(Target::from_row).collect();
        let open = self.conversation.read(|o| o.among(&targets)) == Some(true);
        let reach = Reach {
            targets,
            marks: Default::default(),
            muted: false,
            mailbox: self.shown(),
        };
        let view = Rc::clone(&self.conversation);
        self.press_on(&view, reach, Scope::Carried { open }, Press::Group(Some(group)))
    }
}
