//! The Set Topic dialog: one field that takes a topic's name, with a list
//! under it of the topics the store knows. Typing narrows the list to the
//! topics that match, and the button at the field's end shows them all,
//! as an editable combo box does. A new name starts a topic.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};
use mailrs_domain::topic;
use mailrs_domain::translate::gettext;

/// The most topics the list shows at once; past it, typing narrows it.
const SHOWN: usize = 8;

/// The known topics that match `typed`, ignoring case: those whose name
/// starts with it first, then those that hold it elsewhere, each in the
/// order `known` gives. Every known topic while `typed` is empty, and
/// none once `typed` is a whole name, which needs no list.
pub fn matching<'a>(typed: &str, known: &'a [String]) -> Vec<&'a str> {
    let wanted = typed.trim().to_lowercase();
    if known.iter().any(|k| k.to_lowercase() == wanted) {
        return Vec::new();
    }
    let lower: Vec<String> = known.iter().map(|k| k.to_lowercase()).collect();
    let starts = known.iter().zip(&lower).filter(|(_, l)| l.starts_with(&wanted));
    let holds = known
        .iter()
        .zip(&lower)
        .filter(|(_, l)| !l.starts_with(&wanted) && l.contains(&wanted));
    starts.chain(holds).map(|(k, _)| k.as_str()).take(SHOWN).collect()
}

/// Asks which topic to put mail on, from `known` or a new name, and
/// answers with the name, or nothing when the person cancels.
pub async fn pick(parent: &impl IsA<gtk::Widget>, known: Vec<String>) -> Option<String> {
    let entry = gtk::Entry::builder()
        .placeholder_text(gettext("Topic name"))
        .activates_default(true)
        .max_length(topic::MAX_LEN as i32)
        .build();
    super::name(&entry, &gettext("Topic name"));
    if !known.is_empty() {
        entry.set_secondary_icon_name(Some("pan-down-symbolic"));
        entry.set_secondary_icon_tooltip_text(Some(&gettext("Known topics")));
        entry.set_secondary_icon_activatable(true);
        offer_known(&entry, Rc::new(known));
    }
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Set Topic"))
        .body(gettext(
            "Mail on one topic is shown together in the list. A new name starts a topic.",
        ))
        .extra_child(&entry)
        .build();
    dialog.add_responses(&[("cancel", &gettext("Cancel")), ("topic", &gettext("Set"))]);
    dialog.set_response_appearance("topic", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("topic"));
    dialog.set_close_response("cancel");
    // The button stays off until the field holds a name a topic can carry.
    dialog.set_response_enabled("topic", false);
    let answer = dialog.clone();
    entry.connect_changed(move |entry| {
        answer.set_response_enabled("topic", topic::clean(&entry.text()).is_some());
    });
    let focus = entry.clone();
    dialog.connect_map(move |_| {
        focus.grab_focus();
    });
    if dialog.choose_future(Some(parent)).await != "topic" {
        return None;
    }
    topic::clean(&entry.text())
}

/// The list under `entry`: the topics that match what is typed, shown as
/// the person types and by the button at the field's end, which shows
/// them all. Down goes into the list, Enter or a click takes a topic, and
/// Escape closes the list while the dialog stays.
fn offer_known(entry: &gtk::Entry, known: Rc<Vec<String>>) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["navigation-sidebar"])
        .build();
    super::name(&list, &gettext("Known topics"));
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(280)
        .min_content_width(260)
        .child(&list)
        .build();
    let popover = gtk::Popover::builder()
        .autohide(false)
        .has_arrow(false)
        .position(gtk::PositionType::Bottom)
        .halign(gtk::Align::Start)
        .child(&scroller)
        .build();
    popover.set_parent(entry);
    let unparent = popover.clone();
    entry.connect_destroy(move |_| unparent.unparent());

    // Set while a pick fills the field, so the change it makes does not
    // open the list again.
    let filling = Rc::new(Cell::new(false));
    let show = {
        let (list, popover, entry) = (list.clone(), popover.clone(), entry.clone());
        move |names: Vec<&str>| {
            // As wide as the field, as a combo box's list is.
            popover.set_size_request(entry.width(), -1);
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }
            for name in &names {
                let label = gtk::Label::builder()
                    .label(*name)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                let row = gtk::ListBoxRow::builder().child(&label).build();
                row.set_widget_name(name);
                list.append(&row);
            }
            match names.is_empty() {
                true => popover.popdown(),
                false => popover.popup(),
            }
        }
    };

    {
        let (known, filling, show) = (Rc::clone(&known), Rc::clone(&filling), show.clone());
        entry.connect_changed(move |entry| {
            if filling.get() {
                return;
            }
            let text = entry.text();
            match text.trim().is_empty() {
                true => show(Vec::new()),
                false => show(matching(&text, &known)),
            }
        });
    }
    {
        let (known, show) = (Rc::clone(&known), show.clone());
        entry.connect_icon_press(move |_, icon| {
            if icon == gtk::EntryIconPosition::Secondary {
                show(matching("", &known));
            }
        });
    }
    {
        let (entry, popover, filling) = (entry.clone(), popover.clone(), Rc::clone(&filling));
        list.connect_row_activated(move |_, row| {
            filling.set(true);
            entry.set_text(row.widget_name().as_str());
            filling.set(false);
            popover.popdown();
            entry.grab_focus_without_selecting();
            entry.set_position(-1);
        });
    }
    // Down from the field goes into the list; Escape closes the list and
    // keeps the dialog, from the field or from the list.
    let keys = gtk::EventControllerKey::new();
    {
        let (list, popover) = (list.clone(), popover.clone());
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Down if popover.is_visible() => {
                if let Some(first) = list.row_at_index(0) {
                    first.grab_focus();
                }
                glib::Propagation::Stop
            }
            gdk::Key::Escape if popover.is_visible() => {
                popover.popdown();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
    }
    entry.add_controller(keys);
    let list_keys = gtk::EventControllerKey::new();
    {
        let (entry, popover) = (entry.clone(), popover.clone());
        list_keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Escape => {
                popover.popdown();
                entry.grab_focus_without_selecting();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
    }
    list.add_controller(list_keys);
}

#[cfg(test)]
mod tests {
    use super::matching;

    fn known() -> Vec<String> {
        ["Q3 Launch", "Invoices", "Café trip", "Launch party"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn topics_that_start_with_the_text_come_before_those_that_hold_it() {
        assert_eq!(matching("launch", &known()), ["Launch party", "Q3 Launch"]);
        assert_eq!(matching("CAF", &known()), ["Café trip"]);
        assert!(matching("Taxes", &known()).is_empty());
    }

    #[test]
    fn an_empty_field_lists_every_topic_and_a_whole_name_none() {
        assert_eq!(matching("", &known()).len(), 4);
        assert!(matching("invoices", &known()).is_empty());
    }
}
