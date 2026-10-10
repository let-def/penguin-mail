//! The lines a bundle adds to the thread list: its header, with the
//! topic's name, how many rows it holds and the arrows to the topic's
//! next and previous places; the trailer that opens or closes it; and the
//! summary that stands for a closed bundle whose rows are all read. One
//! widget draws all three, since the list reuses its widgets for whatever
//! entry scrolls into view.

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::*;
use mailrs_domain::ThreadSummary;
use mailrs_domain::translate::{fill, fill_plural, gettext};

use super::Row;
use crate::format::relative_date;
use crate::ui::thread_row::set_avatar_hue;

/// How many senders and subjects a summary shows.
const SUMMARY_ROWS: usize = 3;

/// What the line draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Header,
    Trailer,
    /// The rows a closed bundle hides, newest first.
    Summary(Vec<Row>),
}

/// The bundle a line belongs to, as the line shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub topic: String,
    /// Which of the topic colours, `account-N` in the stylesheet.
    pub color: usize,
    /// How many rows the bundle holds, and how many a closed one hides.
    pub count: usize,
    pub hidden: usize,
    pub open: bool,
    /// Whether the topic shows above or below the bundle, or may show
    /// further down once more rows load.
    pub earlier: bool,
    pub later: bool,
    /// The rows stand for conversations rather than single messages.
    pub threaded: bool,
}

/// What a line asks of the list, with the line's position in it.
#[derive(Clone)]
pub struct Handlers {
    pub toggle: Rc<dyn Fn(u32)>,
    /// Goes to the topic's place above (`-1`) or below (`1`).
    pub jump: Rc<dyn Fn(u32, i32)>,
    /// Mail dropped on the line, to put in its topic; true when taken.
    pub dropped: Rc<dyn Fn(u32) -> bool>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BundleLine {
        pub item: glib::WeakRef<gtk::ListItem>,
        pub header: OnceCell<gtk::Box>,
        pub dot: OnceCell<gtk::Box>,
        pub title: OnceCell<gtk::Label>,
        pub count: OnceCell<gtk::Label>,
        pub up: OnceCell<gtk::Button>,
        pub down: OnceCell<gtk::Button>,
        pub summary: OnceCell<gtk::Box>,
        pub faces: OnceCell<gtk::Box>,
        pub names: OnceCell<gtk::Label>,
        pub date: OnceCell<gtk::Label>,
        pub subjects: OnceCell<gtk::Box>,
        pub trailer: OnceCell<gtk::Box>,
        pub more: OnceCell<gtk::Label>,
        pub chevron: OnceCell<gtk::Image>,
        pub kind: RefCell<Option<Kind>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BundleLine {
        const NAME: &'static str = "MailrsBundleLine";
        type Type = super::BundleLine;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for BundleLine {}
    impl WidgetImpl for BundleLine {}
    impl BoxImpl for BundleLine {}
}

glib::wrapper! {
    pub struct BundleLine(ObjectSubclass<imp::BundleLine>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl BundleLine {
    pub fn new(item: &gtk::ListItem, handlers: Handlers) -> BundleLine {
        let line: BundleLine = glib::Object::builder()
            .property("orientation", gtk::Orientation::Vertical)
            .build();
        line.add_css_class("bundle-line");
        let imp = line.imp();
        imp.item.set(Some(item));

        let header = gtk::Box::builder()
            .spacing(8)
            .css_classes(["bundle-header"])
            .build();
        let dot = gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes(["account-dot", "bundle-dot"])
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["bundle-title"])
            .build();
        let count = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .css_classes(["count"])
            .build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        let arrow = |icon: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular", "bundle-jump"])
                .build()
        };
        let (up, down) = (arrow("go-up-symbolic"), arrow("go-down-symbolic"));
        for part in [
            dot.upcast_ref::<gtk::Widget>(),
            title.upcast_ref(),
            count.upcast_ref(),
            spacer.upcast_ref(),
            up.upcast_ref(),
            down.upcast_ref(),
        ] {
            header.append(part);
        }

        let summary = gtk::Box::builder()
            .spacing(12)
            .css_classes(["bundle-summary"])
            .build();
        let faces = gtk::Box::builder()
            .valign(gtk::Align::Start)
            .css_classes(["bundle-faces"])
            .build();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let names = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["from"])
            .build();
        let date = gtk::Label::builder().css_classes(["date"]).build();
        top.append(&names);
        top.append(&date);
        let subjects = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["bundle-subjects"])
            .build();
        text.append(&top);
        text.append(&subjects);
        summary.append(&faces);
        summary.append(&text);

        let trailer = gtk::Box::builder()
            .spacing(6)
            .css_classes(["bundle-trailer"])
            .build();
        let more = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let chevron = gtk::Image::from_icon_name("pan-down-symbolic");
        trailer.append(&more);
        trailer.append(&chevron);

        line.append(&header);
        line.append(&summary);
        line.append(&trailer);

        let position = {
            let item = item.downgrade();
            move || {
                item.upgrade()
                    .map(|i| i.position())
                    .filter(|p| *p != gtk::INVALID_LIST_POSITION)
            }
        };
        // A click anywhere but on an arrow opens or closes the bundle.
        let click = gtk::GestureClick::builder()
            .button(gtk::gdk::BUTTON_PRIMARY)
            .build();
        let (toggle, at) = (Rc::clone(&handlers.toggle), position.clone());
        click.connect_released(move |gesture, _, x, y| {
            let on_arrow = gesture
                .widget()
                .and_then(|w| w.pick(x, y, gtk::PickFlags::DEFAULT))
                .is_some_and(|picked| picked.ancestor(gtk::Button::static_type()).is_some());
            if let (false, Some(at)) = (on_arrow, at()) {
                toggle(at);
            }
        });
        line.add_controller(click);
        for (button, way) in [(&up, -1), (&down, 1)] {
            let (jump, at) = (Rc::clone(&handlers.jump), position.clone());
            button.connect_clicked(move |_| {
                if let Some(at) = at() {
                    jump(at, way);
                }
            });
        }
        let target = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        let (dropped, at) = (Rc::clone(&handlers.dropped), position);
        target.connect_drop(move |_, value, _, _| {
            value.get::<String>().is_ok_and(|v| v == crate::ui::sidebar::DRAG_MAIL)
                && at().is_some_and(|at| dropped(at))
        });
        line.add_controller(target);

        imp.header.set(header).ok();
        imp.dot.set(dot).ok();
        imp.title.set(title).ok();
        imp.count.set(count).ok();
        imp.up.set(up).ok();
        imp.down.set(down).ok();
        imp.summary.set(summary).ok();
        imp.faces.set(faces).ok();
        imp.names.set(names).ok();
        imp.date.set(date).ok();
        imp.subjects.set(subjects).ok();
        imp.trailer.set(trailer).ok();
        imp.more.set(more).ok();
        imp.chevron.set(chevron).ok();
        line
    }

