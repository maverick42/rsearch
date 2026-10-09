//! The pure view layer of the search results: mask filter, sort and
//! the per-file detail line.
//!
//! `report.results` is the untouched source of truth — this layer
//! only computes a separate display order ([`compute_visible_order`])
//! and display strings. It imports nothing from `std::fs`: dates and
//! sizes come from the engine's `FileResult` (the index snapshot,
//! D18), local-timezone rendering reuses the existing
//! [`crate::util`] helpers, and name matching reuses the engine's
//! single mask implementation.

use std::cmp::Ordering;
use std::path::{Path, MAIN_SEPARATOR};

use rsearch_engine::masks::file_name_segment;
use rsearch_engine::{matches_masks, parse_masks, FileResult};

use crate::util;

/// Sort keys of the results view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Full path — the engine's canonical `(file_path, entry_path)`
    /// order; reproduces the historical display exactly.
    #[default]
    Path,
    /// Leaf name, case-insensitive (an archive entry: the entry's
    /// leaf name).
    Name,
    /// Index-time modification time; `None` always last, whatever
    /// the direction.
    Modified,
    /// Number of verified occurrences.
    Occurrences,
    /// Index-time size.
    Size,
    /// Lowercase extension, then name; no extension sorts as `""`.
    Extension,
}

impl SortKey {
    /// All keys in combo order — the index mapping the UI uses.
    pub const ALL: [SortKey; 6] = [
        SortKey::Path,
        SortKey::Name,
        SortKey::Modified,
        SortKey::Occurrences,
        SortKey::Size,
        SortKey::Extension,
    ];

    /// The key at a combo index; out of range falls back to the
    /// default (`Path`).
    pub fn from_index(index: usize) -> SortKey {
        Self::ALL.get(index).copied().unwrap_or_default()
    }

    /// The combo index of a key.
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|k| *k == self).unwrap_or(0)
    }
}

/// How the results are displayed: a name-mask filter plus a sort.
/// Per-tab state; a fresh tab starts at the defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewSpec {
    /// Raw filter text, `""` = no filter. Parsed with the engine's
    /// `parse_masks` — the same grammar as every other mask field.
    pub filter: String,
    pub sort: SortKey,
    pub desc: bool,
}

/// Why a view specification was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// A mask containing a path separator can never match a leaf
    /// name — masks apply to names only, so this is certainly a
    /// mistake. The caller keeps the previous view and signals it.
    InvalidMask(String),
}

/// The indices of `results` to display, in display order.
///
/// The filter matches the leaf name (an archive entry: the entry's
/// leaf name); a filter that parses to no mask keeps everything. The
/// sort is a total deterministic order: key, then container path,
/// then entry path. `desc` reverses the whole order except
/// `mtime: None`, which stays last whatever the direction.
pub fn compute_visible_order(
    results: &[FileResult],
    spec: &ViewSpec,
) -> Result<Vec<usize>, FilterError> {
    let masks = parse_masks(&spec.filter);
    for mask in &masks {
        if mask.contains('/') || mask.contains('\\') {
            return Err(FilterError::InvalidMask(mask.clone()));
        }
    }
    let mut order: Vec<usize> = (0..results.len())
        .filter(|&i| matches_masks(&masks, leaf_name(&results[i])))
        .collect();
    let sort = spec.sort;
    let desc = spec.desc;
    order.sort_by(|&a, &b| compare(&results[a], &results[b], sort, desc));
    Ok(order)
}

/// The detail line of one file row:
/// `name [occurrences] - size - local date - parent directory`.
///
/// The size comes from [`util::format_bytes`] and the date from
/// [`util::format_unix_local`] — the PC's local timezone, the same
/// helper that renders the project database file's date — `—` when
/// no mtime was recorded. Everything comes from the `FileResult`;
/// no filesystem access.
pub fn file_detail_line(r: &FileResult) -> String {
    let date = r
        .mtime
        .map(util::format_unix_local)
        .unwrap_or_else(|| "—".to_owned());
    format!(
        "{} [{}] - {} - {} - {}",
        leaf_name(r),
        r.occurrences.len(),
        util::format_bytes(r.size),
        date,
        parent_display(r),
    )
}

