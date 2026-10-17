//! The middle pane: threads of the current mailbox, or search results.
//! One page of rows loads at a time; scrolling near the end asks for more.
//! Every row is held once, behind an `Rc`, and shared with the list model.
//!
//! Rows of one topic that sit near each other are drawn together as
//! a bundle ([`bundles`]), so the model holds a bundle's header, trailer
//! or summary between the rows. A position in the model is therefore an
//! item, and only some items are rows: everything that maps a position to
//! a row goes through the layout.

mod bundle_line;
mod bundles;
mod entry;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene};
use mailrs_domain::{AccountId, ThreadSummary};

use super::list_feed::order;
use super::sidebar::DRAG_MAIL;
use super::thread_row::{Avatar, OpenMenu, ThreadRow};
use crate::diff::edits;
use crate::photos::ContactPhotos;
use crate::settings::TopicSpan;
use bundle_line::{BundleLine, Handlers, Kind};
use bundles::{Entry, Layout, Opened};
use mailrs_domain::translate::gettext;

/// The keys that open the menu of the row with the focus, in GTK's
/// accelerator syntax. The Keyboard Shortcuts dialog lists these.
pub const MENU_KEYS: [&str; 2] = ["Menu", "<Shift>F10"];

/// What is selected in the list.
/// The rows are the ones the list holds, shared rather than copied: a
/// select-all over a large mailbox hands out every row.
pub enum Picked {
    None,
    One(Row),
    Many(Vec<Row>),
}

type Key = (AccountId, String, Option<String>);

/// A row shared between the list model and the rows this pane keeps.
pub type Row = Rc<ThreadSummary>;

/// Puts the dragged rows on a topic, or with `None` takes them off theirs;
/// true when it took them.
type PutInTopic = dyn Fn(Option<String>) -> bool;

/// How many pages a jump to a topic's next place loads before it gives
/// up on finding one.
const JUMP_PAGES: u8 = 10;

/// One item of the list model: a row, with its place in a bundle when it
/// sits in one, or a line a bundle adds.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Row(Row, Option<Band>),
    Line(Kind, bundle_line::Shown, Band),
}

/// Where an item sits in its bundle, for the band of colour that marks a
/// bundle from its header to its last line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Band {
    /// Which of the topic colours, `topic-N` in the stylesheet.
    color: usize,
    /// The bundle's header, where the band starts.
    start: bool,
    /// The bundle's last line, where the band ends.
    end: bool,
}

/// The CSS classes a band puts on the row GTK wraps an item in.
const BAND_CLASSES: [&str; 3] = ["bundled", "bundle-start", "bundle-end"];

/// Marks the row GTK wraps `slot` in with `band` once the bind is over.
/// The row is GTK's own and still being put together while a bind runs:
/// so much as holding it then leaves GTK with widgets it no longer knows.
/// By the time the mark goes on, the row may show another item, which
/// then keeps the mark its own bind gives it.
fn band_later(item: &gtk::ListItem, slot: &gtk::Widget, object: &glib::BoxedAnyObject, band: Option<Band>) {
    let (item, slot, object) = (item.downgrade(), slot.downgrade(), object.downgrade());
    glib::idle_add_local_once(move || {
        let (Some(item), Some(slot), Some(object)) = (item.upgrade(), slot.upgrade(), object.upgrade()) else {
            return;
        };
        let current = item.item().and_downcast::<glib::BoxedAnyObject>();
        if current.as_ref() != Some(&object) {
            return;
        }
        if let Some(wrapper) = slot.parent() {
            mark_band(&wrapper, band);
        }
    });
}

/// Puts `band`'s classes on `wrapper`, the row GTK wraps an item in, and
/// takes off what an earlier item bound to it left there.
fn mark_band(wrapper: &gtk::Widget, band: Option<Band>) {
    for class in BAND_CLASSES {
        wrapper.remove_css_class(class);
    }
    for color in 0..bundles::COLORS {
        wrapper.remove_css_class(&format!("topic-{color}"));
    }
    let Some(band) = band else { return };
    wrapper.add_css_class("bundled");
    wrapper.add_css_class(&format!("topic-{}", band.color));
    if band.start {
        wrapper.add_css_class("bundle-start");
    }
    if band.end {
        wrapper.add_css_class("bundle-end");
    }
}

/// Where the list's focus goes once a change it is told of has landed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Landing {
    /// On the row with this key.
    Row(Key),
    /// On the header of the bundle of this topic that holds this row.
    Bundle(String, Key),
}

/// What the list shows: the rows in list order, laid out in bundles, and
/// the items the model holds for that layout.
#[derive(Default)]
struct Shown {
    rows: Vec<Row>,
    layout: Layout,
    items: Vec<Item>,
    opened: Opened,
    /// Rows a closed bundle keeps showing once read: those the reader
    /// selected in this mailbox, so a row does not vanish as it is read.
    kept: HashSet<Key>,
    /// How rows gather into bundles, if they do; a search lists hits.
    bundling: Option<TopicSpan>,
    /// The mailbox has rows past the last one loaded.
    more: bool,
    /// Where the focus goes after the next change, set before a change
    /// whose effect on the rows only the list can tell.
    landing: Option<Landing>,
    /// A jump down to a topic's next place waiting for more rows: the
    /// topic, the first row of the bundle it left, and the pages it may
    /// still load.
    jump: Option<(String, Key, u8)>,
}

impl Shown {
    fn row_at(&self, position: usize) -> Option<&Row> {
        self.layout.row_at(position).and_then(|r| self.rows.get(r))
    }

    /// The bundle whose header, trailer or summary is at `position`.
    fn bundle_at(&self, position: usize) -> Option<&bundles::Bundle> {
        match self.layout.entries.get(position)? {
            Entry::Header(b) | Entry::Trailer(b) | Entry::Summary(b) => self.layout.bundles.get(*b),
            Entry::Row { .. } => None,
        }
    }

    /// The index of the bundle the item at `position` belongs to: its
    /// header, a line or a row of it.
    fn bundle_index(&self, position: usize) -> Option<usize> {
        match self.layout.entries.get(position)? {
            Entry::Row { bundle, .. } => *bundle,
            Entry::Header(b) | Entry::Trailer(b) | Entry::Summary(b) => Some(*b),
        }
    }

    /// The bundle the item at `position` belongs to.
    fn bundle_of(&self, position: usize) -> Option<&bundles::Bundle> {
        self.bundle_index(position).and_then(|b| self.layout.bundles.get(b))
    }

    /// The bundle of `topic` whose rows include `row`.
    fn bundle_holding(&self, topic: &str, row: &Key) -> Option<&bundles::Bundle> {
        self.layout.bundles.iter().find(|b| {
            mailrs_domain::topic::same(&b.topic, topic)
                && b.members.iter().any(|m| key(&self.rows[*m]) == *row)
        })
    }
}

/// The arrow's choices of how bundles gather, as radio buttons on the
/// window's `topic-span` action. A menu model would draw the same radio
/// items, but GTK points their accessible label at an empty box, so a
/// screen reader would read three nameless items.
fn topic_spans() -> gtk::Popover {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    let popover = gtk::Popover::builder()
        .child(&content)
        .css_classes(["topic-spans"])
        .build();
    let mut first: Option<gtk::CheckButton> = None;
    for span in TopicSpan::ALL {
        let choice = gtk::CheckButton::builder()
            .label(span.label())
            .action_name("win.topic-span")
            .action_target(&span.id().to_variant())
            .build();
        // Grouped, so each draws as a radio button.
        choice.set_group(first.as_ref());
        first.get_or_insert_with(|| choice.clone());
        let pop = popover.clone();
        choice.connect_toggled(move |choice| {
            if choice.is_active() && pop.is_visible() {
                pop.popdown();
            }
        });
        content.append(&choice);
    }
    popover
}

/// How far bundles reach over `rows` for `span`: a week and a month go
/// by each row's newest message on this computer's calendar, with weeks
/// starting where the calendar's do.
fn reach(rows: &[Row], span: TopicSpan) -> bundles::Reach {
    let period = match span {
        TopicSpan::Nearby => return bundles::NEARBY,
        TopicSpan::Week => bundles::Period::Week(crate::locale_time::week_start_weekday()),
        TopicSpan::Month => bundles::Period::Month,
    };
    bundles::Reach::Periods(
        rows.iter()
            .map(|row| {
                let day = crate::format::local(row.last_message_at)
                    .map(|when| when.date_naive())
                    .unwrap_or_default();
                bundles::period(day, period)
            })
            .collect(),
    )
}