    /// The position in the list the line shows, or `None` while it shows
    /// nothing.
    pub fn position(&self) -> Option<u32> {
        let position = self.imp().item.upgrade()?.position();
        (position != gtk::INVALID_LIST_POSITION).then_some(position)
    }

    /// Draws `kind` for the bundle `shown`, and gives the list item the
    /// words a screen reader says for it.
    pub fn bind(&self, kind: &Kind, shown: &Shown) {
        let imp = self.imp();
        let get = |cell: &OnceCell<gtk::Box>| cell.get().expect("built in new").clone();
        let header = get(&imp.header);
        let summary = get(&imp.summary);
        let trailer = get(&imp.trailer);
        header.set_visible(*kind == Kind::Header);
        summary.set_visible(matches!(kind, Kind::Summary(_)));
        trailer.set_visible(*kind != Kind::Header);
        let dot = get(&imp.dot);
        let wanted = format!("account-{}", shown.color);
        for class in dot.css_classes() {
            if class.starts_with("account-") && class != "account-dot" {
                dot.remove_css_class(&class);
            }
        }
        dot.add_css_class(&wanted);
        let label = |cell: &OnceCell<gtk::Label>| cell.get().expect("built in new").clone();
        let topic = shown.topic.as_str();
        label(&imp.title).set_label(topic);
        label(&imp.count).set_label(&shown.count.to_string());
        let up = imp.up.get().expect("built in new");
        let down = imp.down.get().expect("built in new");
        up.set_sensitive(shown.earlier);
        down.set_sensitive(shown.later);
        let previous = fill(&gettext("Previous in {topic}"), &[("topic", topic)]);
        let next = fill(&gettext("Next in {topic}"), &[("topic", topic)]);
        up.set_tooltip_text(Some(&previous));
        down.set_tooltip_text(Some(&next));
        crate::ui::name(up, &previous);
        crate::ui::name(down, &next);

        let count = shown.count.to_string();
        let hidden = shown.hidden.to_string();
        let more = label(&imp.more);
        more.set_label(&match (kind, shown.open) {
            (Kind::Trailer, false) => fill_plural(
                "{count} more in {topic}",
                "{count} more in {topic}",
                shown.hidden,
                &[("count", &hidden), ("topic", topic)],
            ),
            _ => fill_plural(
                "{count} in {topic}",
                "{count} in {topic}",
                shown.count,
                &[("count", &count), ("topic", topic)],
            ),
        });
        imp.chevron.get().expect("built in new").set_icon_name(Some(match shown.open {
            true => "pan-up-symbolic",
            false => "pan-down-symbolic",
        }));
        if let Kind::Summary(rows) = kind {
            self.fill_summary(rows);
        }
        let words = spoken(kind, shown);
        self.update_property(&[gtk::accessible::Property::Label(&words)]);
        if let Some(item) = imp.item.upgrade() {
            item.set_accessible_label(&words);
        }
        *imp.kind.borrow_mut() = Some(kind.clone());
    }

