//! Shared text matcher for every picker. Fields cache grapheme normalization and
//! word boundaries; parsed queries and reusable scratch keep keystroke work small.
//! Callers supply field priorities and own presentation and row tie breakers.
use nucleo_matcher::chars::{normalize, to_lower_case};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

/// Match quality, ordered best first and independent of any field priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MatchClass {
    Exact,
    Prefix,
    Contiguous,
    /// Gaps stay within one word or begin runs at word starts.
    NonContiguous,
}

/// A parsed, case-insensitive fzf query. Empty needles are removed by the parser,
/// so every exposed atom can safely enter the alignment algorithms.
#[derive(Debug)]
pub(crate) struct Query {
    atoms: Vec<Atom>,
}
impl Query {
    pub fn parse(text: &str) -> Self {
        let query: String = text.trim().nfc().collect();
        let atoms = Pattern::parse(&query, CaseMatching::Ignore, Normalization::Smart)
            .atoms
            .into_iter()
            .map(|atom| {
                let needle: Vec<_> = atom.needle_text().chars().collect();
                let normalize = needle.iter().all(|&c| normalize(c) == c);
                Atom {
                    kind: atom.kind,
                    negative: atom.negative,
                    needle,
                    normalize,
                }
            })
            .collect();
        Self { atoms }
    }

    pub fn atoms(&self) -> &[Atom] {
        &self.atoms
    }
}

/// A borrowed prepared field and caller-defined priority (lower sorts first).
/// Indices in match results refer to this input slice, never to sorted fields.
#[derive(Debug)]
pub(crate) struct Field<'a, P> {
    pub haystack: &'a Haystack,
    pub priority: P,
}

/// One accepted field alignment, linked back to the caller's field slice.
#[derive(Debug)]
pub(crate) struct FieldMatch {
    pub field_index: usize,
    pub matched: TextMatch,
}

/// All accepted fields for a positive atom, plus its winning field. Keeping all
/// alignments lets each picker explain matches without running a second search.
#[derive(Debug)]
pub(crate) struct AtomMatches {
    pub atom_index: usize,
    best_index: usize,
    fields: Vec<FieldMatch>,
}
impl AtomMatches {
    pub fn best(&self) -> &FieldMatch {
        &self.fields[self.best_index]
    }
    pub fn into_fields(self) -> Vec<FieldMatch> {
        self.fields
    }
}

/// Aggregate relevance for one slice of fields. Negative atoms only exclude;
/// they contribute neither scores nor indices. Empty queries succeed with no atoms.
#[derive(Debug)]
pub(crate) struct QueryMatches {
    pub worst_class: MatchClass,
    pub total_score: i32,
    pub atoms: Vec<AtomMatches>,
}

/// Reusable alignment workspace. One instance can search every field of every
/// row; immutable queries and haystacks contain no per-search scratch state.
#[derive(Default)]
pub(crate) struct Matcher {
    scratch: Vec<Step>,
}
impl Matcher {
    /// Find a positive occurrence of an atom, even when it is marked negative.
    /// Use match_fields to apply whole-query negation across all fields.
    pub fn match_atom(&mut self, atom: &Atom, haystack: &Haystack) -> Option<TextMatch> {
        haystack.match_atom(atom, &mut self.scratch)
    }

    /// Choose each positive atom's best (class, priority, -score) field. Every
    /// positive atom must match, and any occurrence of a negative atom rejects
    /// the entire slice. Input order resolves otherwise identical field ties.
    pub fn match_fields<P: Ord>(
        &mut self,
        query: &Query,
        fields: &[Field<'_, P>],
    ) -> Option<QueryMatches> {
        let mut result = QueryMatches {
            worst_class: MatchClass::Exact,
            total_score: 0,
            atoms: Vec::new(),
        };
        for (atom_index, atom) in query.atoms().iter().enumerate() {
            if atom.is_negative() {
                if fields
                    .iter()
                    .any(|field| self.match_atom(atom, field.haystack).is_some())
                {
                    return None;
                }
                continue;
            }
            let matches: Vec<_> = fields
                .iter()
                .enumerate()
                .filter_map(|(field_index, field)| {
                    self.match_atom(atom, field.haystack)
                        .map(|matched| FieldMatch {
                            field_index,
                            matched,
                        })
                })
                .collect();
            let (best_index, best) = matches.iter().enumerate().min_by_key(|(_, found)| {
                (
                    found.matched.class,
                    &fields[found.field_index].priority,
                    std::cmp::Reverse(found.matched.score),
                )
            })?;
            result.worst_class = result.worst_class.max(best.matched.class);
            result.total_score += best.matched.score;
            result.atoms.push(AtomMatches {
                atom_index,
                best_index,
                fields: matches,
            });
        }
        Some(result)
    }
}

/// Prepared once when a field changes, not once per keystroke. Every position is
/// one displayed grapheme, including decomposed accents; folding never expands
/// that position, matching Nucleo's character normalization convention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Haystack {
    text: String,
    folded: Vec<char>,
    normalized: Vec<char>,
    starts: Vec<bool>,
    words: Vec<usize>,
}