/// The items for `layout` over `rows`. `threaded` says whether the rows
/// stand for conversations, and `more` whether rows past the last may
/// hold a topic's next place.
fn items_of(rows: &[Row], layout: &Layout, threaded: bool, more: bool) -> Vec<Item> {
    let shown = |b: usize| {
        let bundle = &layout.bundles[b];
        bundle_line::Shown {
            topic: bundle.topic.clone(),
            color: bundles::color_of(&bundle.topic),
            count: bundle.members.len(),
            hidden: bundle.hidden,
            open: bundle.open,
            earlier: bundle.earlier.is_some(),
            later: bundle.later.is_some() || more,
            threaded,
        }
    };
    let bundle_of = |entry: &Entry| match entry {
        Entry::Row { bundle, .. } => *bundle,
        Entry::Header(b) | Entry::Trailer(b) | Entry::Summary(b) => Some(*b),
    };
    let band = |at: usize| {
        let bundle = bundle_of(&layout.entries[at])?;
        let next = layout.entries.get(at + 1).and_then(bundle_of);
        Some(Band {
            color: bundles::color_of(&layout.bundles[bundle].topic),
            start: matches!(layout.entries[at], Entry::Header(_)),
            end: next != Some(bundle),
        })
    };
    layout
        .entries
        .iter()
        .enumerate()
        .map(|(at, entry)| match entry {
            Entry::Row { row, .. } => Item::Row(Rc::clone(&rows[*row]), band(at)),
            // The header draws neither whether the bundle is open nor how
            // many rows it hides, so those stay out of its item: opening
            // or closing then leaves the header's item as it was, and GTK
            // keeps the view anchored to it instead of jumping.
            Entry::Header(b) => Item::Line(
                Kind::Header,
                bundle_line::Shown {
                    open: false,
                    hidden: 0,
                    ..shown(*b)
                },
                band(at).expect("a header sits in its bundle"),
            ),
            Entry::Trailer(b) => {
                Item::Line(Kind::Trailer, shown(*b), band(at).expect("a trailer sits in its bundle"))
            }
            Entry::Summary(b) => {
                let hidden = layout.bundles[*b]
                    .members
                    .iter()
                    .map(|m| Rc::clone(&rows[*m]))
                    .collect();
                Item::Line(Kind::Summary(hidden), shown(*b), band(at).expect("a summary sits in its bundle"))
            }
        })
        .collect()
}

fn key(row: &ThreadSummary) -> Key {
    (row.account_id, row.id.clone(), row.message_id.clone())
}

pub struct ThreadList {
    pub page: adw::NavigationPage,
    pub sidebar_button: gtk::ToggleButton,
    pub search_button: gtk::ToggleButton,
    /// Turns bundles on and off through the window's `group-topics`
    /// action, which also grays it out over search hits; its arrow picks
    /// how they gather through `topic-span`.
    group_button: adw::SplitButton,
    /// Where `CategoryBar::install_categories` puts the chips, right under
    /// the header and above the banners, so a sign-in or Grant Access
    /// banner never lands between the header and the chips.
    pub categories_slot: gtk::Box,
    pub banner: adw::Banner,
    /// The Grant Access bars, one for each account that still wants
    /// one; the window fills it. They sit on the mail list, since the
    /// calendar says what its accounts lack in its own sidebar.
    pub grant_bars: gtk::Box,
    pub search_entry: gtk::SearchEntry,
    search_bar: gtk::SearchBar,
    /// "All Inboxes", and under it the unread count and category.
    title: gtk::Label,
    subtitle: gtk::Label,
    /// The header and the bar at the page's foot, between which New moves
    /// as the window narrows ([`super::narrow_header`]).
    header: adw::HeaderBar,
    compose_button: gtk::Button,
    bottom_slot: adw::Bin,
    toolbar: adw::ToolbarView,
    narrow: Cell<bool>,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    store: gio::ListStore,
    selection: gtk::MultiSelection,
    view: gtk::ListView,
    scroller: gtk::ScrolledWindow,
    more: gtk::Button,
    shown: Rc<RefCell<Shown>>,
    /// Tells the focus handler that brings Tab to the open conversation's
    /// row that the list moved the focus itself, which stays where it is.
    steered: Rc<Cell<bool>>,
    /// Asks the window for the next page, as scrolling to the end does.
    on_more: RefCell<Option<Rc<dyn Fn()>>>,
    /// Puts the dragged rows on a bundle's topic, from a drop on any line
    /// of it, or takes them off theirs, from a drop outside every bundle.
    on_topic_drop: RefCell<Option<Box<PutInTopic>>>,
    /// The rows of the drag in progress.
    dragged: Rc<RefCell<Vec<Row>>>,
    show_accounts: Rc<Cell<bool>>,
    appearance: Rc<Cell<(crate::settings::PreviewLines, bool, bool, bool)>>,
    /// Lower-case VIP addresses; their rows get a star.
    vips: Rc<RefCell<HashSet<String>>>,
    /// Contact photos, read as rows ask for them. None, or none on disk
    /// while contacts are off, and rows show no face at all.
    photos: Rc<RefCell<Option<Rc<ContactPhotos>>>>,
    /// The row widgets the list made, which it recycles as it scrolls.
    made: Rc<RefCell<Vec<glib::WeakRef<ThreadRow>>>>,
    /// Draws one row for the item it holds.
    bind: Rc<dyn Fn(&gtk::ListItem)>,
    muted: Cell<bool>,
    /// The menu a right click on a row opens. It has no model until the
    /// window hands over the conversation's own through `set_row_menu`.
    row_popover: gtk::PopoverMenu,
}

impl ThreadList {
    pub fn new(
        on_select: impl Fn(Picked) + 'static,
        on_search: impl Fn(String) + 'static,
    ) -> Rc<ThreadList> {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::MultiSelection::new(Some(store.clone()));
        let show_accounts = Rc::new(Cell::new(true));
        let state = Rc::new(RefCell::new(Shown {
            bundling: Some(TopicSpan::Nearby),
            ..Shown::default()
        }));
        // The lines a bundle adds reach the list through this, which is
        // filled in once the list exists.
        let me: Rc<RefCell<std::rc::Weak<ThreadList>>> = Rc::default();
        let handlers = {
            let (toggle, jump) = (Rc::clone(&me), Rc::clone(&me));
            Handlers {
                toggle: Rc::new(move |at| {
                    if let Some(list) = toggle.borrow().upgrade() {
                        list.toggle_at(at as usize);
                    }
                }),
                jump: Rc::new(move |at, way| {
                    if let Some(list) = jump.borrow().upgrade() {
                        list.jump_from(at as usize, way);
                    }
                }),
            }
        };
        let row_popover = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        super::name_menu_items(&row_popover);
        row_popover.set_has_arrow(false);
        row_popover.set_halign(gtk::Align::Start);
        let dragged: Rc<RefCell<Vec<Row>>> = Rc::new(RefCell::new(Vec::new()));
        let made: Rc<RefCell<Vec<glib::WeakRef<ThreadRow>>>> = Rc::default();
        let factory = gtk::SignalListItemFactory::new();
        let making = Rc::clone(&made);
        let (all, drag_rows, picked) = (Rc::clone(&state), Rc::clone(&dragged), selection.clone());
        let menu = row_popover.clone();
        let me_for_drops = Rc::clone(&me);
        factory.connect_setup(move |_, item| {
            let me = &me_for_drops;
            let item = item
                .downcast_ref::<gtk::ListItem>()
                .expect("list items are ListItems");
            // A slot holds a row and a bundle's line, and shows the one
            // its item calls for.
            let slot = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .css_classes(["list-slot"])
                .build();
            let row = ThreadRow::default();
            row.set_item(item);
            making.borrow_mut().push(row.downgrade());
            context_menu(&row, item, &picked, &menu);
            // Dragging a selected row takes the whole selection along.
            let source = gtk::DragSource::new();
            source.set_actions(gdk::DragAction::MOVE);
            let counted = Rc::clone(&drag_rows);
            let (list_item, all, taken_rows, picked) = (
                item.clone(),
                Rc::clone(&all),
                Rc::clone(&drag_rows),
                picked.clone(),
            );
            source.connect_prepare(move |_, _, _| {
                let position = list_item.position();
                let shown = all.borrow();
                let taken: Vec<Row> = if picked.is_selected(position) {
                    (0..shown.items.len() as u32)
                        .filter(|p| picked.is_selected(*p))
                        .filter_map(|p| shown.row_at(p as usize).map(Rc::clone))
                        .collect()
                } else {
                    shown
                        .row_at(position as usize)
                        .map(Rc::clone)
                        .into_iter()
                        .collect()
                };
                if taken.is_empty() {
                    return None;
                }
                *taken_rows.borrow_mut() = taken;
                Some(gdk::ContentProvider::for_value(&DRAG_MAIL.to_value()))
            });
            source.connect_drag_begin(move |source, drag| {
                let count = counted.borrow().len();
                let label = gtk::Label::builder()
                    .label(count.to_string())
                    .css_classes(["drag-badge"])
                    .build();
                let icon = gtk::DragIcon::for_drag(drag);
                icon.set_child(Some(&label));
                source.set_icon(None::<&gdk::Paintable>, 0, 0);
            });
            row.add_controller(source);
            slot.append(&row);
            slot.append(&BundleLine::new(item, handlers.clone()));
            slot.add_controller(drop_target(item, me));
            item.set_child(Some(&slot));
        });
        let appearance = Rc::new(Cell::new((
            crate::settings::PreviewLines::Two,
            true,
            false,
            true,
        )));
        let row_appearance = Rc::clone(&appearance);
        let shown = Rc::clone(&show_accounts);
        let vips: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
        let starred_people = Rc::clone(&vips);
        let photos: Rc<RefCell<Option<Rc<ContactPhotos>>>> = Rc::default();
        let faces = Rc::clone(&photos);
        let bind: Rc<dyn Fn(&gtk::ListItem)> = Rc::new(move |item| {
            let (Some(slot), Some(object)) = (item.child(), item.item().and_downcast::<glib::BoxedAnyObject>())
            else {
                return;
            };
            let (Some(row), Some(line)) = (
                slot.first_child().and_downcast::<ThreadRow>(),
                slot.last_child().and_downcast::<BundleLine>(),
            ) else {
                return;
            };
            let band = match &*object.borrow::<Item>() {
                Item::Row(_, band) => *band,
                Item::Line(_, _, band) => Some(*band),
            };
            band_later(item, &slot, &object, band);
            let thread = match &*object.borrow::<Item>() {
                Item::Row(thread, _) => Rc::clone(thread),
                Item::Line(kind, shown, _) => {
                    row.set_visible(false);
                    line.set_visible(true);
                    item.set_selectable(false);
                    line.bind(kind, shown);
                    return;
                }
            };
            row.set_visible(true);
            line.set_visible(false);
            item.set_selectable(true);
            // The face the row shows: on a Follow Up or Outbox row, the
            // recipient's, not the sender's own.
            let sender = super::thread_row::person(&thread).1.to_lowercase();
            let vip = starred_people.borrow().contains(&sender);
            let (lines, bold, bold_senders, show_avatars) = row_appearance.get();
            let faces = faces
                .borrow()
                .clone()
                .filter(|faces| show_avatars && faces.any());
            let photo = faces.as_ref().and_then(|faces| faces.texture(&sender));
            let avatar = match (&faces, &photo) {
                (None, _) => Avatar::Hidden,
                (Some(_), Some(photo)) => Avatar::Photo(photo),
                (Some(_), None) => Avatar::Initials,
            };
            row.bind(&thread, shown.get(), vip, avatar);
            row.set_appearance(&thread, lines, bold, bold_senders);
        });
        let binds = Rc::clone(&bind);
        factory.connect_bind(move |_, item| {
            let item = item
                .downcast_ref::<gtk::ListItem>()
                .expect("list items are ListItems");
            binds(item);
        });
        let view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .css_classes(["navigation-sidebar", "thread-list"])
            .single_click_activate(false)
            // One Tab stop for the whole list: Tab and Shift+Tab leave it,
            // and the arrows, Home, End, Page Up and Page Down move
            // between rows, opening each as before.
            .tab_behavior(gtk::ListTabBehavior::Item)
            .build();
        view.add_controller(menu_keys());
        // Set by a click in the list, or by the list moving the focus
        // itself, so the focus that arrives is left where it lands.
        let steered = Rc::new(Cell::new(false));
        land_on_selection(&view, &selection, Rc::clone(&steered));
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&view)
            .build();
        row_popover.set_parent(&scroller);
        let popover = row_popover.clone();
        scroller.connect_destroy(move |_| popover.unparent());
        let empty = adw::StatusPage::builder()
            .icon_name("penguin-mail-inbox-symbolic")
            .title(gettext("No Mail"))
            .build();
        empty.add_css_class("compact");
        let spinner = adw::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&scroller, Some("list"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&spinner, Some("loading"));