/// The name the filter and the Name sort act on: the file's own name
/// for a regular file, the entry's leaf name for an archive entry.
pub(crate) fn leaf_name(r: &FileResult) -> &str {
    match &r.entry_path {
        Some(entry) => file_name_segment(entry),
        None => r
            .file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(""),
    }
}

/// The parent part displayed after the last `-`: the parent
/// directory with its trailing separator for a regular file,
/// `archive!entry-dir/` for an archive entry (the stored path minus
/// its leaf — the same `file!entry` representation the list already
/// shows). Display only, no filesystem access.
fn parent_display(r: &FileResult) -> String {
    match &r.entry_path {
        None => {
            let mut parent = r
                .file_path
                .parent()
                .unwrap_or(Path::new(""))
                .display()
                .to_string();
            if !parent.is_empty() && !parent.ends_with(MAIN_SEPARATOR) {
                parent.push(MAIN_SEPARATOR);
            }
            parent
        }
        Some(entry) => match entry.rfind(['/', '\\']) {
            Some(pos) => format!("{}!{}", r.file_path.display(), &entry[..=pos]),
            None => r.file_path.display().to_string(),
        },
    }
}

/// Lowercase extension of a leaf name — `""` without one. A leading
/// dot (`.gitignore`) is not an extension, like the engine's own
/// entry-extension rule.
fn extension_of(leaf: &str) -> String {
    match leaf.rfind('.') {
        Some(pos) if pos > 0 => leaf[pos + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// The total display order of two results: primary key, then the
/// engine's canonical path order as the deterministic tiebreak.
fn compare(a: &FileResult, b: &FileResult, sort: SortKey, desc: bool) -> Ordering {
    let dir = |o: Ordering| if desc { o.reverse() } else { o };
    let primary = match sort {
        SortKey::Path => path_cmp(a, b),
        SortKey::Name => name_cmp(a, b),
        SortKey::Modified => {
            // `None` stays last whatever the direction: the rank is
            // never reversed, only the timestamp is.
            let (ra, va) = mtime_rank(a);
            let (rb, vb) = mtime_rank(b);
            return ra
                .cmp(&rb)
                .then_with(|| dir(va.cmp(&vb)))
                .then_with(|| dir(path_cmp(a, b)));
        }
        SortKey::Occurrences => a.occurrences.len().cmp(&b.occurrences.len()),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Extension => extension_of(leaf_name(a)).cmp(&extension_of(leaf_name(b))),
    };
    dir(primary).then_with(|| dir(path_cmp(a, b)))
}

/// The engine's canonical order — exactly what `report.results` is
/// sorted by, so the default view reproduces the historical display.
fn path_cmp(a: &FileResult, b: &FileResult) -> Ordering {
    a.file_path
        .cmp(&b.file_path)
        .then_with(|| a.entry_path.cmp(&b.entry_path))
}

fn name_cmp(a: &FileResult, b: &FileResult) -> Ordering {
    leaf_name(a)
        .to_lowercase()
        .cmp(&leaf_name(b).to_lowercase())
}

/// `(0, mtime)` when a mtime exists, `(1, 0)` otherwise — the rank
/// puts `None` last in both directions.
fn mtime_rank(r: &FileResult) -> (u8, i64) {
    match r.mtime {
        Some(v) => (0, v),
        None => (1, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsearch_engine::Occurrence;
    use std::path::PathBuf;

    fn occ() -> Occurrence {
        Occurrence {
            line: 1,
            column: 1,
            line_text: "x".into(),
            context_before: Vec::new(),
            context_after: Vec::new(),
        }
    }

    fn res(path: &str, size: u64, mtime: Option<i64>, occ_count: usize) -> FileResult {
        FileResult {
            file_path: PathBuf::from(path),
            entry_path: None,
            size,
            mtime,
            occurrences: vec![occ(); occ_count],
        }
    }

    fn entry(container: &str, entry: &str, size: u64, mtime: Option<i64>) -> FileResult {
        FileResult {
            file_path: PathBuf::from(container),
            entry_path: Some(entry.to_string()),
            size,
            mtime,
            occurrences: vec![occ()],
        }
    }

    fn spec(filter: &str, sort: SortKey, desc: bool) -> ViewSpec {
        ViewSpec {
            filter: filter.to_string(),
            sort,
            desc,
        }
    }

    fn names_of(results: &[FileResult], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .map(|&i| leaf_name(&results[i]).to_string())
            .collect()
    }

    // -- Order ------------------------------------------------------------

    #[test]
    fn sort_key_index_round_trips() {
        assert_eq!(SortKey::from_index(0), SortKey::Path);
        assert_eq!(SortKey::from_index(5), SortKey::Extension);
        // Out of range falls back to the default.
        assert_eq!(SortKey::from_index(6), SortKey::Path);
        assert_eq!(SortKey::from_index(usize::MAX), SortKey::Path);
        for (i, key) in SortKey::ALL.iter().enumerate() {
            assert_eq!(key.index(), i);
            assert_eq!(SortKey::from_index(i), *key);
        }
    }

    #[test]
    fn no_filter_path_asc_reproduces_the_current_order() {
        let results = vec![
            res("C:\\a\\b.txt", 1, Some(1), 1),
            res("C:\\a\\c.txt", 1, Some(2), 2),
            res("C:\\d\\a.txt", 1, Some(3), 3),
        ];
        let order = compute_visible_order(&results, &ViewSpec::default()).unwrap();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn each_sort_key_orders_asc_and_desc() {
        let results = vec![
            res("C:\\x\\m.txt", 300, Some(30), 3),
            res("C:\\x\\a.txt", 100, Some(10), 1),
            res("C:\\x\\z.txt", 200, Some(20), 2),
        ];
        // Name asc: a, m, z — desc: z, m, a.
        let asc = compute_visible_order(&results, &spec("", SortKey::Name, false)).unwrap();
        assert_eq!(names_of(&results, &asc), vec!["a.txt", "m.txt", "z.txt"]);
        let desc = compute_visible_order(&results, &spec("", SortKey::Name, true)).unwrap();
        assert_eq!(names_of(&results, &desc), vec!["z.txt", "m.txt", "a.txt"]);

        // Occurrences.
        let asc = compute_visible_order(&results, &spec("", SortKey::Occurrences, false)).unwrap();
        assert_eq!(asc, vec![1, 2, 0]);
        let desc = compute_visible_order(&results, &spec("", SortKey::Occurrences, true)).unwrap();
        assert_eq!(desc, vec![0, 2, 1]);

        // Size.
        let asc = compute_visible_order(&results, &spec("", SortKey::Size, false)).unwrap();
        assert_eq!(asc, vec![1, 2, 0]);
        let desc = compute_visible_order(&results, &spec("", SortKey::Size, true)).unwrap();
        assert_eq!(desc, vec![0, 2, 1]);

        // Modified — all Some here, so desc is the plain reverse.
        let asc = compute_visible_order(&results, &spec("", SortKey::Modified, false)).unwrap();
        assert_eq!(asc, vec![1, 2, 0]);
        let desc = compute_visible_order(&results, &spec("", SortKey::Modified, true)).unwrap();
        assert_eq!(desc, vec![0, 2, 1]);
    }

    #[test]
    fn ties_break_by_container_then_entry_path() {
        // Same leaf name in two containers; same size — the container
        // path decides.
        let results = vec![
            res("C:\\y\\same.txt", 10, Some(1), 1),
            res("C:\\x\\same.txt", 10, Some(2), 1),
        ];
        let asc = compute_visible_order(&results, &spec("", SortKey::Name, false)).unwrap();
        assert_eq!(asc, vec![1, 0], "container C:\\x before C:\\y");
        // Same container, two entries — the entry path decides.
        let results = vec![
            entry("C:\\x\\a.zip", "b.txt", 10, Some(1)),
            entry("C:\\x\\a.zip", "a.txt", 10, Some(1)),
        ];
        let asc = compute_visible_order(&results, &spec("", SortKey::Size, false)).unwrap();
        assert_eq!(asc, vec![1, 0], "entry a.txt before b.txt");
    }

    #[test]
    fn none_mtime_sorts_last_in_both_directions() {
        let results = vec![
            res("C:\\x\\none.txt", 1, None, 1),
            res("C:\\x\\old.txt", 1, Some(10), 1),
            res("C:\\x\\new.txt", 1, Some(20), 1),
        ];
        let asc = compute_visible_order(&results, &spec("", SortKey::Modified, false)).unwrap();
        assert_eq!(asc, vec![1, 2, 0], "oldest first, None last");
        let desc = compute_visible_order(&results, &spec("", SortKey::Modified, true)).unwrap();
        assert_eq!(desc, vec![2, 1, 0], "newest first, None still last");
    }

    #[test]
    fn name_sort_uses_the_leaf_name_of_archive_entries() {
        let results = vec![
            res("C:\\x\\zzz.txt", 1, Some(1), 1),
            entry("C:\\x\\a.zip", "dir/aaa.txt", 1, Some(1)),
        ];
        let asc = compute_visible_order(&results, &spec("", SortKey::Name, false)).unwrap();
        assert_eq!(names_of(&results, &asc), vec!["aaa.txt", "zzz.txt"]);
    }

    #[test]
    fn extension_sort_uses_lowercase_extension_then_name() {
        let results = vec![
            res("C:\\x\\b.ASP", 1, Some(1), 1),
            res("C:\\x\\a.txt", 1, Some(1), 1),
            res("C:\\x\\a.asp", 1, Some(1), 1),
            res("C:\\x\\noext", 1, Some(1), 1),
            res("C:\\x\\.gitignore", 1, Some(1), 1),
        ];
        let asc = compute_visible_order(&results, &spec("", SortKey::Extension, false)).unwrap();
        // "" (.gitignore, noext) then asp (a.asp, b.ASP by name) then txt.
        assert_eq!(asc, vec![4, 3, 2, 0, 1]);
    }

    // -- Filter ------------------------------------------------------------

    #[test]
    fn filter_keeps_matching_leaf_names() {
        let results = vec![
            res("C:\\x\\a1.asp", 1, Some(1), 1),
            res("C:\\x\\a2.txt", 1, Some(1), 1),
            res("C:\\x\\b1.asp", 1, Some(1), 1),
        ];
        let order = compute_visible_order(&results, &spec("a*.asp", SortKey::Path, false)).unwrap();
        assert_eq!(order, vec![0]);
    }

    #[test]
    fn filter_is_case_insensitive() {
        let results = vec![res("C:\\x\\Fiche_Tarif.ASP", 1, Some(1), 1)];
        let order =
            compute_visible_order(&results, &spec("fiche*.asp", SortKey::Path, false)).unwrap();
        assert_eq!(order, vec![0]);
    }

    #[test]
    fn filter_matches_the_leaf_name_of_archive_entries() {
        let results = vec![
            entry("C:\\x\\outer.zip", "docs/a1.asp", 1, Some(1)),
            entry("C:\\x\\outer.zip", "docs/b.txt", 1, Some(1)),
            res("C:\\x\\a9.asp", 1, Some(1), 1),
        ];
        let order = compute_visible_order(&results, &spec("a*.asp", SortKey::Path, false)).unwrap();
        assert_eq!(
            order,
            vec![2, 0],
            "entry leaf and regular file both match, path order"
        );
    }

    #[test]
    fn empty_filter_keeps_everything() {
        let results = vec![
            res("C:\\x\\a.asp", 1, Some(1), 1),
            res("C:\\x\\b.txt", 1, Some(1), 1),
        ];
        for filter in ["", "  ", " ; ; "] {
            let order =
                compute_visible_order(&results, &spec(filter, SortKey::Path, false)).unwrap();
            assert_eq!(order, vec![0, 1], "filter {filter:?} keeps everything");
        }
    }

    #[test]
    fn filter_matching_nothing_gives_an_empty_order_and_untouched_results() {
        let results = vec![res("C:\\x\\a.asp", 1, Some(1), 1)];
        let order = compute_visible_order(&results, &spec("*.zzz", SortKey::Path, false)).unwrap();
        assert!(order.is_empty());
        assert_eq!(results.len(), 1, "the report is never modified");
    }

    #[test]
    fn mask_with_a_path_separator_is_invalid_without_panic() {
        let results = vec![res("C:\\x\\a.asp", 1, Some(1), 1)];
        for filter in ["dir\\a*.asp", "dir/a*.asp"] {
            assert!(matches!(
                compute_visible_order(&results, &spec(filter, SortKey::Path, false)),
                Err(FilterError::InvalidMask(_))
            ));
        }
    }

    // -- Property ------------------------------------------------------------

    #[test]
    fn property_indices_are_valid_unique_and_match_the_filter() {
        // Deterministic LCG — no rand dependency.
        let mut seed = 0x5EED_1234_ABCD_0001u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        let prefixes = ["a", "b", "file"];
        let exts = ["asp", "txt", ""];
        let mut results = Vec::new();
        for i in 0..60 {
            let name = format!(
                "{}{}{}.{}",
                prefixes[(next() % 3) as usize],
                i,
                if next() % 2 == 0 { "x" } else { "" },
                exts[(next() % 3) as usize]
            );
            let mtime = if next() % 4 == 0 {
                None
            } else {
                Some((next() % 1_000) as i64)
            };
            let occ_count = (next() % 5) as usize;
            let r = if next() % 3 == 0 {
                entry(
                    "C:\\z\\arc.zip",
                    &format!("dir/{name}"),
                    next() % 10_000,
                    mtime,
                )
            } else {
                res(&format!("C:\\z\\{name}"), next() % 10_000, mtime, occ_count)
            };
            results.push(r);
        }
        let filters = ["", "a*", "*.asp", "*x*", "file*", "*.zzz"];
        for filter in filters {
            for &sort in &[
                SortKey::Path,
                SortKey::Name,
                SortKey::Modified,
                SortKey::Occurrences,
                SortKey::Size,
                SortKey::Extension,
            ] {
                for &desc in &[false, true] {
                    let s = spec(filter, sort, desc);
                    let order = compute_visible_order(&results, &s).unwrap();
                    // Valid, unique indices.
                    let mut seen = std::collections::HashSet::new();
                    for &i in &order {
                        assert!(i < results.len());
                        assert!(seen.insert(i), "duplicate index {i}");
                    }
                    // Exactly the indices whose leaf name passes the filter.
                    let masks = parse_masks(filter);
                    let expected: Vec<usize> = (0..results.len())
                        .filter(|&i| matches_masks(&masks, leaf_name(&results[i])))
                        .collect();
                    let mut got = order.clone();
                    got.sort_unstable();
                    assert_eq!(got, expected, "filter {filter:?} sort {sort:?} desc {desc}");
                }
            }
        }
    }

    // -- Counts and detail line --------------------------------------------

    #[test]
    fn detail_line_shows_name_occurrences_size_date_parent() {
        let r = res(
            "C:\\test\\aspd\\Site\\Commande_Reception\\fiche_tarif.asp",
            31_979,
            Some(1_727_913_600),
            40,
        );
        let line = file_detail_line(&r);
        // The date segment is exactly the existing local-timezone
        // helper's output — the same mechanism as the project
        // database file's date, never a second implementation.
        let date = util::format_unix_local(1_727_913_600);
        assert_eq!(
            line,
            format!(
                "fiche_tarif.asp [40] - {} - {} - C:\\test\\aspd\\Site\\Commande_Reception\\",
                util::format_bytes(31_979),
                date
            )
        );
        // The date is the local rendering, not a UTC-only hardcode:
        // it equals what the app already displays elsewhere.
        assert!(line.contains(&date));
    }

    #[test]
    fn detail_line_without_mtime_shows_a_dash() {
        let r = res("C:\\x\\a.asp", 512, None, 1);
        let line = file_detail_line(&r);
        assert_eq!(line, "a.asp [1] - 512 B - — - C:\\x\\");
    }

    #[test]
    fn detail_line_sizes_use_the_existing_readable_format() {
        for (bytes, expected) in [
            (512u64, "512 B"),
            (2_048, "2.0 KiB"),
            (16 * 1024 * 1024, "16.0 MiB"),
        ] {
            let r = res("C:\\x\\a.asp", bytes, Some(1), 1);
            assert!(file_detail_line(&r).contains(&format!("- {expected} -")));
        }
    }

    #[test]
    fn detail_line_of_an_archive_entry_shows_the_container_parent() {
        let r = entry("C:\\x\\outer.zip", "docs/a1.asp", 100, Some(1_727_913_600));
        let line = file_detail_line(&r);
        let date = util::format_unix_local(1_727_913_600);
        assert_eq!(
            line,
            format!("a1.asp [1] - 100 B - {date} - C:\\x\\outer.zip!docs/")
        );
        // A dir-less entry falls back to the container alone.
        let r = entry("C:\\x\\outer.zip", "a1.asp", 100, None);
        assert_eq!(
            file_detail_line(&r),
            "a1.asp [1] - 100 B - — - C:\\x\\outer.zip"
        );
    }
}
