//! Bundles: the rows of one topic that sit near each other in the
//! list, drawn together under a header. Nothing here touches GTK; the list
//! hands over its rows and draws the entries this lays out.
//!
//! The list keeps its order, newest first. Walking down it, a row in a
//! topic gathers every later row of that topic that comes within
//! [`WINDOW`] rows of the last one gathered, so a long run of the topic
//! keeps growing. A bundle sits where its newest row was; the rows it
//! pulled up leave their places, and the rows between keep their order. A
//! row of a topic with no other row of it near is an ordinary row.
//!
//! A bundle shows only its unread rows until it is opened, with a trailer
//! counting the rest. One whose rows are all read shows a summary of its
//! senders and subjects instead. A row the list keeps on screen, such as
//! the selected one, stays visible, so opening an unread row does not
//! pull it out from under the reader once it is read.

use std::collections::HashSet;

use mailrs_domain::{ThreadSummary, topic};

use super::{Key, Row, key};

/// How many rows past the last row of a bundle the next row of its topic
/// may come and still join it.
pub const WINDOW: usize = 20;

/// How many colours bundles take, by the hash of their topic's name: the
/// account colours, `account-0` and on in the stylesheet.
pub const COLORS: usize = crate::format::PALETTE.len();

/// One line of the laid-out list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A list row, by its index into the rows, and the bundle it sits in.
    Row { row: usize, bundle: Option<usize> },
    /// A bundle's header: its topic, how many rows it holds, the arrows.
    Header(usize),
    /// The line under an opened bundle, or under a closed one hiding read
    /// rows, which opens or closes it.
    Trailer(usize),
    /// A closed bundle whose rows are all hidden: its senders and subjects
    /// in one line, which opens it.
    Summary(usize),
}

/// One bundle of rows of a topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// The topic's name, as the newest row spells it.
    pub topic: String,
    /// The rows, by index, in list order.
    pub members: Vec<usize>,
    pub open: bool,
    /// How many rows a closed bundle hides; 0 while it is open.
    pub hidden: usize,
    /// Where the header is among the entries.
    pub at: usize,
    /// The entry the up and down arrows go to: the nearest place above
    /// and below where the topic shows again, outside this bundle.
    pub earlier: Option<usize>,
    pub later: Option<usize>,
}

/// The rows laid out as entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub entries: Vec<Entry>,
    pub bundles: Vec<Bundle>,
}

impl Layout {
    /// The index of `row` among the entries, if it shows.
    pub fn entry_of_row(&self, row: usize) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| matches!(e, Entry::Row { row: r, .. } if *r == row))
    }

    /// The bundle that hides `row`, which has to open for it to show.
    pub fn hiding(&self, row: usize) -> Option<&Bundle> {
        self.bundles
            .iter()
            .find(|b| b.members.contains(&row) && self.entry_of_row(row).is_none())
    }

    /// The row an entry shows, if it shows one.
    pub fn row_at(&self, entry: usize) -> Option<usize> {
        match self.entries.get(entry)? {
            Entry::Row { row, .. } => Some(*row),
            _ => None,
        }
    }
}

/// Which bundles the person opened, remembered by topic and row, so a
/// bundle stays open when a new row joins it or the list reloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Opened(HashSet<(String, Key)>);

impl Opened {
    fn holds(&self, topic: &str, rows: &[Row], members: &[usize]) -> bool {
        let topic = topic.to_lowercase();
        members
            .iter()
            .any(|m| self.0.contains(&(topic.clone(), key(&rows[*m]))))
    }

