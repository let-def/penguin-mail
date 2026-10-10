//! The Choose Group dialog: a name typed in an entry that completes the
//! groups the store knows as the person types, or one picked from a list
//! of them. A new name starts a group.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use mailrs_domain::mail_group;
use mailrs_domain::translate::gettext;

/// The rest of the first known group whose name starts with `typed`,
/// ignoring case, for the entry to offer after what the person typed.
/// Nothing while `typed` is empty or already a whole name.
pub fn completion<'a>(typed: &str, known: &'a [String]) -> Option<&'a str> {
    if typed.is_empty() {
        return None;
    }
    let lower = typed.to_lowercase();
    if known.iter().any(|k| k.to_lowercase() == lower) {
        return None;
    }
    known.iter().find_map(|name| {
        let start = name.char_indices().nth(typed.chars().count()).map(|(i, _)| i)?;
        (name[..start].to_lowercase() == lower).then(|| &name[start..])
    })
}

/// Asks which group to put mail in, from `known` or a new name, and
/// answers with the name, or nothing when the person cancels.
pub async fn pick(parent: &impl IsA<gtk::Widget>, known: Vec<String>) -> Option<String> {
    let entry = gtk::Entry::builder()
        .placeholder_text(gettext("Group name"))
        .activates_default(true)
        .max_length(mail_group::MAX_LEN as i32)
        .build();
    super::name(&entry, &gettext("Group name"));
    complete_as_typed(&entry, Rc::new(known.clone()));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.append(&entry);
    if !known.is_empty() {
        let names: Vec<&str> = known.iter().map(String::as_str).collect();
        let list = gtk::DropDown::from_strings(&names);
        list.set_selected(gtk::INVALID_LIST_POSITION);
        list.set_enable_search(true);
        list.set_search_match_mode(gtk::StringFilterMatchMode::Substring);
        list.set_expression(Some(gtk::PropertyExpression::new(
            gtk::StringObject::static_type(),
            None::<&gtk::Expression>,
            "string",
        )));
        super::name(&list, &gettext("Known groups"));
        list.set_tooltip_text(Some(&gettext("Known groups")));
        let typed = entry.clone();
        list.connect_selected_item_notify(move |list| {
            if let Some(name) = list.selected_item().and_downcast::<gtk::StringObject>() {
                typed.set_text(&name.string());
                typed.set_position(-1);
            }
        });
        content.append(&list);
    }
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Choose Group"))
        .body(gettext(
            "Mail in one group is drawn together in the list. A new name starts a group.",
        ))
        .extra_child(&content)
        .build();
    dialog.add_responses(&[("cancel", &gettext("Cancel")), ("group", &gettext("Group"))]);
    dialog.set_response_appearance("group", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("group"));
    dialog.set_close_response("cancel");
    // The button stays off until the entry holds a name a group can carry.
    dialog.set_response_enabled("group", false);
    let answer = dialog.clone();
    entry.connect_changed(move |entry| {
        answer.set_response_enabled("group", mail_group::clean(&entry.text()).is_some());
    });
    let focus = entry.clone();
    dialog.connect_map(move |_| {
        focus.grab_focus();
    });
    if dialog.choose_future(Some(parent)).await != "group" {
        return None;
    }
    mail_group::clean(&entry.text())
}

/// Fills in the rest of a known group's name after the cursor as the
/// person types at the end of the entry, selected, so typing on replaces
/// it and Enter takes it.
fn complete_as_typed(entry: &gtk::Entry, known: Rc<Vec<String>>) {
    let filling = Rc::new(Cell::new(false));
    let typed_len = Rc::new(Cell::new(0usize));
    entry.connect_changed(move |entry| {
        if filling.get() {
            return;
        }
        let text = entry.text().to_string();
        let length = text.chars().count();
        // Only a keystroke that added to the end completes; a deletion
        // leaves the person's own text alone.
        let grew = length > typed_len.replace(length);
        if !grew || entry.position() != length as i32 {
            return;
        }
        let Some(rest) = completion(&text, &known) else {
            return;
        };
        let rest = rest.to_string();
        filling.set(true);
        let (entry, filling) = (entry.clone(), Rc::clone(&filling));
        // Changing the text from inside its own change signal confuses
        // the entry's cursor, so the fill waits for the change to finish.
        glib::idle_add_local_once(move || {
            if entry.text() == text {
                entry.set_text(&format!("{text}{rest}"));
                entry.select_region(length as i32, -1);
            }
            filling.set(false);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::completion;

    fn known() -> Vec<String> {
        ["Q3 Launch", "Invoices", "Café trip"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn typing_the_start_of_a_group_offers_the_rest() {
        assert_eq!(completion("q3", &known()), Some(" Launch"));
        assert_eq!(completion("Inv", &known()), Some("oices"));
        assert_eq!(completion("caf", &known()), Some("é trip"));
    }

    #[test]
    fn nothing_is_offered_for_no_match_an_empty_entry_or_a_whole_name() {
        assert_eq!(completion("Taxes", &known()), None);
        assert_eq!(completion("", &known()), None);
        assert_eq!(completion("invoices", &known()), None);
        assert_eq!(completion("Q3 Launch and more", &known()), None);
    }
}
