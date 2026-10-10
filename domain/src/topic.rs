//! Topics: names a person gives a piece of work, which the thread list
//! draws as bundles of nearby rows. A message joins a topic as a keyword
//! on a server that stores keywords, and as a label on Gmail.
//!
//! An IMAP keyword is an ASCII atom without spaces, compared without case,
//! so the keyword spells the name in lower-case letters and digits and
//! writes every other byte of its UTF-8 as `=` and two lower-case hex
//! digits: "Foo bar" is `topic:=46oo=20bar`. The spelling survives a
//! server that changes the case of keywords, and every device decodes the
//! same name from it. A Gmail label takes any text, so it is the name
//! itself after the prefix: `topic:Foo bar`.

/// What every topic keyword and topic label starts with.
pub const PREFIX: &str = "topic:";

/// The entry in a list of stored keywords that stands for every topic
/// keyword, since a server that takes `\*` stores any of them.
pub const FAMILY: &str = "topic:*";

/// The longest name a topic may have, in characters. Escaped, the longest
/// keyword stays well under the line lengths IMAP servers take.
pub const MAX_LEN: usize = 64;

/// The name as a topic may carry it: trimmed, with runs of white space
/// made one space. `None` for an empty name, one with a control
/// character, or one longer than [`MAX_LEN`].
pub fn clean(name: &str) -> Option<String> {
    let cleaned = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let fits = !cleaned.is_empty()
        && cleaned.chars().count() <= MAX_LEN
        && !cleaned.chars().any(char::is_control);
    fits.then_some(cleaned)
}

/// Whether two names stand for the same topic. Topics match ignoring
/// case, as Gmail's labels do, so "Q3 launch" joins "Q3 Launch".
pub fn same(one: &str, other: &str) -> bool {
    one.to_lowercase() == other.to_lowercase()
}

/// The keyword that puts a message in the topic `name`.
pub fn keyword_of(name: &str) -> String {
    let mut keyword = String::from(PREFIX);
    for byte in name.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => keyword.push(byte as char),
            _ => keyword.push_str(&format!("={byte:02x}")),
        }
    }
    keyword
}

/// The topic a keyword puts its message in. `None` for a keyword of
/// another kind, or one that is not a whole escape of a UTF-8 name.
pub fn name_of_keyword(keyword: &str) -> Option<String> {
    let escaped = keyword.get(..PREFIX.len())?;
    if !escaped.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let mut bytes = Vec::new();
    let mut rest = keyword[PREFIX.len()..].bytes();
    while let Some(byte) = rest.next() {
        match byte.to_ascii_lowercase() {
            b'=' => {
                let high = hex(rest.next()?)?;
                let low = hex(rest.next()?)?;
                bytes.push(high << 4 | low);
            }
            plain @ (b'a'..=b'z' | b'0'..=b'9') => bytes.push(plain),
            _ => return None,
        }
    }
    let name = String::from_utf8(bytes).ok()?;
    clean(&name).filter(|cleaned| *cleaned == name)
}

/// The label that puts a message in the topic `name` on Gmail.
pub fn label_of(name: &str) -> String {
    format!("{PREFIX}{name}")
}

/// The topic a label stands for, from the label's name.
pub fn name_of_label(label: &str) -> Option<&str> {
    let name = label.strip_prefix(PREFIX)?;
    (!name.trim().is_empty()).then_some(name)
}

fn hex(digit: u8) -> Option<u8> {
    (digit as char).to_digit(16).map(|d| d as u8)
}

/// Whether a server whose stored keywords are `stored` keeps `keyword`:
/// it is among them, or it is a topic keyword and they hold [`FAMILY`].
pub fn stored(stored: &[&str], keyword: &str) -> bool {
    stored.contains(&keyword)
        || (stored.contains(&FAMILY) && name_of_keyword(keyword).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_survives_the_keyword_whatever_the_server_does_to_case() {
        for name in ["Foo bar", "foo bar", "Café", "Invoice #2041", "Q3 Launch", "日本"] {
            let keyword = keyword_of(name);
            assert!(keyword.is_ascii() && !keyword.contains(' '), "{keyword}");
            assert_eq!(name_of_keyword(&keyword).as_deref(), Some(name));
            assert_eq!(
                name_of_keyword(&keyword.to_ascii_uppercase()).as_deref(),
                Some(name),
                "a server that raises the case keeps the name"
            );
        }
        assert_eq!(keyword_of("Foo bar"), "topic:=46oo=20bar");
        assert_eq!(keyword_of("Café"), "topic:=43af=c3=a9");
    }

    #[test]
    fn the_keyword_is_an_imap_atom() {
        // RFC 9051's atom-specials, and `%` and `*`, which are wildcards.
        let keyword = keyword_of("a (b) {c} %d* \"e\" \\f ]g");
        assert!(!keyword.chars().any(|c| "(){ %*\"\\]".contains(c)), "{keyword}");
    }

    #[test]
    fn other_keywords_and_broken_escapes_name_no_topic() {
        for keyword in ["$seen", "topic:", "topic:=4", "topic:=zz", "topic:a b", "topic:=ff", "topica"] {
            assert_eq!(name_of_keyword(keyword), None, "{keyword}");
        }
        assert_eq!(name_of_keyword("topic:=20a"), None, "a padded name is not clean");
    }

    #[test]
    fn a_label_carries_the_name_as_it_is() {
        assert_eq!(label_of("Foo bar"), "topic:Foo bar");
        assert_eq!(name_of_label("topic:Foo bar"), Some("Foo bar"));
        assert_eq!(name_of_label("Receipts"), None);
        assert_eq!(name_of_label("topic: "), None);
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(clean("  Q3   launch "), Some("Q3 launch".into()));
        assert_eq!(clean("   "), None);
        assert_eq!(clean("a\u{7}b"), None);
        assert_eq!(clean(&"x".repeat(MAX_LEN)), Some("x".repeat(MAX_LEN)));
        assert_eq!(clean(&"x".repeat(MAX_LEN + 1)), None);
    }

    #[test]
    fn topics_match_ignoring_case() {
        assert!(same("Q3 Launch", "q3 launch"));
        assert!(same("CAFÉ", "café"));
        assert!(!same("Q3", "Q4"));
    }

    #[test]
    fn a_server_that_takes_any_keyword_stores_every_topic() {
        let open = ["$seen", FAMILY];
        assert!(stored(&open, "$seen"));
        assert!(stored(&open, &keyword_of("Foo")));
        assert!(!stored(&open, "$junk"));
        assert!(!stored(&["$seen"], &keyword_of("Foo")));
    }
}
