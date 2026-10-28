//! The splices that turn an old list into a new one, so a list model can
//! update in place and keep its selection and scroll position: one splice
//! over everything that differs, or the few that change only what did.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice {
    pub position: u32,
    pub removed: u32,
    /// The replacement items, as a range of the new list.
    pub added: Range<usize>,
}

/// `None` when the lists are equal. Otherwise keeps the longest common
/// prefix and suffix and replaces what lies between.
pub fn splice<T: PartialEq>(old: &[T], new: &[T]) -> Option<Splice> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    Some(Splice {
        position: prefix as u32,
        removed: (old.len() - prefix - suffix) as u32,
        added: prefix..new.len() - suffix,
    })
}

/// The most cells of the comparison table [`edits`] fills before it
/// settles for one splice: two stretches of five hundred items that
/// differ.
const MOST_CELLS: usize = 250_000;

/// The splices that turn `old` into `new` while leaving every item the
/// two share in order where it is, last first: applied in that order,
/// each splice's position still names its place in the list as it then
/// stands. An item that moved is one removal and one insertion, and the
/// items it passed are not touched, which matters to a list view that
/// holds its scroll position on one of them. Past [`MOST_CELLS`] it
/// falls back to the one [`splice`] over the stretch that differs.
pub fn edits<T: PartialEq>(old: &[T], new: &[T]) -> Vec<Splice> {
    let Some(whole) = splice(old, new) else {
        return Vec::new();
    };
    let start = whole.position as usize;
    let (a, b) = (
        &old[start..start + whole.removed as usize],
        &new[whole.added.clone()],
    );
    let (m, n) = (a.len(), b.len());
    if m == 0 || n == 0 || (m + 1) * (n + 1) > MOST_CELLS {
        return vec![whole];
    }
    // `common[i][j]`: how many items `a[i..]` and `b[j..]` share in order.
    let width = n + 1;
    let mut common = vec![0u32; (m + 1) * width];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            common[i * width + j] = match a[i] == b[j] {
                true => common[(i + 1) * width + j + 1] + 1,
                false => common[(i + 1) * width + j].max(common[i * width + j + 1]),
            };
        }
    }
    let at = |i: usize, j: usize| common[i * width + j];
    let mut splices = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m || j < n {
        if i < m && j < n && a[i] == b[j] && at(i, j) == at(i + 1, j + 1) + 1 {
            i += 1;
            j += 1;
            continue;
        }
        let (from, to) = (i, j);
        while i < m || j < n {
            if i < m && j < n && a[i] == b[j] && at(i, j) == at(i + 1, j + 1) + 1 {
                break;
            }
            if j < n && (i == m || at(i, j + 1) >= at(i + 1, j)) {
                j += 1;
            } else {
                i += 1;
            }
        }
        splices.push(Splice {
            position: (start + from) as u32,
            removed: (i - from) as u32,
            added: whole.added.start + to..whole.added.start + j,
        });
    }
    splices.reverse();
    splices
}

#[cfg(test)]
mod tests {
    use super::{Splice, edits, splice};

    /// `old` with `edits` applied in the order given.
    fn applied(old: &[i32], new: &[i32]) -> Vec<i32> {
        let mut list = old.to_vec();
        for edit in edits(old, new) {
            let at = edit.position as usize;
            list.splice(
                at..at + edit.removed as usize,
                new[edit.added].iter().copied(),
            );
        }
        list
    }

    #[test]
    fn edits_rebuild_the_new_list() {
        for (old, new) in [
            (vec![1, 2, 3, 4, 5], vec![9, 5, 2, 3, 4]),
            (vec![1, 2, 3], vec![3, 2, 1]),
            (vec![], vec![1, 2]),
            (vec![1, 2], vec![]),
            (vec![1, 1, 2, 1], vec![1, 2, 1, 1]),
            (vec![1, 2, 3], vec![1, 2, 3]),
        ] {
            assert_eq!(applied(&old, &new), new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn a_row_moved_up_leaves_the_rows_it_passed_alone() {
        // A row at the end joins a bundle at the top: the header changes,
        // the row goes in under it, and 2, 3 and 4 stay put.
        let (old, new) = ([10, 2, 3, 4, 5], [11, 5, 2, 3, 4]);
        assert_eq!(
            edits(&old, &new),
            [
                Splice {
                    position: 4,
                    removed: 1,
                    added: 5..5
                },
                Splice {
                    position: 0,
                    removed: 1,
                    added: 0..2
                },
            ]
        );
    }

    #[test]
    fn equal_lists_need_nothing() {
        assert_eq!(splice(&[1, 2, 3], &[1, 2, 3]), None);
        assert_eq!(splice::<i32>(&[], &[]), None);
    }

    #[test]
    fn new_mail_at_the_top_is_one_insert() {
        assert_eq!(
            splice(&[2, 3], &[1, 2, 3]),
            Some(Splice {
                position: 0,
                removed: 0,
                added: 0..1
            })
        );
    }

    #[test]
    fn a_removed_row_is_one_removal() {
        assert_eq!(
            splice(&[1, 2, 3], &[1, 3]),
            Some(Splice {
                position: 1,
                removed: 1,
                added: 1..1
            })
        );
    }

    #[test]
    fn a_changed_row_is_replaced_alone() {
        assert_eq!(
            splice(&[1, 2, 3], &[1, 9, 3]),
            Some(Splice {
                position: 1,
                removed: 1,
                added: 1..2
            })
        );
    }

    #[test]
    fn repeated_values_do_not_overlap_prefix_and_suffix() {
        assert_eq!(
            splice(&[1, 1], &[1]),
            Some(Splice {
                position: 1,
                removed: 1,
                added: 1..1
            })
        );
        assert_eq!(
            splice(&[1], &[1, 1]),
            Some(Splice {
                position: 1,
                removed: 0,
                added: 1..2
            })
        );
    }

    #[test]
    fn unrelated_lists_are_replaced_whole() {
        assert_eq!(
            splice(&[1, 2], &[3, 4, 5]),
            Some(Splice {
                position: 0,
                removed: 2,
                added: 0..3
            })
        );
    }
}