impl Haystack {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let chars: Vec<char> = text
            .graphemes(true)
            .filter_map(|g| g.nfc().next())
            .collect();
        let mut starts = Vec::with_capacity(chars.len());
        let mut words = Vec::with_capacity(chars.len());
        let mut word = 0;
        for (i, &c) in chars.iter().enumerate() {
            let start = i == 0
                || separator(chars[i - 1])
                || (c.is_uppercase()
                    && (chars[i - 1].is_lowercase()
                        || (chars[i - 1].is_uppercase()
                            && chars.get(i + 1).is_some_and(|c| c.is_lowercase()))));
            let start = start && !separator(c);
            word += usize::from(start);
            starts.push(start);
            words.push(if separator(c) { usize::MAX } else { word });
        }
        Self {
            text,
            folded: chars.iter().copied().map(to_lower_case).collect(),
            normalized: chars
                .iter()
                .copied()
                .map(|c| to_lower_case(normalize(c)))
                .collect(),
            starts,
            words,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn match_atom(&self, atom: &Atom, scratch: &mut Vec<Step>) -> Option<TextMatch> {
        let hay = if atom.normalize {
            &self.normalized
        } else {
            &self.folded
        };
        let needle = &atom.needle;
        if needle.len() > hay.len() {
            return None;
        }
        let mut best: Option<TextMatch> = None;
        // Inspect every substring, because Nucleo's highest-scoring fuzzy
        // alignment can be illegal even when a valid contiguous match exists.
        for (start, window) in hay.windows(needle.len()).enumerate() {
            let end = start + needle.len();
            let allowed = match atom.kind {
                AtomKind::Fuzzy | AtomKind::Substring => true,
                AtomKind::Prefix => start == 0,
                AtomKind::Postfix => end == hay.len(),
                AtomKind::Exact => start == 0 && end == hay.len(),
                _ => false,
            };
            if !allowed || window != needle {
                continue;
            }
            let class = if needle.len() == hay.len() {
                MatchClass::Exact
            } else if start == 0 || self.starts[start] {
                MatchClass::Prefix
            } else {
                MatchClass::Contiguous
            };
            let score = (start..end)
                .map(|i| self.reward(i) + CONSECUTIVE_BONUS * i32::from(i > start))
                .sum();
            if best
                .as_ref()
                .is_none_or(|best| (class, std::cmp::Reverse(score)) < best.key())
            {
                best = Some(TextMatch {
                    class,
                    score,
                    indices: (start as u32..end as u32).collect(),
                });
            }
        }
        if best.is_some() || atom.kind != AtomKind::Fuzzy {
            return best;
        }
        // Most fields fail this cheap subsequence check; only plausible fuzzy
        // matches need the bounded dynamic-programming scratch buffer.
        let mut matched = 0;
        for c in hay {
            if *c == needle[matched] {
                matched += 1;
                if matched == needle.len() {
                    break;
                }
            }
        }
        if matched != needle.len() {
            return None;
        }
        for mode in [Alignment::WithinWord, Alignment::WordStarts] {
            if let Some(found) = self.align(hay, needle, mode, scratch) {
                if best.as_ref().is_none_or(|best| found.score > best.score) {
                    best = Some(found);
                }
            }
        }
        best
    }

    fn reward(&self, index: usize) -> i32 {
        MATCH_SCORE + WORD_START_BONUS * i32::from(self.starts[index])
    }

    /// Each cell ends at one matching character. Gaps either stay in the same
    /// word, or start a new run at a word start. Keeping predecessors finds an
    /// accepted alignment even when greedy matching would choose a dead end.
    /// A running best predecessor makes this O(haystack × atom), not quadratic
    /// in the field length. The caller reuses the allocation across all fields.
    fn align(
        &self,
        hay: &[char],
        needle: &[char],
        mode: Alignment,
        scratch: &mut Vec<Step>,
    ) -> Option<TextMatch> {
        let width = hay.len();
        scratch.clear();
        scratch.resize(width * needle.len(), Step::default());
        for (i, &wanted) in needle.iter().enumerate() {
            let mut gap: Option<(i32, usize)> = None;
            for (j, &c) in hay.iter().enumerate() {
                let same_word =
                    j > 0 && self.words[j] != usize::MAX && self.words[j] == self.words[j - 1];
                if mode == Alignment::WithinWord && !same_word {
                    gap = None;
                }
                if i > 0 && j > 0 && (mode == Alignment::WordStarts || same_word) {
                    let previous = scratch[(i - 1) * width + j - 1];
                    if previous.score != i32::MIN {
                        let adjusted = previous.score + j as i32 - 1;
                        if gap.is_none_or(|(score, _)| adjusted > score) {
                            gap = Some((adjusted, j - 1));
                        }
                    }
                }
                if c != wanted || (mode == Alignment::WithinWord && self.words[j] == usize::MAX) {
                    continue;
                }
                let reward = self.reward(j);
                let mut step = Step::default();
                if i == 0 {
                    if mode == Alignment::WithinWord || self.starts[j] {
                        step.score = reward;
                    }
                } else {
                    if j > 0 && (mode == Alignment::WordStarts || same_word) {
                        let previous = scratch[(i - 1) * width + j - 1];
                        if previous.score != i32::MIN {
                            step = Step {
                                score: previous.score + reward + CONSECUTIVE_BONUS,
                                previous: j - 1,
                            };
                        }
                    }
                    if mode == Alignment::WithinWord || self.starts[j] {
                        if let Some((score, previous)) = gap {
                            let score = score - j as i32 + 1 + reward;
                            if score > step.score {
                                step = Step { score, previous };
                            }
                        }
                    }
                }
                scratch[i * width + j] = step;
            }
        }
        let last = (needle.len() - 1) * width;
        let mut end = None;
        for j in 0..width {
            let score = scratch[last + j].score;
            if score != i32::MIN && end.is_none_or(|(_, best)| score > best) {
                end = Some((j, score));
            }
        }
        let (mut j, score) = end?;
        let mut indices = vec![0; needle.len()];
        for i in (0..needle.len()).rev() {
            indices[i] = j as u32;
            j = scratch[i * width + j].previous;
        }
        Some(TextMatch {
            class: MatchClass::NonContiguous,
            score,
            indices,
        })
    }
}

fn separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '/' | '-' | '_' | '.' | '|' | ':' | '@' | '›')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Alignment {
    WithinWord,
    WordStarts,
}