        let search_entry = gtk::SearchEntry::builder()
            // The two search terms are Gmail's own and stay in English.
            .placeholder_text(gettext("Search mail, e.g. from:ann has:attachment"))
            .hexpand(true)
            .build();
        let search_bar = gtk::SearchBar::builder()
            .child(
                &adw::Clamp::builder()
                    .maximum_size(520)
                    .child(&search_entry)
                    .build(),
            )
            .show_close_button(false)
            .build();
        search_bar.connect_entry(&search_entry);

        // "All Inboxes" leads the list column, with the unread count and
        // category under it, both flush left where the chips and the rows
        // start. `adw::WindowTitle` centres its two labels on each other
        // and in the bar, so two plain labels stand in for it.
        let title = gtk::Label::builder()
            .label(gettext("All Inboxes"))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["title"])
            .build();
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .visible(false)
            .css_classes(["subtitle"])
            .build();
        let titles = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Start)
            .css_classes(["list-header-title"])
            .build();
        titles.append(&title);
        titles.append(&subtitle);
        let sidebar_button = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text(gettext("Show Mailboxes"))
            .visible(false)
            .build();
        let compose_button = gtk::Button::builder()
            .icon_name("mail-message-new-symbolic")
            .tooltip_text(gettext("New Message (C)"))
            .action_name("win.compose")
            .build();
        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text(gettext("Search (/)"))
            .build();
        let group_button = adw::SplitButton::builder()
            .icon_name("penguin-mail-bundle-symbolic")
            .tooltip_text(gettext("Group by Topic"))
            .dropdown_tooltip(gettext("Choose How to Group"))
            .popover(&topic_spans())
            .action_name("win.group-topics")
            .build();
        search_button
            .bind_property("active", &search_bar, "search-mode-enabled")
            .bidirectional()
            .sync_create()
            .build();
        // The tooltips already say what these do; the spoken name takes
        // the words and leaves the keys to a property of their own.
        super::name(&search_entry, &gettext("Search mail"));
        super::name(&sidebar_button, &gettext("Show Mailboxes"));
        super::name(&group_button, &gettext("Group by Topic"));
        super::name_arrow(
            &group_button,
            &gettext("Choose How to Group"),
            &gettext("Draws mail of one topic together in the list"),
        );
        for button in [
            compose_button.upcast_ref::<gtk::Widget>(),
            search_button.upcast_ref(),
        ] {
            let tip = button.tooltip_text().unwrap_or_default();
            super::name_with_shortcut(button, &tip);
        }
        let header = adw::HeaderBar::builder().show_title(false).build();
        header.pack_start(&sidebar_button);
        header.pack_start(&titles);
        header.pack_end(&compose_button);
        header.pack_end(&search_button);
        header.pack_end(&group_button);

        let categories_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let banner = adw::Banner::builder()
            .use_markup(false)
            .revealed(false)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&categories_slot);
        toolbar.add_top_bar(&search_bar);
        toolbar.add_top_bar(&banner);
        let grant_bars = gtk::Box::new(gtk::Orientation::Vertical, 0);
        toolbar.add_top_bar(&grant_bars);
        // A page can contain only replies to one conversation, leaving
        // no scrollbar. Keep the next server page reachable by a button.
        let more = gtk::Button::builder()
            .label(gettext("More"))
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .visible(false)
            .build();
        super::name(&more, &gettext("More"));
        stack.set_vexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&stack);
        content.append(&more);
        toolbar.set_content(Some(&content));
        let bottom_slot = adw::Bin::builder()
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        toolbar.add_bottom_bar(&bottom_slot);
        toolbar.set_reveal_bottom_bars(false);
        let page = adw::NavigationPage::builder()
            .title(gettext("Mail"))
            .tag("list")
            .child(&toolbar)
            .build();

        let list = Rc::new(ThreadList {
            page,
            sidebar_button,
            search_button,
            group_button,
            categories_slot,
            banner,
            grant_bars,
            search_entry,
            search_bar,
            title,
            subtitle,
            header,
            compose_button,
            bottom_slot,
            toolbar,
            narrow: Cell::new(false),
            stack,
            empty,
            store,
            selection,
            view,
            scroller,
            more,
            shown: state,
            steered,
            on_more: RefCell::new(None),
            on_topic_drop: RefCell::new(None),
            dragged,
            show_accounts,
            appearance,
            vips,
            photos,
            made,
            bind,
            muted: Cell::new(false),
            row_popover,
        });
        *me.borrow_mut() = Rc::downgrade(&list);
        // A search's subtitle is its query, which the open entry already
        // shows; it comes back when the entry closes.
        let weak = Rc::downgrade(&list);
        list.search_bar
            .connect_search_mode_enabled_notify(move |_| {
                if let Some(list) = weak.upgrade() {
                    list.show_subtitle();
                }
            });
        let weak = Rc::downgrade(&list);
        list.search_entry.connect_changed(move |_| {
            if let Some(list) = weak.upgrade() {
                list.show_subtitle();
            }
        });
        let weak = Rc::downgrade(&list);
        list.selection.connect_selection_changed(move |_, _, _| {
            let Some(list) = weak.upgrade() else { return };
            if list.muted.get() {
                return;
            }
            list.drop_lines_from_selection();
            let keys = list.selected_keys();
            list.shown.borrow_mut().kept.extend(keys);
            on_select(list.picked());
        });
        list.search_entry.connect_activate(move |entry| {
            let query = entry.text().trim().to_string();
            if !query.is_empty() {
                on_search(query);
            }
        });
        list
    }

    pub fn set_title(&self, title: &str, subtitle: &str) {
        self.title.set_label(title);
        self.subtitle.set_label(subtitle);
        self.show_subtitle();
        self.page.set_title(title);
    }

    /// Shows the subtitle when the layout has room for it and it says
    /// something the search entry under it does not. The subtitle hangs
    /// 7 px below the header bar (style.css, `.list-header-title`), into
    /// room the chips leave under it. A mailbox with no chips keeps that
    /// room empty, so a banner or the first row under the header does not
    /// cover the subtitle.
    pub(crate) fn show_subtitle(&self) {
        let layout = super::narrow_header::header_layout(self.narrow.get());
        let query = self.search_entry.text();
        let searching = self.search_bar.is_search_mode().then_some(query.as_str());
        let shows = subtitle_shows(layout.subtitle, &self.subtitle.label(), searching);
        self.subtitle.set_visible(shows);
        self.categories_slot
            .set_height_request(if shows { 7 } else { 0 });
    }

    /// Lays the header out for a phone or for a wider window. On a phone
    /// the unread line goes, the title shrinks a step, and New Message
    /// moves to a bar at the foot of the list with its name beside the
    /// icon.
    pub fn set_narrow(&self, narrow: bool) {
        if self.narrow.replace(narrow) == narrow {
            return;
        }
        let layout = super::narrow_header::header_layout(narrow);
        self.show_subtitle();
        match layout.small_title {
            true => self.title.add_css_class("small"),
            false => self.title.remove_css_class("small"),
        }
        if layout.new_below {
            self.header.remove(&self.compose_button);
            let content = adw::ButtonContent::builder()
                .icon_name("mail-message-new-symbolic")
                .label(gettext("New Message"))
                .build();
            self.compose_button.set_child(Some(&content));
            for class in ["suggested-action", "pill", "bottom-new"] {
                self.compose_button.add_css_class(class);
            }
            self.bottom_slot.set_child(Some(&self.compose_button));
        } else {
            self.bottom_slot.set_child(None::<&gtk::Widget>);
            self.compose_button
                .set_icon_name("mail-message-new-symbolic");
            for class in ["suggested-action", "pill", "bottom-new"] {
                self.compose_button.remove_css_class(class);
            }
            // New sits at the outer end with Search and Group by Topic
            // inside it, so those come off and go back after New.
            self.header.remove(&self.search_button);
            self.header.remove(&self.group_button);
            self.header.pack_end(&self.compose_button);
            self.header.pack_end(&self.search_button);
            self.header.pack_end(&self.group_button);
        }
        self.toolbar.set_reveal_bottom_bars(layout.new_below);
    }

    pub fn set_show_accounts(&self, show: bool) {
        if self.show_accounts.replace(show) != show {
            // Rebind every row so account dots appear or disappear.
            self.rebind();
        }
    }

    /// Draws every row again, for example after account colours change.
    /// Only the rows the list has made are drawn now; the rest read the
    /// same settings when they scroll into view.
    pub fn rebind(&self) {
        self.redraw_made(true, |_| true);
    }

    /// Hands the list the contact photos its rows show. A photo not read
    /// yet arrives later, and only the rows showing that person are drawn
    /// again.
    pub fn set_photos(self: &Rc<Self>, photos: Rc<ContactPhotos>) {
        let list = Rc::downgrade(self);
        photos.on_texture(move |addresses| match list.upgrade() {
            Some(list) => {
                list.photos_arrived(addresses);
                true
            }
            None => false,
        });
        *self.photos.borrow_mut() = Some(photos);
        self.rebind();
    }

    /// Draws every row again after the photos on disk changed: contacts
    /// loaded, or were turned off.
    pub fn photos_changed(&self) {
        self.rebind();
    }

    /// Draws again the rows on screen whose person's photo just arrived.
    fn photos_arrived(&self, addresses: &[String]) {
        self.redraw_made(false, |thread| {
            let person = super::thread_row::person(thread).1;
            addresses.iter().any(|a| a.eq_ignore_ascii_case(person))
        });
    }

    /// Draws again the rows the list made whose thread `which` accepts,
    /// and every bundle's line when `lines` asks for them too.
    fn redraw_made(&self, lines: bool, which: impl Fn(&ThreadSummary) -> bool) {
        let rows: Vec<ThreadRow> = {
            let mut made = self.made.borrow_mut();
            made.retain(|row| row.upgrade().is_some());
            made.iter().filter_map(|row| row.upgrade()).collect()
        };
        for row in rows {
            let Some(item) = row.item() else { continue };
            let showing = item
                .item()
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|object| match &*object.borrow::<Item>() {
                    Item::Row(thread, _) => which(thread),
                    Item::Line(..) => lines,
                });
            if showing {
                (self.bind)(&item);
            }
        }
    }

    pub fn set_appearance(
        &self,
        lines: crate::settings::PreviewLines,
        bold: bool,
        bold_senders: bool,
        show_avatars: bool,
    ) {
        let appearance = (lines, bold, bold_senders, show_avatars);
        if self.appearance.replace(appearance) != appearance {
            self.rebind();
        }
    }

    /// Checks the viewport rather than all bound rows, which GTK also keeps offscreen.
    pub fn connect_previews(
        self: &Rc<Self>,
        visible: impl Fn(Vec<mailrs_domain::Target>) + 'static,
    ) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            let Some(list) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let targets = list.visible_previews();
            visible(targets);
            glib::ControlFlow::Continue
        });
    }

    fn visible_previews(&self) -> Vec<mailrs_domain::Target> {
        if self.appearance.get().0 == crate::settings::PreviewLines::None || !self.view.is_mapped()
        {
            return Vec::new();
        }
        let mut visible = Vec::new();
        for row in self.made.borrow().iter().filter_map(|row| row.upgrade()) {
            let Some(bounds) = row.compute_bounds(&self.scroller) else {
                continue;
            };
            if bounds.width() <= 0.0
                || bounds.height() <= 0.0
                || bounds.y() >= self.scroller.height() as f32
                || bounds.y() + bounds.height() <= 0.0
            {
                continue;
            }
            let Some(object) = row
                .item()
                .and_then(|item| item.item())
                .and_downcast::<glib::BoxedAnyObject>()
            else {
                continue;
            };
            if let Item::Row(thread, _) = &*object.borrow::<Item>()
                && thread.snippet.is_empty()
                && !thread.scheduled
            {
                visible.push((bounds.y(), mailrs_domain::Target::for_preview(thread)));
            }
        }
        visible.sort_by(|a, b| a.0.total_cmp(&b.0));
        visible.into_iter().map(|(_, target)| target).collect()
    }

    pub fn preview_arrived(&self, target: &mailrs_domain::Target, text: &str) {
        if text.is_empty() {
            return;
        }
        let waiting = |row: &Row| mailrs_domain::Target::for_preview(row) == *target && row.snippet.is_empty();
        let mut shown = self.shown.borrow_mut();
        for row in shown.rows.iter_mut().filter(|row| waiting(row)) {
            Rc::make_mut(row).snippet = text.to_string();
        }
        // The items change in place rather than through a splice, so the
        // selection and the scroll stay where they are.
        for (index, item) in shown.items.iter_mut().enumerate() {
            let Item::Row(row, _) = item else { continue };
            if !waiting(row) {
                continue;
            }
            Rc::make_mut(row).snippet = text.to_string();
            if let Some(object) = self.store.item(index as u32).and_downcast::<glib::BoxedAnyObject>() {
                *object.borrow_mut::<Item>() = item.clone();
            }
        }
        drop(shown);
        self.redraw_made(false, |row| mailrs_domain::Target::for_preview(row) == *target);
    }

    /// Marks rows from these addresses as VIP mail.
    pub fn set_vips(&self, vips: HashSet<String>) {
        if *self.vips.borrow() != vips {
            *self.vips.borrow_mut() = vips;
            self.rebind();
        }
    }

    /// The rows loaded so far, for asking the mailbox for the page after
    /// them. The mailbox may hold more.
    pub fn loaded(&self) -> mailrs_sync::Loaded {
        mailrs_sync::Loaded::rows(&self.shown.borrow().rows)
    }

    /// Runs `more` near the list's end or when the reader asks for the
    /// next server page with the button, even when no scrollbar is needed,
    /// and when a jump to a topic's next place needs rows not loaded yet.
    pub fn connect_more(&self, more: impl Fn() + 'static) {
        let more: Rc<dyn Fn()> = Rc::new(more);
        *self.on_more.borrow_mut() = Some(Rc::clone(&more));
        let clicked = Rc::clone(&more);
        self.more.connect_clicked(move |_| clicked());
        self.scroller
            .vadjustment()
            .connect_value_changed(move |adjustment| {
                let seen = adjustment.value() + adjustment.page_size();
                if adjustment.upper() - seen < adjustment.page_size() {
                    more();
                }
            });
    }

    /// Says whether the mailbox has rows past the last one loaded, where
    /// a topic may show again.
    pub fn set_more(&self, more: bool) {
        let changed = {
            let mut shown = self.shown.borrow_mut();
            let changed = shown.more != more;
            shown.more = more;
            if !more {
                shown.jump = None;
            }
            changed
        };
        if changed {
            self.relayout();
        }
    }

    /// Shows Group by Topic pressed or not. A split button's main half is
    /// a plain button, so the look comes from the stylesheet and a screen
    /// reader hears the pressed state set here.
    pub fn show_grouping(&self, on: bool) {
        match on {
            true => self.group_button.add_css_class("pressed"),
            false => self.group_button.remove_css_class("pressed"),
        }
        let pressed = match on {
            true => gtk::AccessibleTristate::True,
            false => gtk::AccessibleTristate::False,
        };
        let mut child = self.group_button.first_child();
        while let Some(widget) = child {
            if widget.is::<gtk::Button>() {
                widget.update_state(&[gtk::accessible::State::Pressed(pressed)]);
            }
            child = widget.next_sibling();
        }
    }

    /// Gathers rows into bundles over `span`, or not at all: a search
    /// lists its hits as they come.
    pub fn set_bundling(&self, span: Option<TopicSpan>) {
        let changed = std::mem::replace(&mut self.shown.borrow_mut().bundling, span) != span;
        if changed {
            self.relayout();
        }
    }

    /// Forgets which bundles were open and which read rows they kept, as
    /// the list turns to another mailbox.
    pub fn forget_bundles(&self) {
        let mut shown = self.shown.borrow_mut();
        shown.opened.clear();
        shown.kept.clear();
        shown.jump = None;
    }

    /// Runs `put` with a bundle's topic when rows are dropped on it; it
    /// says whether it took them.
    pub fn connect_topic_drop(&self, put: impl Fn(Option<String>) -> bool + 'static) {
        *self.on_topic_drop.borrow_mut() = Some(Box::new(put));
    }

    pub fn show_loading(&self) {
        self.more.set_visible(false);
        self.stack.set_visible_child_name("loading");
    }

    /// Only cursor-paged server mailboxes need a manual next page: a
    /// grouped page may be too short to scroll, or may need a retry.
    pub fn show_more(&self, more: bool) {
        self.more.set_visible(more);
    }

    /// The line under the empty list's title, saying what fills the
    /// mailbox; empty for none.
    pub fn set_empty_description(&self, description: &str) {
        self.empty
            .set_description((!description.is_empty()).then_some(description));
    }

    /// Updates the list with one splice, keeping the selected rows selected
    /// when they are still present.
    pub fn set_rows(&self, rows: Vec<Row>, empty_title: &str, empty_icon: &str) {
        self.shown.borrow_mut().rows = rows;
        self.relayout();
        self.empty.set_title(empty_title);
        self.empty.set_icon_name(Some(empty_icon));
        self.stack
            .set_visible_child_name(if self.shown.borrow().rows.is_empty() {
                "empty"
            } else {
                "list"
            });
    }

    /// Adds a page of older rows at the end, leaving the selection alone.
    /// A bundle near the end may gather some of them.
    pub fn append(&self, rows: Vec<Row>) {
        if rows.is_empty() {
            self.continue_jump();
            return;
        }
        self.shown.borrow_mut().rows.extend(rows);
        self.relayout();
        self.stack.set_visible_child_name("list");
        self.continue_jump();
    }

    /// Lays the rows out again and brings the model to the new layout in
    /// one splice, keeping the selected rows selected.
    fn relayout(&self) {
        let keys = self.selected_keys();
        let threaded;
        let (old, new) = {
            let mut shown = self.shown.borrow_mut();
            threaded = shown.rows.first().is_none_or(|r| r.message_id.is_none());
            let layout = match shown.bundling {
                Some(span) => {
                    let reach = reach(&shown.rows, span);
                    bundles::lay_out(&shown.rows, &reach, &shown.opened, &shown.kept)
                }
                None => bundles::flat(shown.rows.len()),
            };
            let items = items_of(&shown.rows, &layout, threaded, shown.more);
            shown.layout = layout;
            let old = std::mem::replace(&mut shown.items, items.clone());
            (old, items)
        };
        self.muted.set(true);
        // The row the keyboard is on, which holding the view still may move
        // the focus off for the change, and which takes it back after.
        let focused = self
            .focus_in_list()
            .then(|| self.focused_position())
            .flatten()
            .and_then(|at| match old.get(at)? {
                Item::Row(row, _) => Some(key(row)),
                Item::Line(..) => None,
            });
        // Only what changed is spliced, last first, so the items between
        // two changes stay the same objects and GTK keeps the view on them.
        let changes = edits(&old, &new);
        // The edits come last first, so the topmost change is the last.
        let held = changes
            .last()
            .and_then(|top| self.hold_view_above(top.position as usize, &changes));
        for change in changes {
            let added: Vec<glib::BoxedAnyObject> = new[change.added]
                .iter()
                .map(|item| glib::BoxedAnyObject::new(item.clone()))
                .collect();
            self.store.splice(change.position, change.removed, &added);
        }
        self.reselect(&keys);
        self.muted.set(false);
        if let Some(wanted) = focused {
            self.refocus(&new, &wanted);
        }
        if let Some((position, was)) = held {
            self.put_back(position, was);
        }
        let landing = self.shown.borrow_mut().landing.take();
        if let Some(landing) = landing {
            self.land_focus(&landing);
        }
    }

    /// Opens or closes the bundle whose line is at `position`. Its
    /// header's item stays as it was, so the view stays on it.
    fn toggle_at(&self, position: usize) {
        {
            let mut shown = self.shown.borrow_mut();
            // A bundle of one always shows its row; there is nothing to
            // open or fold.
            let Some(bundle) = shown.bundle_at(position).filter(|b| b.members.len() > 1) else {
                return;
            };
            let (topic, members) = (bundle.topic.clone(), bundle.members.clone());
            let Shown { opened, rows, kept, .. } = &mut *shown;
            opened.toggle(&topic, rows, &members, kept);
        }
        self.relayout();
    }

    /// Readies the focus for `rows` leaving their topic. The row that was
    /// its bundle's newest stays where it is, since the bundle sat at its
    /// place, so the focus stays on it; any other row goes back to its own
    /// place in the list, likely off screen, so the focus stays on the
    /// bundle it left, whose header the view keeps.
    pub fn leaving_topic(&self, rows: &[Row]) {
        let mut shown = self.shown.borrow_mut();
        let Some(first) = rows.first() else { return };
        let wanted = key(first);
        let Some(index) = shown.rows.iter().position(|r| key(r) == wanted) else {
            return;
        };
        let landing = shown
            .layout
            .bundles
            .iter()
            .find(|b| b.members.contains(&index))
            .map(|bundle| match bundle.members.first() == Some(&index) {
                true => Landing::Row(wanted.clone()),
                false => Landing::Bundle(bundle.topic.clone(), key(&shown.rows[bundle.members[0]])),
            });
        shown.landing = landing;
    }

    /// Gives the keyboard focus back to the row `wanted`, wherever `items`
    /// now put it. Holding the view still across a change moves the focus
    /// off a row below the change, and GTK may hold the focus to a place
    /// in the list rather than to the row there, so after a row above it
    /// leaves, as when mail is deleted and the list moves on, the focus
    /// would sit on another row than the one open. A row that left keeps
    /// the focus where the change put it.
    fn refocus(&self, items: &[Item], wanted: &Key) {
        let Some(at) = items
            .iter()
            .position(|item| matches!(item, Item::Row(row, _) if key(row) == *wanted))
        else {
            return;
        };
        if self.focused_position() == Some(at) {
            return;
        }
        match self.item_widget(at) {
            Some(widget) => {
                self.steered.set(true);
                widget.grab_focus();
            }
            None => self
                .view
                .scroll_to(at as u32, gtk::ListScrollFlags::FOCUS, None),
        }
    }

    /// Puts the focus where `landing` says, when the keyboard is in the
    /// list and the item is on screen; nothing scrolls.
    fn land_focus(&self, landing: &Landing) {
        let in_list = self.focus_in_list();
        if !in_list {
            return;
        }
        let position = {
            let shown = self.shown.borrow();
            match landing {
                Landing::Row(wanted) => shown
                    .rows
                    .iter()
                    .position(|r| key(r) == *wanted)
                    .and_then(|row| shown.layout.entry_of_row(row)),
                Landing::Bundle(topic, member) => shown.bundle_holding(topic, member).map(|b| b.at),
            }
        };
        if let Some(widget) = position.and_then(|p| self.item_widget(p)) {
            self.steered.set(true);
            widget.grab_focus();
        }
    }

    /// Keeps the view where it is across `changes`, whose topmost one
    /// starts at `top`, and answers the item on screen just above it and
    /// where it sits, for [`ThreadList::put_back`] to hold it there.
    ///
    /// GTK holds the view on the item that has the list's focus, whether
    /// or not the keyboard is in the list. When a change takes that item
    /// away, GTK holds the same place in the list instead, which by then
    /// belongs to another item; and an item below a change stays still
    /// while the rows above it grow or shrink. Either way the rows the
    /// person was looking at move. So a keyboard focus in the list goes
    /// first to the item just above the topmost change, which no change
    /// moves; a focus elsewhere in the window stays where it is, and the
    /// measured item is put back where it was once GTK has laid the list
    /// out again.
    fn hold_view_above(&self, top: usize, changes: &[crate::diff::Splice]) -> Option<(usize, f32)> {
        let above = top.checked_sub(1)?;
        let widget = self.item_widget(above)?;
        let was = self.top_of(&widget)?;
        let in_list = self.focus_in_list();
        let focused = self.focused_position();
        let touched = focused.is_some_and(|f| {
            f >= top
                || changes
                    .iter()
                    .any(|c| f >= c.position as usize && f < (c.position + c.removed) as usize)
        });
        if in_list && touched {
            self.steered.set(true);
            widget.grab_focus();
        }
        Some((above, was))
    }

    /// Scrolls so the item at `position` sits at `was` again once GTK has
    /// laid the list out after a change.
    fn put_back(&self, position: usize, was: f32) {
        let (list, scroller) = (self.view.downgrade(), self.scroller.downgrade());
        self.view.add_tick_callback(move |_, _| {
            let (Some(list), Some(scroller)) = (list.upgrade(), scroller.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            let now = item_at(&list, position).and_then(|w| w.compute_point(&scroller, &graphene::Point::zero()));
            if let Some(now) = now {
                let moved = f64::from(now.y() - was);
                if moved.abs() >= 1.0 {
                    let adjustment = scroller.vadjustment();
                    adjustment.set_value(adjustment.value() + moved);
                }
            }
            glib::ControlFlow::Break
        });
    }

    /// Where `widget` sits in the scrolled window, from its top.
    fn top_of(&self, widget: &gtk::Widget) -> Option<f32> {
        widget
            .compute_point(&self.scroller, &graphene::Point::zero())
            .map(|p| p.y())
    }

    /// Whether the keyboard focus is in the list: on a row, or on the
    /// scrolled window around it, where it goes back to when the row menu,
    /// which belongs to that window, closes.
    fn focus_in_list(&self) -> bool {
        let scroller = self.scroller.upcast_ref::<gtk::Widget>();
        self.view
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| &focus == scroller || focus.is_ancestor(scroller))
    }

    /// The position of the item the list's focus is on.
    fn focused_position(&self) -> Option<usize> {
        self.view.focus_child().and_then(|item| position_in(&item))
    }

    /// The widget GTK shows the item at `position` in, while it is on
    /// screen.
    fn item_widget(&self, position: usize) -> Option<gtk::Widget> {
        item_at(&self.view, position)
    }

    /// Goes from the bundle whose line is at `position` to the place its
    /// topic shows next above (`way` -1) or below (1). Below, when no
    /// loaded row has the topic, the list loads more rows and looks again.
    fn jump_from(&self, position: usize, way: i32) {
        let (target, waiting) = {
            let shown = self.shown.borrow();
            let Some(bundle) = shown.bundle_at(position) else {
                return;
            };
            let target = match way < 0 {
                true => bundle.earlier,
                false => bundle.later,
            };
            let first = key(&shown.rows[bundle.members[0]]);
            (target, (bundle.topic.clone(), first))
        };
        match target {
            Some(target) => self.land_on(target),
            None if way > 0 && self.shown.borrow().more => {
                let (topic, first) = waiting;
                self.shown.borrow_mut().jump = Some((topic, first, JUMP_PAGES));
                self.ask_more();
            }
            None => {}
        }
    }

    /// After a page arrives, finishes a jump down that waited for it, or
    /// asks for another page while it may.
    fn continue_jump(&self) {
        let Some((topic, first, pages)) = self.shown.borrow_mut().jump.take() else {
            return;
        };
        let (target, more) = {
            let shown = self.shown.borrow();
            let target = shown.bundle_holding(&topic, &first).and_then(|b| b.later);
            (target, shown.more)
        };
        match target {
            Some(target) => self.land_on(target),
            None if more && pages > 1 => {
                self.shown.borrow_mut().jump = Some((topic, first, pages - 1));
                self.ask_more();
            }
            None => {}
        }
    }

    fn ask_more(&self) {
        let more = self.on_more.borrow().clone();
        if let Some(more) = more {
            more();
        }
    }

    /// Scrolls to the item at `position` and puts the focus on it.
    fn land_on(&self, position: usize) {
        self.view
            .scroll_to(position as u32, gtk::ListScrollFlags::FOCUS, None);
    }

    /// What a drop at `position` does with the dragged rows: puts them on
    /// the topic of the bundle there, any line of it, or takes them off
    /// their topics on a row outside every bundle. `None` refuses the
    /// drop: rows already on that topic, or rows on no topic dropped
    /// outside a bundle.
    fn drop_choice(&self, position: usize) -> Option<Option<String>> {
        let shown = self.shown.borrow();
        let dragged = self.dragged.borrow();
        if dragged.is_empty() {
            return None;
        }
        match shown.bundle_of(position) {
            Some(bundle) => {
                let topic = &bundle.topic;
                let all_there = dragged
                    .iter()
                    .all(|row| row.topics.iter().any(|t| mailrs_domain::topic::same(t, topic)));
                (!all_there).then(|| Some(topic.clone()))
            }
            None => dragged.iter().any(|row| !row.topics.is_empty()).then_some(None),
        }
    }

    /// Lights up where a drop at `position` lands: the whole bundle there,
    /// or the one row outside every bundle.
    fn show_drop(&self, position: usize) {
        self.clear_drop();
        let positions: Vec<usize> = {
            let shown = self.shown.borrow();
            match shown.bundle_index(position) {
                Some(bundle) => (0..shown.layout.entries.len())
                    .filter(|p| shown.bundle_index(*p) == Some(bundle))
                    .collect(),
                None => vec![position],
            }
        };
        for position in positions {
            if let Some(widget) = self.item_widget(position) {
                widget.add_css_class("drop-here");
            }
        }
    }

    fn clear_drop(&self) {
        let mut child = self.view.first_child();
        while let Some(widget) = child {
            widget.remove_css_class("drop-here");
            child = widget.next_sibling();
        }
    }

    /// Hands the dragged rows to what a drop at `position` does.
    fn dropped_on(&self, position: usize) -> bool {
        self.clear_drop();
        let Some(choice) = self.drop_choice(position) else {
            return false;
        };
        match self.on_topic_drop.borrow().as_ref() {
            Some(put) => put(choice),
            None => false,
        }
    }

    /// Takes the lines a bundle adds out of the selection, which a range
    /// or Select All sweeps up along with the rows.
    fn drop_lines_from_selection(&self) {
        let lines: Vec<u32> = {
            let shown = self.shown.borrow();
            self.positions()
                .into_iter()
                .filter(|p| shown.layout.row_at(*p).is_none())
                .map(|p| p as u32)
                .collect()
        };
        if lines.is_empty() {
            return;
        }
        self.muted.set(true);
        for position in lines {
            self.selection.unselect_item(position);
        }
        self.muted.set(false);
    }

    /// Puts fresh rows in place of the ones belonging to `changed` threads.
    /// Rows that came back keep the list in date order; the rest drop out.
    /// Which fresh rows belong below the last one loaded is the list
    /// feed's call (`ListFeed::place`), made before they get here.
    pub fn replace_threads(&self, changed: &[(AccountId, String)], fresh: Vec<ThreadSummary>) {
        let touched = |row: &ThreadSummary| {
            changed
                .iter()
                .any(|(account_id, id)| *account_id == row.account_id && *id == row.id)
        };
        let mut rows: Vec<Row> = self
            .shown
            .borrow()
            .rows
            .iter()
            .filter(|row| !touched(row))
            .map(Rc::clone)
            .collect();
        for row in fresh {
            let row = Rc::new(row);
            let at = rows.partition_point(|held| order(held) > order(&row));
            rows.insert(at, row);
        }
        let title = self.empty.title().to_string();
        let icon = self
            .empty
            .icon_name()
            .map(|i| i.to_string())
            .unwrap_or_default();
        self.set_rows(rows, &title, &icon);
    }

    /// The keys of the selected rows, read off the items the model holds
    /// now. The rows may already be the next ones, as when `set_rows` has
    /// stored them and the layout has not caught up, and a position read
    /// through them would name whatever row took that place.
    fn selected_keys(&self) -> HashSet<Key> {
        let shown = self.shown.borrow();
        self.positions()
            .into_iter()
            .filter_map(|p| match shown.items.get(p)? {
                Item::Row(row, _) => Some(key(row)),
                Item::Line(..) => None,
            })
            .collect()
    }

    /// Selects exactly the rows whose keys are in `keys`, in one change to
    /// the selection rather than one per row.
    fn reselect(&self, keys: &HashSet<Key>) {
        let shown = self.shown.borrow();
        let selected = gtk::Bitset::new_empty();
        for position in positions_of(&shown, keys) {
            selected.add(position);
        }
        let every = gtk::Bitset::new_range(0, shown.items.len() as u32);
        self.selection.set_selection(&selected, &every);
    }

    /// The selected positions, in order, read off the selection itself
    /// rather than asked of every row.
    fn positions(&self) -> Vec<usize> {
        let selected = self.selection.selection();
        match gtk::BitsetIter::init_first(&selected) {
            Some((rest, first)) => std::iter::once(first)
                .chain(rest)
                .map(|p| p as usize)
                .collect(),
            None => Vec::new(),
        }
    }

    /// How many rows are selected.
    pub fn selected_count(&self) -> usize {
        let shown = self.shown.borrow();
        self.positions()
            .into_iter()
            .filter(|p| shown.layout.row_at(*p).is_some())
            .count()
    }

    pub fn picked(&self) -> Picked {
        let mut picked = self.selected_rows();
        match picked.len() {
            0 => Picked::None,
            1 => Picked::One(picked.remove(0)),
            _ => Picked::Many(picked),
        }
    }

    pub fn selected_rows(&self) -> Vec<Row> {
        let shown = self.shown.borrow();
        self.positions()
            .into_iter()
            .filter_map(|p| shown.row_at(p).map(Rc::clone))
            .collect()
    }

    /// Selects a row alone: the thread's first row, or the given message's
    /// row. A closed bundle that hides it opens.
    pub fn select(&self, account_id: AccountId, thread_id: &str, message_id: Option<&str>) {
        let found = self.shown.borrow().rows.iter().position(|t| {
            t.account_id == account_id
                && t.id == thread_id
                && message_id.is_none_or(|m| t.message_id.as_deref() == Some(m))
        });
        let Some(row) = found else {
            return;
        };
        let hiding = self
            .shown
            .borrow()
            .layout
            .hiding(row)
            .map(|b| (b.topic.clone(), b.members.clone()));
        if let Some((topic, members)) = hiding {
            {
                let mut shown = self.shown.borrow_mut();
                let Shown { opened, rows, kept, .. } = &mut *shown;
                opened.toggle(&topic, rows, &members, kept);
            }
            self.relayout();
        }
        let position = self.shown.borrow().layout.entry_of_row(row);
        if let Some(position) = position {
            self.selection.select_item(position as u32, true);
            self.view
                .scroll_to(position as u32, gtk::ListScrollFlags::FOCUS, None);
        }
    }

    pub fn unselect(&self) {
        self.muted.set(true);
        self.selection.unselect_all();
        self.muted.set(false);
    }

    pub fn select_all(&self) {
        self.selection.select_all();
    }

    /// Moves to the row `delta` rows away from the last selected one and
    /// opens it, passing over the lines bundles add.
    pub fn step(&self, delta: i32) {
        let next = {
            let shown = self.shown.borrow();
            let rows: Vec<usize> = (0..shown.items.len())
                .filter(|p| shown.layout.row_at(*p).is_some())
                .collect();
            let Some(last) = rows.len().checked_sub(1) else {
                return;
            };
            let at = match self.positions().last() {
                None => 0,
                Some(&current) => {
                    let here = rows.partition_point(|p| *p < current) as i64;
                    (here + delta as i64).clamp(0, last as i64) as usize
                }
            };
            rows[at]
        };
        self.selection.select_item(next as u32, true);
        self.view
            .scroll_to(next as u32, gtk::ListScrollFlags::FOCUS, None);
    }

    /// The row to open once the selected rows leave the list: the first
    /// unselected row after them, else the last one before them.
    pub fn neighbour_of_selected(&self) -> Option<ThreadSummary> {
        let selected = self.selection.selection();
        if selected.is_empty() {
            return None;
        }
        let shown = self.shown.borrow();
        let rows: Vec<usize> = (0..shown.items.len())
            .filter(|p| shown.layout.row_at(*p).is_some())
            .collect();
        let first = rows.partition_point(|p| *p < selected.minimum() as usize);
        let last = rows
            .partition_point(|p| *p <= selected.maximum() as usize)
            .checked_sub(1)?;
        let at = neighbour(rows.len(), first, last)?;
        shown.row_at(rows[at]).map(|row| (**row).clone())
    }

    /// Keeps only the rows `keep` accepts, leaving the rest selected as they were.
    pub fn retain(&self, keep: impl Fn(&ThreadSummary) -> bool) {
        let rows: Vec<Row> = self
            .shown
            .borrow()
            .rows
            .iter()
            .filter(|r| keep(r))
            .map(Rc::clone)
            .collect();
        let title = self.empty.title().to_string();
        let icon = self
            .empty
            .icon_name()
            .map(|i| i.to_string())
            .unwrap_or_default();
        self.set_rows(rows, &title, &icon);
    }

    /// Runs `open` with a row that was double-clicked or activated with
    /// Enter. A bundle's line opens or closes the bundle instead.
    pub fn connect_open(self: &Rc<Self>, open: impl Fn(ThreadSummary) + 'static) {
        let weak = Rc::downgrade(self);
        self.view.connect_activate(move |_, position| {
            let Some(list) = weak.upgrade() else { return };
            let row = list
                .shown
                .borrow()
                .row_at(position as usize)
                .map(|row| (**row).clone());
            match row {
                Some(row) => open(row),
                None => list.toggle_at(position as usize),
            }
        });
    }

    /// The rows of the last drag.
    pub fn dragged(&self) -> Vec<Row> {
        self.dragged.borrow().clone()
    }

    pub fn open_search(&self) {
        self.search_bar.set_search_mode(true);
        self.search_entry.grab_focus();
    }

    pub fn close_search(&self) {
        self.search_bar.set_search_mode(false);
    }

    pub fn search_open(&self) -> bool {
        self.search_bar.is_search_mode()
    }

    /// Makes `model` what a right click on a row offers. The conversation
    /// view owns the menu and its wording, so a row menu and More Actions
    /// never drift apart.
    pub fn set_row_menu(&self, model: &gio::MenuModel) {
        self.row_popover.set_menu_model(Some(model));
    }
}

