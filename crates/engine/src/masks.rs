//! Shared file-name masks: wildcard matching, text parsing and the
//! compiled include/exclude pair.
//!
//! The mask language is deliberately minimal: `*` matches any run of
//! characters (including none), `?` matches exactly one, everything
//! else is literal. Matching is case-insensitive, anchored to the
//! whole string, and applies to file NAMES only — never to full
//! paths. No character classes, no `**`, no braces, no escaping, no
//! path globs; those are out of scope on purpose.
//!
//! The same [`NameMasks`] value drives both levels of filtering:
//!
//! * build time (`BuildOptions::include_masks`/`exclude_masks`) —
//!   the masks define what may enter the index at all;
//! * search time (`SearchOptions::include_masks`/`exclude_masks`) —
//!   the masks can only narrow what the index already contains.
//!
//! The matcher itself knows nothing about documents or archives: it
//! answers `pattern × name -> bool`. The callers decide which names a
//! candidate contributes (a regular file: its own name; an archive
//! entry: its entry name, plus the archive name for exclusion).

/// Splits user-entered mask text into individual masks.
///
/// `;` and newlines separate items, each item is trimmed, empty items
/// are dropped and the order is preserved. A comma is an ordinary
/// mask character — `report,*.csv` is one mask, never two.
pub fn parse_masks(text: &str) -> Vec<String> {
    text.split([';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// The last segment of a path-like string: the only part masks ever
/// see. Works for filesystem paths, raw archive entry names
/// (`dir/Foo.java` → `Foo.java`) and stored entry paths
/// (`inner.zip!/dir/x.xml` → `x.xml`).
pub fn file_name_segment(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or("")
}

/// One pattern, lowercased and pre-split into characters so matching a
/// name never re-parses the pattern. `*` and `?` keep their wildcard
/// meaning; there is no way to escape them.
#[derive(Debug, Clone)]
struct Mask {
    chars: Vec<char>,
}

impl Mask {
    fn new(pattern: &str) -> Self {
        Mask {
            chars: pattern.to_lowercase().chars().collect(),
        }
    }

    /// Whole-string match against an already-lowercased name.
    fn matches(&self, name: &[char]) -> bool {
        wildcard_match_chars(&self.chars, name)
    }
}

/// Case-insensitive whole-string wildcard match: `*` (any run, possibly
/// empty), `?` (exactly one character), everything else literal.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let folded: Vec<char> = name.to_lowercase().chars().collect();
    Mask::new(pattern).matches(&folded)
}

/// Whether `name` passes a parsed mask list — the include-side rule:
/// an empty list accepts everything, otherwise at least one mask must
/// match. Case-insensitive, whole-name, the same mask language as
/// every other mask field. The GUI's results filter uses this so the
/// language keeps exactly one implementation.
pub fn matches_masks(masks: &[String], name: &str) -> bool {
    if masks.is_empty() {
        return true;
    }
    masks.iter().any(|m| wildcard_match(m, name))
}

/// Greedy two-pointer match over characters: the last `*` seen is a
/// backtrack point, so the name is walked once and nothing is
/// allocated. `pat` and `s` are already lowercase-folded.
fn wildcard_match_chars(pat: &[char], s: &[char]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while t < s.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == s[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = p;
            mark = t;
            p += 1;
        } else if star != usize::MAX {
            // The last `*` swallows one more character and the match
            // resumes right after it.
            mark += 1;
            t = mark;
            p = star + 1;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// A compiled include + exclude mask pair for file names.
///
/// Semantics for a candidate name:
///
/// ```text
/// (include empty OR at least one include mask matches)
/// AND (no exclude mask matches)
/// ```
///
/// Exclusion always wins over inclusion.
#[derive(Debug, Clone, Default)]
pub struct NameMasks {
    include: Vec<Mask>,
    exclude: Vec<Mask>,
}

impl NameMasks {
    /// Compiles the raw mask lists. Order inside each list does not
    /// matter for the result; parsing keeps the user's order anyway.
    pub fn new(include: &[String], exclude: &[String]) -> Self {
        NameMasks {
            include: include.iter().map(|m| Mask::new(m)).collect(),
            exclude: exclude.iter().map(|m| Mask::new(m)).collect(),
        }
    }

    /// Whether the exclude side rejects `name`.
    pub fn excluded(&self, name: &str) -> bool {
        let folded: Vec<char> = name.to_lowercase().chars().collect();
        self.exclude.iter().any(|m| m.matches(&folded))
    }

    /// Whether the include side accepts `name` — an empty include list
    /// accepts everything.
    pub fn included(&self, name: &str) -> bool {
        if self.include.is_empty() {
            return true;
        }
        let folded: Vec<char> = name.to_lowercase().chars().collect();
        self.include.iter().any(|m| m.matches(&folded))
    }

    /// Whether `name` passes the whole filter (include and exclude).
    ///
    /// This is the rule for a regular file name at build time and for
    /// an archive entry name (the entry name alone decides — the
    /// archive's own name never satisfies the include side).
    pub fn accepts_file(&self, name: &str) -> bool {
        self.included(name) && !self.excluded(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Matcher ------------------------------------------------------

    #[test]
    fn suffix_star_matches_any_extension_case_insensitively() {
        assert!(wildcard_match("*.java", "Foo.java"));
        assert!(wildcard_match("*.java", "foo.JAVA"));
        assert!(wildcard_match("*.java", "Foo.Java"));
        assert!(!wildcard_match("*.java", "Foo.java~"));
        assert!(!wildcard_match("*.java", "Foojav"));
        // A bare extension is not enough: masks match the whole name.
        assert!(!wildcard_match("*.java", "java"));
    }

    #[test]
    fn prefix_star_matches() {
        assert!(wildcard_match("Test*.java", "TestFoo.java"));
        assert!(wildcard_match("Test*.java", "testfoo.java"));
        assert!(wildcard_match("Test*.java", "Test.java"));
        assert!(!wildcard_match("Test*.java", "FooTest.java"));
    }

    #[test]
    fn question_mark_is_exactly_one_character() {
        assert!(wildcard_match("foo?.java", "foo1.java"));
        assert!(!wildcard_match("foo?.java", "foo12.java"));
        assert!(!wildcard_match("foo?.java", "foo.java"));
        assert!(wildcard_match("foo?.java", "foo..java"));
        assert!(wildcard_match("???", "abc"));
        assert!(!wildcard_match("???", "ab"));
        assert!(!wildcard_match("???", "abcd"));
    }

    #[test]
    fn lone_star_matches_anything_including_empty() {
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*", "日本語.md"));
    }

    #[test]
    fn literal_mask_is_exact_and_case_insensitive() {
        assert!(wildcard_match("foo", "foo"));
        assert!(wildcard_match("foo", "FOO"));
        assert!(!wildcard_match("foo", "foobar"));
        assert!(!wildcard_match("foo", "bar"));
    }

    #[test]
    fn substring_mask_matches_inside_the_name() {
        assert!(wildcard_match("*foo*", "xxfooyy"));
        assert!(wildcard_match("*generated*", "X_Generated_Y.java"));
        assert!(!wildcard_match("*foo*", "fobar"));
    }

    #[test]
    fn multiple_stars() {
        assert!(wildcard_match("*a*b*", "xxaybz"));
        assert!(wildcard_match("a*b*c", "abbc"));
        assert!(wildcard_match("**", "anything"));
        assert!(wildcard_match("*.*", "pom."));
        assert!(!wildcard_match("*a*b*", "xbz"));
        // Classic backtracking case: the first `*` must give back
        // characters so the tail still matches.
        assert!(wildcard_match("*bc", "abcbc"));
        assert!(wildcard_match("a*b*c", "aXXbYYbZc"));
    }

    #[test]
    fn comma_is_an_ordinary_mask_character() {
        assert!(wildcard_match("report,*.csv", "report,123.csv"));
        assert!(!wildcard_match("report,*.csv", "report,123.txt"));
        assert!(wildcard_match("a,b", "A,B"));
    }

    #[test]
    fn ordinary_special_characters_are_literal() {
        assert!(wildcard_match("a+b(c)", "a+b(c)"));
        assert!(wildcard_match("[x].y", "[x].y"));
        assert!(wildcard_match("pom.*", "pom.xml"));
        assert!(!wildcard_match("pom.*", "pomx"));
    }

    #[test]
    fn empty_pattern_matches_only_empty_name() {
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("", "x"));
    }

    #[test]
    fn reasonable_unicode() {
        assert!(wildcard_match("*.md", "日本語.md"));
        assert!(wildcard_match("données*", "DONNÉES.txt"));
        assert!(wildcard_match("café?.txt", "café1.txt"));
        assert!(!wildcard_match("café?.txt", "café12.txt"));
        // Lowercase folding of a capital accented name.
        assert!(wildcard_match("*été*", "prÉTÉ"));
    }

    // -- matches_masks ---------------------------------------------------

    #[test]
    fn matches_masks_is_the_include_side_rule() {
        let masks = parse_masks("*.java;Test*");
        assert!(matches_masks(&masks, "Foo.java"));
        assert!(matches_masks(&masks, "testfoo.kt"));
        assert!(!matches_masks(&masks, "Foo.txt"));
        // An empty list accepts everything — the filter is inactive.
        assert!(matches_masks(&[], "anything.asp"));
    }

    // -- parse_masks ---------------------------------------------------

    #[test]
    fn parse_splits_on_semicolons_and_newlines() {
        assert_eq!(parse_masks("*.rs;*.toml"), vec!["*.rs", "*.toml"]);
        assert_eq!(parse_masks("*.rs\n*.toml"), vec!["*.rs", "*.toml"]);
        assert_eq!(
            parse_masks("*.rs; *.toml\n *.md "),
            vec!["*.rs", "*.toml", "*.md"]
        );
        assert_eq!(parse_masks("  ; \n ; "), Vec::<String>::new());
        assert_eq!(parse_masks(""), Vec::<String>::new());
    }

    #[test]
    fn parse_never_treats_commas_as_separators() {
        assert_eq!(parse_masks("*.csv,*.bak"), vec!["*.csv,*.bak"]);
        assert_eq!(
            parse_masks("report,*.csv;*.rs"),
            vec!["report,*.csv", "*.rs"]
        );
    }

    #[test]
    fn parse_preserves_order_and_trims() {
        assert_eq!(
            parse_masks("  Test*.java  ;*generated*;\n foo?.rs"),
            vec!["Test*.java", "*generated*", "foo?.rs"]
        );
    }

    #[test]
    fn file_name_segment_takes_the_last_component() {
        assert_eq!(file_name_segment("dir/Foo.java"), "Foo.java");
        assert_eq!(file_name_segment("dir\\sub\\Foo.java"), "Foo.java");
        assert_eq!(file_name_segment("Foo.java"), "Foo.java");
        assert_eq!(file_name_segment("inner.zip!/dir/x.xml"), "x.xml");
        assert_eq!(file_name_segment("inner.zip!/x.xml"), "x.xml");
        assert_eq!(file_name_segment(""), "");
    }

    // -- NameMasks ------------------------------------------------------

    #[test]
    fn empty_masks_accept_everything() {
        let m = NameMasks::new(&[], &[]);
        assert!(m.accepts_file("anything.txt"));
        assert!(m.accepts_file(""));
    }

    #[test]
    fn include_only_keeps_matching_names() {
        let m = NameMasks::new(&["*.java".to_string()], &[]);
        assert!(m.accepts_file("Foo.java"));
        assert!(m.accepts_file("foo.JAVA"));
        assert!(!m.accepts_file("Foo.txt"));
    }

    #[test]
    fn exclude_only_drops_matching_names() {
        let m = NameMasks::new(&[], &["*.log".to_string()]);
        assert!(m.accepts_file("Foo.java"));
        assert!(!m.accepts_file("trace.LOG"));
    }

    #[test]
    fn exclusion_wins_over_inclusion() {
        let m = NameMasks::new(&["*.java".to_string()], &["Test*.java".to_string()]);
        assert!(m.accepts_file("Foo.java"));
        assert!(!m.accepts_file("TestFoo.java"));
        assert!(!m.accepts_file("testfoo.java"));
    }

    #[test]
    fn multiple_include_and_exclude_masks() {
        let m = NameMasks::new(
            &["*.java".to_string(), "*.kt".to_string()],
            &["Test*.java".to_string(), "*Generated*".to_string()],
        );
        assert!(m.accepts_file("Foo.java"));
        assert!(m.accepts_file("Bar.kt"));
        assert!(!m.accepts_file("TestFoo.java"));
        assert!(!m.accepts_file("KtGenerated.kt"));
        assert!(!m.accepts_file("Foo.txt"));
    }
}