    /// Opens or closes the bundle of `topic` holding `members`. Closing
    /// is the person asking for the unread rows alone, so the read rows
    /// the bundle kept on screen leave `kept` and hide too.
    pub fn toggle(&mut self, topic: &str, rows: &[Row], members: &[usize], kept: &mut HashSet<Key>) {
        let open = self.holds(topic, rows, members);
        if open {
            for member in members {
                kept.remove(&key(&rows[*member]));
            }
        }
        let topic = topic.to_lowercase();
        for member in members {
            let entry = (topic.clone(), key(&rows[*member]));
            match open {
                true => self.0.remove(&entry),
                false => self.0.insert(entry),
            };
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// Lays `rows` out in bundles. A row whose key is in `kept` shows even in
/// a closed bundle.
pub fn lay_out(rows: &[Row], window: usize, opened: &Opened, kept: &HashSet<Key>) -> Layout {
    let topics: Vec<Vec<&String>> = rows.iter().map(|r| sorted_topics(r)).collect();
    let mut placed = vec![false; rows.len()];
    let mut layout = Layout::default();
    for start in 0..rows.len() {
        if placed[start] {
            continue;
        }
        placed[start] = true;
        let gathered = topics[start].iter().find_map(|topic| {
            let members = gather(&topics, &placed, start, topic, window);
            (members.len() > 1).then_some((topic, members))
        });
        let Some((topic, members)) = gathered else {
            layout.entries.push(Entry::Row {
                row: start,
                bundle: None,
            });
            continue;
        };
        for member in &members {
            placed[*member] = true;
        }
        let open = opened.holds(topic, rows, &members);
        let index = layout.bundles.len();
        let at = layout.entries.len();
        layout.entries.push(Entry::Header(index));
        let shows = |m: &usize| open || rows[*m].unread || kept.contains(&key(&rows[*m]));
        let shown: Vec<usize> = members.iter().copied().filter(shows).collect();
        let hidden = members.len() - shown.len();
        if shown.is_empty() {
            layout.entries.push(Entry::Summary(index));
        } else {
            layout.entries.extend(shown.into_iter().map(|row| Entry::Row {
                row,
                bundle: Some(index),
            }));
            if open || hidden > 0 {
                layout.entries.push(Entry::Trailer(index));
            }
        }
        layout.bundles.push(Bundle {
            topic: (*topic).clone(),
            members,
            open,
            hidden,
            at,
            earlier: None,
            later: None,
        });
    }
    link_occurrences(rows, &mut layout);
    layout
}

/// `rows` rows laid out as they come, with no bundles.
pub fn flat(rows: usize) -> Layout {
    Layout {
        entries: (0..rows).map(|row| Entry::Row { row, bundle: None }).collect(),
        bundles: Vec::new(),
    }
}

/// A row's topics in a fixed order, so a row in two topics tries them the
/// same way on every device.
fn sorted_topics(row: &ThreadSummary) -> Vec<&String> {
    let mut topics: Vec<&String> = row.topics.iter().collect();
    topics.sort_by_key(|g| g.to_lowercase());
    topics
}

/// `start` and the rows after it that join it in `topic`: each unplaced
/// row of the topic within `window` rows of the last one gathered.
fn gather(
    topics: &[Vec<&String>],
    placed: &[bool],
    start: usize,
    topic: &str,
    window: usize,
) -> Vec<usize> {
    let mut members = vec![start];
    let mut next = start + 1;
    while next < topics.len() && next <= members[members.len() - 1] + window {
        let joins = !placed[next] && topics[next].iter().any(|g| topic::same(g, topic));
        if joins {
            members.push(next);
        }
        next += 1;
    }
    members
}

/// Fills in where each bundle's arrows go: the nearest entry above and
/// below where its topic shows again, outside the bundle. A row of the
/// topic hidden inside another bundle shows through that bundle's header.
fn link_occurrences(rows: &[Row], layout: &mut Layout) {
    let place_of_row = |row: usize| -> Option<usize> {
        layout.entry_of_row(row).or_else(|| {
            layout
                .bundles
                .iter()
                .find(|b| b.members.contains(&row))
                .map(|b| b.at)
        })
    };
    let mut links = Vec::with_capacity(layout.bundles.len());
    for bundle in &layout.bundles {
        let mut places: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(index, row)| {
                !bundle.members.contains(index)
                    && row.topics.iter().any(|g| topic::same(g, &bundle.topic))
            })
            .filter_map(|(index, _)| place_of_row(index))
            .collect();
        places.sort_unstable();
        places.dedup();
        let earlier = places.iter().rev().find(|p| **p < bundle.at).copied();
        let later = places.iter().find(|p| **p > bundle.at).copied();
        links.push((earlier, later));
    }
    for (bundle, (earlier, later)) in layout.bundles.iter_mut().zip(links) {
        bundle.earlier = earlier;
        bundle.later = later;
    }
}

/// The topics the rows name, newest first and each once: what a menu
/// offers as the topics in use.
pub fn recent(rows: &[Row], most: usize) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for row in rows {
        for topic in sorted_topics(row) {
            if names.len() == most {
                return names;
            }
            if !names.iter().any(|n| topic::same(n, topic)) {
                names.push(topic.clone());
            }
        }
    }
    names
}

/// Which of the [`COLORS`] a topic takes: the same on every device, since
/// it comes from the name alone.
pub fn color_of(topic: &str) -> usize {
    // FNV-1a, whose result no Rust release changes.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in topic.to_lowercase().bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash as usize % COLORS
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    /// Rows named by letter, each with its topics and whether it is
    /// unread: `("a", &["Q3"], true)`.
    fn rows(spec: &[(&str, &[&str], bool)]) -> Vec<Row> {
        spec.iter()
            .map(|(id, topics, unread)| {
                Rc::new(ThreadSummary {
                    account_id: 1,
                    id: id.to_string(),
                    unread: *unread,
                    topics: topics.iter().map(|g| g.to_string()).collect(),
                    ..ThreadSummary::default()
                })
            })
            .collect()
    }

    /// The layout in words: a row by its id, a header as `[topic n]`, a
    /// trailer as `+hidden`, a summary as `(summary)`.
    fn words(rows: &[Row], layout: &Layout) -> Vec<String> {
        layout
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Row { row, .. } => rows[*row].id.clone(),
                Entry::Header(b) => {
                    let bundle = &layout.bundles[*b];
                    format!("[{} {}]", bundle.topic, bundle.members.len())
                }
                Entry::Trailer(b) => format!("+{}", layout.bundles[*b].hidden),
                Entry::Summary(_) => "(summary)".into(),
            })
            .collect()
    }

    fn plain(rows: &[Row], window: usize) -> Vec<String> {
        words(rows, &lay_out(rows, window, &Opened::default(), &HashSet::new()))
    }

    #[test]
    fn rows_of_a_topic_near_each_other_gather_under_the_newest() {
        let list = rows(&[
            ("a", &["Q3"], true),
            ("b", &[], true),
            ("c", &["Q3"], true),
            ("d", &[], true),
        ]);
        assert_eq!(plain(&list, WINDOW), ["[Q3 2]", "a", "c", "b", "d"]);
    }

    #[test]
    fn a_row_too_far_from_the_rest_of_its_topic_stands_alone() {
        let list = rows(&[
            ("a", &["Q3"], true),
            ("b", &[], true),
            ("c", &[], true),
            ("d", &["Q3"], true),
        ]);
        assert_eq!(plain(&list, 2), ["a", "b", "c", "d"]);
        assert_eq!(plain(&list, 3), ["[Q3 2]", "a", "d", "b", "c"]);
    }

    #[test]
    fn the_window_counts_from_the_last_row_gathered() {
        let list = rows(&[
            ("a", &["Q3"], true),
            ("b", &[], true),
            ("c", &["Q3"], true),
            ("d", &[], true),
            ("e", &["Q3"], true),
        ]);
        assert_eq!(plain(&list, 2), ["[Q3 3]", "a", "c", "e", "b", "d"]);
    }

    #[test]
    fn a_closed_bundle_shows_its_unread_rows_and_counts_the_rest() {
        let list = rows(&[
            ("a", &["Inv"], true),
            ("b", &["Inv"], false),
            ("c", &["Inv"], true),
        ]);
        assert_eq!(plain(&list, WINDOW), ["[Inv 3]", "a", "c", "+1"]);
    }

    #[test]
    fn a_closed_bundle_of_read_rows_shows_a_summary() {
        let list = rows(&[("a", &["Q3"], false), ("b", &["Q3"], false)]);
        assert_eq!(plain(&list, WINDOW), ["[Q3 2]", "(summary)"]);
    }

    #[test]
    fn opening_shows_every_row_in_list_order_and_stays_open_as_rows_join() {
        let list = rows(&[
            ("a", &["Q3"], false),
            ("b", &["Q3"], true),
            ("c", &["Q3"], false),
        ]);
        let mut opened = Opened::default();
        let mut none = HashSet::new();
        opened.toggle("Q3", &list, &[0, 1, 2], &mut none);
        assert_eq!(
            words(&list, &lay_out(&list, WINDOW, &opened, &none)),
            ["[Q3 3]", "a", "b", "c", "+0"]
        );
        let grown = rows(&[
            ("new", &["q3"], false),
            ("a", &["Q3"], false),
            ("b", &["Q3"], true),
            ("c", &["Q3"], false),
        ]);
        assert_eq!(
            words(&grown, &lay_out(&grown, WINDOW, &opened, &none)),
            ["[q3 4]", "new", "a", "b", "c", "+0"]
        );
        opened.toggle("q3", &grown, &[0, 1, 2, 3], &mut none);
        assert_eq!(
            words(&grown, &lay_out(&grown, WINDOW, &opened, &none)),
            ["[q3 4]", "b", "+3"]
        );
    }

    #[test]
    fn a_kept_row_stays_in_a_closed_bundle_once_read() {
        let list = rows(&[("a", &["Q3"], false), ("b", &["Q3"], true)]);
        let kept = HashSet::from([key(&list[0])]);
        assert_eq!(
            words(&list, &lay_out(&list, WINDOW, &Opened::default(), &kept)),
            ["[Q3 2]", "a", "b"]
        );
    }

    #[test]
    fn closing_a_bundle_hides_the_read_rows_it_kept() {
        let list = rows(&[("a", &["Q3"], true), ("b", &["Q3"], false)]);
        let mut opened = Opened::default();
        let mut kept = HashSet::new();
        opened.toggle("Q3", &list, &[0, 1], &mut kept);
        // The reader opens b while the bundle is open, which keeps it.
        kept.insert(key(&list[1]));
        let other = rows(&[("z", &[], false)]);
        kept.insert(key(&other[0]));
        opened.toggle("Q3", &list, &[0, 1], &mut kept);
        assert_eq!(
            words(&list, &lay_out(&list, WINDOW, &opened, &kept)),
            ["[Q3 2]", "a", "+1"]
        );
        assert!(kept.contains(&key(&other[0])), "rows outside the bundle stay kept");
    }

    #[test]
    fn a_row_in_two_topics_joins_the_first_that_gathers_it() {
        let list = rows(&[
            ("a", &["Zed", "Alpha"], true),
            ("b", &["Zed"], true),
            ("c", &["Alpha"], true),
        ]);
        assert_eq!(plain(&list, WINDOW), ["[Alpha 2]", "a", "c", "b"]);
        let alone = rows(&[("a", &["Zed", "Alpha"], true), ("b", &["Zed"], true)]);
        assert_eq!(plain(&alone, WINDOW), ["[Zed 2]", "a", "b"]);
    }

    #[test]
    fn the_arrows_reach_the_topic_where_it_shows_again() {
        let list = rows(&[
            ("a", &["Q3"], true),
            ("b", &["Q3"], true),
            ("c", &[], true),
            ("d", &[], true),
            ("e", &["Q3"], true),
            ("f", &[], true),
            ("g", &[], true),
            ("h", &["Q3"], true),
            ("i", &["Q3"], true),
        ]);
        let layout = lay_out(&list, 1, &Opened::default(), &HashSet::new());
        assert_eq!(
            words(&list, &layout),
            ["[Q3 2]", "a", "b", "c", "d", "e", "f", "g", "[Q3 2]", "h", "i"]
        );
        let (first, last) = (&layout.bundles[0], &layout.bundles[1]);
        assert_eq!((first.earlier, first.later), (None, Some(5)), "the lone row");
        assert_eq!((last.earlier, last.later), (Some(5), None));
    }

    #[test]
    fn an_arrow_to_a_hidden_row_lands_on_the_bundle_that_hides_it() {
        let list = rows(&[
            ("a", &["Q3", "Inv"], false),
            ("b", &["Inv"], false),
            ("c", &[], true),
            ("d", &["Q3"], true),
            ("e", &["Q3"], true),
        ]);
        let layout = lay_out(&list, 1, &Opened::default(), &HashSet::new());
        assert_eq!(
            words(&list, &layout),
            ["[Inv 2]", "(summary)", "c", "[Q3 2]", "d", "e"]
        );
        assert_eq!(layout.bundles[1].earlier, Some(0));
        assert_eq!(layout.hiding(0).map(|b| b.topic.as_str()), Some("Inv"));
        assert_eq!(layout.hiding(3), None);
    }

    #[test]
    fn recent_topics_come_newest_first_each_once() {
        let list = rows(&[
            ("a", &["Q3"], true),
            ("b", &["Inv"], true),
            ("c", &["q3", "Web"], true),
        ]);
        assert_eq!(recent(&list, 5), ["Q3", "Inv", "Web"]);
        assert_eq!(recent(&list, 2), ["Q3", "Inv"]);
    }

    #[test]
    fn a_topic_keeps_its_colour_whatever_the_case() {
        assert_eq!(color_of("Q3 Launch"), color_of("q3 launch"));
        assert!(color_of("Invoices") < COLORS);
        let spread: HashSet<usize> = ["Q3", "Inv", "Web", "Hiring", "Taxes", "Trip"]
            .iter()
            .map(|g| color_of(g))
            .collect();
        assert!(spread.len() > 2, "{spread:?}");
    }
}