/// Opens `popover` under the pointer on a right click or long press of
/// `row`, or at the row's corner from the keyboard. A click outside the
/// selection takes the row it landed on first, so the menu acts on what
/// the pointer is over rather than on whatever was selected before.
///
/// The popover belongs to the list rather than to the row. Taking a new
/// row opens its conversation, the list redraws, and a row can lose its
/// place on screen while the menu is up, which would take a menu of its
/// own down with it.
fn context_menu(
    row: &ThreadRow,
    item: &gtk::ListItem,
    selection: &gtk::MultiSelection,
    popover: &gtk::PopoverMenu,
) {
    let show: OpenMenu = {
        let (popover, item, selection) = (popover.clone(), item.clone(), selection.clone());
        let row = row.downgrade();
        Rc::new(move |at: Option<(f64, f64)>| {
            let (Some(row), Some(list)) = (row.upgrade(), popover.parent()) else {
                return;
            };
            let position = item.position();
            if position != gtk::INVALID_LIST_POSITION && !selection.is_selected(position) {
                selection.select_item(position, true);
            }
            let (x, y) = at.unwrap_or((16.0, 16.0));
            let Some(point) = row.compute_point(&list, &graphene::Point::new(x as f32, y as f32))
            else {
                return;
            };
            let point = gdk::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1);
            popover.set_pointing_to(Some(&point));
            popover.popup();
        })
    };
    let click = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    let open = Rc::clone(&show);
    click.connect_pressed(move |_, _, x, y| open(Some((x, y))));
    row.add_controller(click);
    let press = gtk::GestureLongPress::new();
    let open = Rc::clone(&show);
    press.connect_pressed(move |_, x, y| open(Some((x, y))));
    row.add_controller(press);
    row.set_menu(show);
}