/// One DP cell's score and predecessor. MIN means unreachable, not a bad match.
#[derive(Clone, Copy)]
struct Step {
    score: i32,
    previous: usize,
}
impl Default for Step {
    fn default() -> Self {
        Self {
            score: i32::MIN,
            previous: usize::MAX,
        }
    }
}

/// Parsed by Nucleo so quoting, anchors, escaping and negation share its syntax.
/// Smart normalization is disabled for an atom containing an accented needle.
#[derive(Debug)]
pub(crate) struct Atom {
    kind: AtomKind,
    negative: bool,
    needle: Vec<char>,
    normalize: bool,
}

// Scores compare only accepted alignments within a class/priority. They reward
// letters, word starts and consecutive runs, with one point lost per skipped
// grapheme. Using the same weights in substring search and DP avoids ranking
// a rejected Nucleo alignment whose highlights the user cannot actually see.
const MATCH_SCORE: i32 = 16;
const WORD_START_BONUS: i32 = 8;
const CONSECUTIVE_BONUS: i32 = 4;

/// The one accepted alignment used for both quality and displayed match marks.
#[derive(Debug)]
pub(crate) struct TextMatch {
    pub class: MatchClass,
    pub score: i32,
    pub indices: Vec<u32>,
}

impl Atom {
    pub fn is_negative(&self) -> bool {
        self.negative
    }
}