    fn fill_summary(&self, rows: &[Row]) {
        let imp = self.imp();
        let faces = imp.faces.get().expect("built in new");
        while let Some(child) = faces.first_child() {
            faces.remove(&child);
        }
        for row in rows.iter().take(SUMMARY_ROWS) {
            let face = adw::Avatar::builder()
                .size(24)
                .show_initials(true)
                .text(sender(row))
                .css_classes(["bundle-face"])
                .build();
            set_avatar_hue(&face, &row.from, &row.from_email);
            faces.append(&face);
        }
        let mut names: Vec<String> = Vec::new();
        for row in rows {
            let name = sender(row);
            if !names.contains(&name) {
                names.push(name);
            }
        }
        imp.names.get().expect("built in new").set_label(&names.join(", "));
        let newest = rows.iter().map(|r| r.last_message_at).max().unwrap_or_default();
        imp.date
            .get()
            .expect("built in new")
            .set_label(&relative_date(newest, chrono::Local::now()));
        let subjects = imp.subjects.get().expect("built in new");
        while let Some(child) = subjects.first_child() {
            subjects.remove(&child);
        }
        for row in rows.iter().take(SUMMARY_ROWS) {
            let subject = gtk::Label::builder()
                .label(format!("• {}", subject(row)))
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["snippet"])
                .build();
            subjects.append(&subject);
        }
    }
}

fn sender(row: &ThreadSummary) -> String {
    match row.from.trim() {
        "" => row.from_email.clone(),
        name => name.to_string(),
    }
}

fn subject(row: &ThreadSummary) -> String {
    match row.subject.trim() {
        "" => gettext("(no subject)"),
        subject => subject.to_string(),
    }
}

/// What a screen reader says for a line.
fn spoken(kind: &Kind, shown: &Shown) -> String {
    let topic = shown.topic.as_str();
    let count = shown.count.to_string();
    let values = [("count", count.as_str()), ("topic", topic)];
    match (kind, shown.threaded) {
        (Kind::Header, true) => fill_plural(
            "Topic {topic}, {count} conversation",
            "Topic {topic}, {count} conversations",
            shown.count,
            &values,
        ),
        (Kind::Header, false) => fill_plural(
            "Topic {topic}, {count} message",
            "Topic {topic}, {count} messages",
            shown.count,
            &values,
        ),
        (Kind::Trailer, _) if shown.open => {
            fill(&gettext("Show only unread in {topic}"), &[("topic", topic)])
        }
        (Kind::Trailer, _) => {
            let hidden = shown.hidden.to_string();
            fill_plural(
                "Show {count} more in {topic}",
                "Show {count} more in {topic}",
                shown.hidden,
                &[("count", &hidden), ("topic", topic)],
            )
        }
        (Kind::Summary(_), true) => fill_plural(
            "{count} read conversation in {topic}, show it",
            "{count} read conversations in {topic}, show them",
            shown.count,
            &values,
        ),
        (Kind::Summary(_), false) => fill_plural(
            "{count} read message in {topic}, show it",
            "{count} read messages in {topic}, show them",
            shown.count,
            &values,
        ),
    }
}