/// Menu and Shift+F10 on the list, for the row with the focus. The focus
/// sits on the list item GTK wraps each row in, and the row is a child of
/// that item, so a key controller on the row would never see the key.
fn menu_keys() -> gtk::ShortcutController {
    let controller = gtk::ShortcutController::new();
    for keys in MENU_KEYS {
        let trigger = gtk::ShortcutTrigger::parse_string(keys);
        let action = gtk::CallbackAction::new(|list, _| {
            let opened = list
                .focus_child()
                .and_then(|item| row_in(&item))
                .is_some_and(|row| row.open_menu());
            match opened {
                true => glib::Propagation::Stop,
                false => glib::Propagation::Proceed,
            }
        });
        controller.add_shortcut(gtk::Shortcut::new(trigger, Some(action)));
    }
    controller
}

/// Moves the focus to the open conversation's row when Tab brings it
/// into the list, or to the first row when none is open (`entry`). GTK
/// would go back to the row the focus last left, which a reload or a
/// selection made elsewhere can leave somewhere else. A press in the list
/// brings the focus in to the row under the pointer, so that way in is
/// left alone.
fn land_on_selection(view: &gtk::ListView, selection: &gtk::MultiSelection, pressed: Rc<Cell<bool>>) {
    let press = gtk::GestureClick::builder().button(0).build();
    press.set_propagation_phase(gtk::PropagationPhase::Capture);
    let held = Rc::clone(&pressed);
    press.connect_pressed(move |_, _, _, _| held.set(true));
    view.add_controller(press);
    let focus = gtk::EventControllerFocus::new();
    let held = Rc::clone(&pressed);
    focus.connect_leave(move |_| held.set(false));
    let (list, selection) = (view.downgrade(), selection.clone());
    focus.connect_enter(move |_| {
        if pressed.replace(false) {
            return;
        }
        // The move waits for GTK to finish the focus change under way.
        let (list, selection) = (list.clone(), selection.clone());
        glib::idle_add_local_once(move || {
            let Some(list) = list.upgrade() else { return };
            let Some(row) = list.focus_child() else {
                return;
            };
            let focused = row_in(&row).and_then(|row| row.position());
            let chosen = selection.selection();
            let first = (!chosen.is_empty()).then(|| chosen.minimum());
            let held = focused.is_some_and(|at| chosen.contains(at));
            if let Some(to) = entry::landing(selection.n_items(), focused, held, first) {
                list.scroll_to(to, gtk::ListScrollFlags::FOCUS, None);
            }
        });
    });
    view.add_controller(focus);
}