impl TextMatch {
    fn key(&self) -> (MatchClass, std::cmp::Reverse<i32>) {
        (self.class, std::cmp::Reverse(self.score))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str, query: &str) -> Option<TextMatch> {
        let query = Query::parse(query);
        assert_eq!(query.atoms().len(), 1);
        Matcher::default().match_atom(&query.atoms()[0], &Haystack::new(text))
    }

    #[test]
    fn classes_and_grapheme_positions_follow_the_match_rule() {
        for (text, query, class, indices) in [
            ("VIM", "vim", MatchClass::Exact, vec![0, 1, 2]),
            ("vim plugin", "vim", MatchClass::Prefix, vec![0, 1, 2]),
            ("open vim", "vim", MatchClass::Prefix, vec![5, 6, 7]),
            ("nvim", "vim", MatchClass::Contiguous, vec![1, 2, 3]),
            ("vaxim", "vim", MatchClass::NonContiguous, vec![0, 3, 4]),
            (
                "Review ferry kill mode",
                "rfkm",
                MatchClass::NonContiguous,
                vec![0, 7, 13, 18],
            ),
            (
                "Review ferry kill mode",
                "revfer",
                MatchClass::NonContiguous,
                vec![0, 1, 2, 7, 8, 9],
            ),
        ] {
            let found = one(text, query).unwrap();
            assert_eq!(found.class, class, "{text} / {query}");
            assert_eq!(found.indices, indices, "{text} / {query}");
        }
        assert!(MatchClass::Exact < MatchClass::Prefix);
        assert!(MatchClass::Prefix < MatchClass::Contiguous);
        assert!(MatchClass::Contiguous < MatchClass::NonContiguous);
        for text in [
            "Review ferry kill mode",
            "jev-like-model",
            "novel item model",
        ] {
            assert!(one(text, "vim").is_none(), "{text}");
        }
    }

    #[test]
    fn boundaries_include_whitespace_separators_and_camel_humps() {
        for separator in [
            " ", "\t", "\n", "\u{2003}", "/", "-", "_", ".", "|", ":", "@", "›", "",
        ] {
            let text = format!("Review{separator}Ferry{separator}Kill{separator}Mode");
            assert!(one(&text, "rfkm").is_some(), "{text:?}");
        }
        assert_eq!(one("readXMLParser", "rxp").unwrap().indices, [0, 4, 7]);
    }

    #[test]
    fn fallback_finds_legal_alignments_instead_of_rejecting_a_greedy_choice() {
        assert_eq!(one("Ax ab Cd", "abcd").unwrap().indices, [3, 4, 6, 7]);
        let found = one("Review ferry kill mode avim", "vim").unwrap();
        assert_eq!(found.class, MatchClass::Contiguous);
        assert_eq!(found.indices, [24, 25, 26]);
        let found = one("Review ferry kill mode vaxim", "vim").unwrap();
        assert_eq!(found.class, MatchClass::NonContiguous);
        assert_eq!(found.indices, [23, 26, 27]);
    }

    #[test]
    fn unicode_normalization_preserves_original_grapheme_offsets() {
        let text = Haystack::new("cafe\u{301} review");
        assert_eq!(text.text(), "cafe\u{301} review");
        for needle in ["cafe", "café", "CAFE\u{301}"] {
            assert_eq!(one(text.text(), needle).unwrap().indices, [0, 1, 2, 3]);
        }
        assert_eq!(
            one(text.text(), "review").unwrap().indices,
            [5, 6, 7, 8, 9, 10]
        );
        assert_eq!(one("日本語 Café", "日本").unwrap().indices, [0, 1]);
        assert_eq!(
            one("👩‍💻 review", "review").unwrap().indices,
            [2, 3, 4, 5, 6, 7]
        );
        assert!(one("e", "é").is_none());
        assert!(one("é", "e").is_some());
    }