/// The drop target on one item of the list, which asks the list what a
/// drop there does: put the dragged mail on the topic of the bundle the
/// item belongs to, take it off its topic on a row outside every bundle,
/// or nothing, which GTK then shows by refusing the drop.
fn drop_target(item: &gtk::ListItem, me: &Rc<RefCell<std::rc::Weak<ThreadList>>>) -> gtk::DropTarget {
    let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    let at = {
        let item = item.downgrade();
        move || {
            item.upgrade()
                .map(|i| i.position())
                .filter(|p| *p != gtk::INVALID_LIST_POSITION)
                .map(|p| p as usize)
        }
    };
    let list = {
        let me = Rc::clone(me);
        move || me.borrow().upgrade()
    };
    let (hover_at, hover_list) = (at.clone(), list.clone());
    let hover = move |_: &gtk::DropTarget, _: f64, _: f64| {
        let (Some(position), Some(list)) = (hover_at(), hover_list()) else {
            return gdk::DragAction::empty();
        };
        match list.drop_choice(position) {
            Some(_) => {
                list.show_drop(position);
                gdk::DragAction::MOVE
            }
            None => {
                list.clear_drop();
                gdk::DragAction::empty()
            }
        }
    };
    let hover_too = hover.clone();
    target.connect_enter(move |t, x, y| hover(t, x, y));
    target.connect_motion(move |t, x, y| hover_too(t, x, y));
    let leave_list = list.clone();
    target.connect_leave(move |_| {
        if let Some(list) = leave_list() {
            list.clear_drop();
        }
    });
    target.connect_drop(move |_, value, _, _| {
        value.get::<String>().is_ok_and(|v| v == DRAG_MAIL)
            && at().zip(list()).is_some_and(|(position, list)| list.dropped_on(position))
    });
    target
}

/// The widget `view` shows the item at `position` in, while it is on
/// screen.
fn item_at(view: &gtk::ListView, position: usize) -> Option<gtk::Widget> {
    let mut child = view.first_child();
    while let Some(widget) = child {
        if position_in(&widget) == Some(position) {
            return Some(widget);
        }
        child = widget.next_sibling();
    }
    None
}

/// The position of the item shown in `wrapper`, the widget GTK wraps a
/// list item in: its slot holds the row first and the bundle's line last.
fn position_in(wrapper: &gtk::Widget) -> Option<usize> {
    let slot = wrapper.first_child()?;
    let line = slot.last_child().and_downcast::<BundleLine>();
    let row = slot.first_child().and_downcast::<ThreadRow>();
    line.filter(|l| l.is_visible())
        .and_then(|l| l.position())
        .or_else(|| row.and_then(|r| r.position()))
        .map(|p| p as usize)
}

/// The row inside the widget GTK wraps a list item in, when the item
/// shows a row: the wrapper holds the slot, and the slot the row first.
fn row_in(wrapper: &gtk::Widget) -> Option<ThreadRow> {
    wrapper
        .first_child()?
        .first_child()
        .and_downcast::<ThreadRow>()
        .filter(|row| row.is_visible())
}

/// The positions of the rows whose keys are in `keys`, in order.
fn positions_of(shown: &Shown, keys: &HashSet<Key>) -> Vec<u32> {
    if keys.is_empty() {
        return Vec::new();
    }
    // Borrowed keys, so a row is looked up without copying its ids.
    let wanted: HashSet<(AccountId, &str, Option<&str>)> = keys
        .iter()
        .map(|(account_id, id, message_id)| (*account_id, id.as_str(), message_id.as_deref()))
        .collect();
    shown
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| match item {
            Item::Row(row, _) => {
                wanted.contains(&(row.account_id, row.id.as_str(), row.message_id.as_deref()))
            }
            Item::Line(..) => false,
        })
        .map(|(p, _)| p as u32)
        .collect()
}