    #[test]
    fn parsed_operators_control_matching_and_negation_has_slice_scope() {
        assert!(one("victim", "'vim").is_none());
        assert!(one("nvim", "^vim").is_none());
        assert!(one("vim plugin", "^vim").is_some());
        assert!(one("nvim", "vim$").is_some());
        assert!(one("vim plugin", "vim$").is_none());
        assert!(one("nvim", "^vim$").is_none());
        assert!(one("VIM", "^vim$").is_some());
        for (text, query) in [
            ("!yes", "\\!yes"),
            ("^hat", "\\^hat"),
            ("cash$", "cash\\$"),
            ("'quote", "\\'quote"),
            ("hello world", "hello\\ world"),
        ] {
            assert!(one(text, query).is_some(), "{query}");
        }
        let fields = [Haystack::new("vim"), Haystack::new("excluded")];
        let fields = fields
            .iter()
            .map(|haystack| Field {
                haystack,
                priority: 0,
            })
            .collect::<Vec<_>>();
        let mut matcher = Matcher::default();
        assert!(matcher
            .match_fields(&Query::parse("vim !excluded"), &fields)
            .is_none());
        let matched = matcher
            .match_fields(&Query::parse("vim !missing"), &fields)
            .unwrap();
        assert_eq!(matched.atoms.len(), 1);
        assert_eq!(matched.atoms[0].best().field_index, 0);
    }

    #[test]
    fn best_fields_aggregate_quality_and_preserve_atom_indices_past_negation() {
        let text = [Haystack::new("ab"), Haystack::new("cxd")];
        let fields = text
            .iter()
            .map(|haystack| Field {
                haystack,
                priority: 0,
            })
            .collect::<Vec<_>>();
        let matched = Matcher::default()
            .match_fields(&Query::parse("ab !nope cd"), &fields)
            .unwrap();
        assert_eq!(matched.worst_class, MatchClass::NonContiguous);
        // ab = 24 + 20; c_d = 24 + 16 - 1 skipped character.
        assert_eq!(matched.total_score, 83);
        assert_eq!(
            matched
                .atoms
                .iter()
                .map(|a| (a.atom_index, a.best().field_index))
                .collect::<Vec<_>>(),
            [(0, 0), (2, 1)]
        );
        assert!(Matcher::default()
            .match_fields(&Query::parse("ab missing"), &fields)
            .is_none());
    }

    #[test]
    fn caller_priority_breaks_class_ties_before_score_and_field_order_is_stable() {
        let text = [Haystack::new("fooBar"), Haystack::new("foobar")];
        let query = Query::parse("foobar");
        for (priorities, expected) in [([1, 0], 1), ([0, 0], 0)] {
            let fields = text
                .iter()
                .zip(priorities)
                .map(|(haystack, priority)| Field { haystack, priority })
                .collect::<Vec<_>>();
            let matched = Matcher::default().match_fields(&query, &fields).unwrap();
            assert_eq!(matched.atoms[0].best().field_index, expected);
            assert_eq!(
                matched
                    .atoms
                    .into_iter()
                    .next()
                    .unwrap()
                    .into_fields()
                    .iter()
                    .map(|f| f.field_index)
                    .collect::<Vec<_>>(),
                [0, 1]
            );
        }
        let text = [Haystack::new("avim"), Haystack::new("vim extra")];
        let fields = [
            Field {
                haystack: &text[0],
                priority: 0,
            },
            Field {
                haystack: &text[1],
                priority: 99,
            },
        ];
        let matched = Matcher::default()
            .match_fields(&Query::parse("vim"), &fields)
            .unwrap();
        assert_eq!(matched.atoms[0].best().field_index, 1);
    }

    #[test]
    fn empty_queries_and_empty_fields_have_defined_results() {
        let mut matcher = Matcher::default();
        let fields: [Field<'_, u8>; 0] = [];
        for query in ["", "   ", "!missing"] {
            let matched = matcher.match_fields(&Query::parse(query), &fields).unwrap();
            assert_eq!(matched.worst_class, MatchClass::Exact);
            assert_eq!(matched.total_score, 0);
            assert!(matched.atoms.is_empty());
        }
        assert!(matcher
            .match_fields(&Query::parse("vim"), &fields)
            .is_none());
        assert!(one("", "vim").is_none());
    }
}