/// The row to open once the rows from `first` to `last` leave a list of
/// `len`: the one after the last, else the one before the first.
fn neighbour(len: usize, first: usize, last: usize) -> Option<usize> {
    if last + 1 < len {
        Some(last + 1)
    } else {
        first.checked_sub(1)
    }
}

/// Whether the list header shows `subtitle`: the layout has room, it says
/// something, and it is not the query of a search whose entry is open
/// (`searching` holds the entry's text then).
fn subtitle_shows(room: bool, subtitle: &str, searching: Option<&str>) -> bool {
    room && !subtitle.is_empty() && searching.is_none_or(|query| query.trim() != subtitle.trim())
}

#[cfg(test)]
pub(crate) mod checks {
    use super::*;

    pub(crate) fn run() {
        crate::ui::thread_row::check_appearance();
        let list = ThreadList::new(|_| {}, |_| {});
        let calls = Rc::new(Cell::new(0));
        let clicked = Rc::clone(&calls);
        list.connect_more(move || clicked.set(clicked.get() + 1));
        assert!(!list.more.is_visible());
        list.show_more(true);
        assert!(list.more.is_visible());
        assert_eq!(list.more.label().as_deref(), Some(gettext("More").as_str()));
        list.more.emit_clicked();
        assert_eq!(calls.get(), 1);
        let row = ThreadSummary {
            account_id: 1,
            id: "plans".into(),
            preview_message_id: Some("matching-message".into()),
            from: "Ann".into(),
            subject: "Project plans".into(),
            snippet: "The updated plans are ready.".into(),
            last_message_at: mailrs_sync::now_millis(),
            message_count: 25,
            unread: true,
            ..Default::default()
        };
        list.set_title("Work / Clients", "");
        list.set_rows(
            vec![Rc::new(row.clone())],
            "No Mail",
            "mail-unread-symbolic",
        );
        list.selection.select_item(0, false);
        let mut empty = row.clone();
        empty.snippet.clear();
        list.set_rows(vec![Rc::new(empty)], "No Mail", "mail-unread-symbolic");
        list.preview_arrived(&mailrs_domain::Target::for_preview(&row), "Fetched preview");
        assert_eq!(list.shown.borrow().rows[0].snippet, "Fetched preview");
        assert!(list.selection.is_selected(0));
        assert_eq!(
            list.store
                .item(0)
                .unwrap()
                .downcast::<glib::BoxedAnyObject>()
                .unwrap()
                .borrow::<Item>()
                .clone(),
            Item::Row(list.shown.borrow().rows[0].clone(), None)
        );

        list.selection.select_item(0, false);
        list.set_rows(
            vec![Rc::new(ThreadSummary {
                message_count: 50,
                ..row
            })],
            "No Mail",
            "mail-unread-symbolic",
        );
        assert!(list.selection.is_selected(0));
        if let Some(directory) = std::env::var_os("PENGUIN_MAIL_TEST_LIST_SHOTS") {
            save_shots(&list, std::path::Path::new(&directory));
        }
        list.show_loading();
        assert!(!list.more.is_visible());
    }

    fn save_shots(list: &ThreadList, directory: &std::path::Path) {
        std::fs::create_dir_all(directory).unwrap();
        let navigation = adw::NavigationView::new();
        navigation.add(&list.page);
        let window = adw::Window::builder()
            .default_width(390)
            .default_height(640)
            .content(&navigation)
            .build();
        let style = adw::StyleManager::default();
        let previous = style.color_scheme();
        window.present();
        for (name, scheme) in [
            ("light", adw::ColorScheme::ForceLight),
            ("dark", adw::ColorScheme::ForceDark),
        ] {
            style.set_color_scheme(scheme);
            let wait = glib::MainLoop::new(None, false);
            let done = wait.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                done.quit()
            });
            wait.run();
            let paintable = gtk::WidgetPaintable::new(Some(&window));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(
                &snapshot,
                f64::from(window.width()),
                f64::from(window.height()),
            );
            let texture = window
                .renderer()
                .unwrap()
                .render_texture(snapshot.to_node().unwrap(), None);
            texture
                .save_to_png(directory.join(format!("folder-more-{name}.png")))
                .unwrap();
        }
        window.close();
        style.set_color_scheme(previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> Row {
        Rc::new(ThreadSummary {
            account_id: 1,
            id: id.into(),
            ..ThreadSummary::default()
        })
    }

    fn in_topic(id: &str, topic: &str, unread: bool) -> Row {
        Rc::new(ThreadSummary {
            account_id: 1,
            id: id.into(),
            unread,
            topics: vec![topic.into()],
            ..ThreadSummary::default()
        })
    }

    /// The list showing `rows`, bundled as the list bundles them.
    fn shown(rows: Vec<Row>, more: bool) -> Shown {
        let layout = bundles::lay_out(&rows, &bundles::NEARBY, &Opened::default(), &HashSet::new());
        let items = items_of(&rows, &layout, true, more);
        Shown {
            rows,
            layout,
            items,
            bundling: Some(TopicSpan::Nearby),
            more,
            ..Shown::default()
        }
    }

    #[test]
    fn a_selection_finds_its_rows_again_after_the_list_changes() {
        let rows: Vec<Row> = ["a", "b", "c", "d"].into_iter().map(row).collect();
        let keys: HashSet<Key> = [key(&rows[3]), key(&rows[1]), (1, "gone".into(), None)]
            .into_iter()
            .collect();
        let shown = shown(rows, false);
        assert_eq!(positions_of(&shown, &keys), [1, 3]);
        assert!(positions_of(&shown, &HashSet::new()).is_empty());
    }

    #[test]
    fn a_position_names_a_row_only_where_the_model_shows_one() {
        let rows = vec![
            in_topic("a", "Q3", true),
            row("b"),
            in_topic("c", "Q3", false),
        ];
        let keys: HashSet<Key> = rows.iter().map(|r| key(r)).collect();
        let shown = shown(rows, false);
        // Header, a, the trailer counting c, then b.
        assert_eq!(shown.items.len(), 4);
        assert!(matches!(&shown.items[0], Item::Line(Kind::Header, s, _) if s.count == 2));
        assert!(matches!(&shown.items[2], Item::Line(Kind::Trailer, s, _) if s.hidden == 1));
        assert_eq!(shown.row_at(1).map(|r| r.id.as_str()), Some("a"));
        assert!(shown.row_at(2).is_none(), "the trailer");
        assert_eq!(shown.bundle_at(2).map(|b| b.topic.as_str()), Some("Q3"));
        assert_eq!(positions_of(&shown, &keys), [1, 3], "c is hidden");
    }

    #[test]
    fn the_down_arrow_stays_on_while_more_rows_may_hold_the_topic() {
        let rows = vec![in_topic("a", "Q3", true), in_topic("b", "Q3", true)];
        let later = |shown: &Shown| match &shown.items[0] {
            Item::Line(Kind::Header, s, _) => s.later,
            other => panic!("{other:?}"),
        };
        assert!(!later(&shown(rows.clone(), false)));
        assert!(later(&shown(rows, true)));
    }

    #[test]
    fn fifteen_thousand_selected_rows_come_back_in_one_pass() {
        let rows: Vec<Row> = (0..15_000).map(|n| row(&format!("t{n}"))).collect();
        let keys: HashSet<Key> = rows[200..].iter().map(|r| key(r)).collect();
        let found = positions_of(&shown(rows, false), &keys);
        assert_eq!(found.len(), 14_800);
        assert_eq!(found.first(), Some(&200));
    }

    #[test]
    fn the_next_row_opens_after_the_selection_or_else_the_one_before() {
        assert_eq!(neighbour(5, 1, 2), Some(3));
        assert_eq!(neighbour(5, 3, 4), Some(2));
        assert_eq!(neighbour(5, 0, 4), None);
        assert_eq!(neighbour(1, 0, 0), None);
    }

    #[test]
    fn a_search_does_not_repeat_its_query_under_the_title() {
        assert!(!super::subtitle_shows(true, "hike", Some("hike ")));
        assert!(super::subtitle_shows(true, "hike", None));
        assert!(super::subtitle_shows(true, "19 unread", Some("hike")));
        assert!(!super::subtitle_shows(false, "19 unread", None));
        assert!(!super::subtitle_shows(true, "", None));
    }
}
