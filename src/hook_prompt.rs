//! UserPromptSubmit retrieval: which indexed passages, if any, are worth adding
//! to the model's context for one prompt. Lexical, local, deterministic and
//! inspectable (`workmap hook UserPromptSubmit --explain`). Silence beats noise.
//!
//! 1. Prompt words are STOP (dropped), WEAK (generic intent words such as
//!    read/project/context/without, or words in more than 5% of all indexed
//!    passages: fix, run, code) or ANCHOR (the rest). Anchors in more than 10%
//!    of this project's passages (`nix` in a NixOS repo) are *common*: they add
//!    little weight and never qualify a passage alone. Only anchors make a
//!    passage relevant; a weak word counts only inside an adjacent prompt
//!    phrase with a distinctive anchor ("context injection"). A word this project never indexed is an
//!    anchor nothing can match: it stays in the denominator, so dropping it
//!    can never make the rest of the prompt look better covered.
//! 2. A passage qualifies with a matched phrase, with ≥2 anchors close to each
//!    other (one of them distinctive) covering at least half of the prompt's
//!    anchor weight (IDF), or
//!    with the prompt's only anchor when that word is identifier-shaped
//!    (snake_case, CamelCase, a sha); a lone ordinary word never qualifies. Text repeated in a few conversations (one brief
//!    given to several agents) is kept once; see 4.
//! 3. A message whose matched words sit in text that recurs verbatim in at
//!    least [`TEMPLATE_CONVS`] conversations (prompt templates, approval JSON,
//!    tool notices) is harness boilerplate and skipped, as is a message that is
//!    one JSON object (approval verdicts, tool payloads).
//! 4. Identical (case/whitespace-normalized, same speaker), near-identical
//!    (word-set Jaccard ≥ [`NEAR`]) and echoed (shared 8-word window) passages
//!    collapse into the first; at most one passage per conversation or doc.
//! 5. Typos: an unknown word of ≥5 letters may complete to the clearly most
//!    frequent indexed word it is a prefix of (≤2 more letters, `contex→context`),
//!    shown in `matched:`. No synonyms, no translation, no edit distance.

use crate::config::Env;
use crate::hook::{self, meta, repo_sessions, Ctx};
use crate::overview::{nodes, rel, Node};
use crate::search;
use crate::util::{clip, fmt_date, sh_quote};
use crate::view::{self, Project};
use rusqlite::{params, Connection};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Function words that carry no topic; dropped before matching.
pub const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "are",
    "but",
    "not",
    "you",
    "your",
    "all",
    "any",
    "can",
    "had",
    "her",
    "was",
    "one",
    "our",
    "out",
    "has",
    "have",
    "him",
    "his",
    "how",
    "its",
    "may",
    "new",
    "now",
    "old",
    "see",
    "two",
    "who",
    "did",
    "get",
    "got",
    "let",
    "say",
    "she",
    "too",
    "use",
    "via",
    "with",
    "this",
    "that",
    "from",
    "they",
    "them",
    "then",
    "than",
    "there",
    "these",
    "those",
    "what",
    "when",
    "where",
    "which",
    "while",
    "why",
    "will",
    "would",
    "could",
    "should",
    "into",
    "onto",
    "about",
    "after",
    "before",
    "again",
    "also",
    "just",
    "like",
    "make",
    "made",
    "more",
    "most",
    "much",
    "must",
    "need",
    "only",
    "other",
    "over",
    "same",
    "some",
    "such",
    "very",
    "were",
    "been",
    "being",
    "does",
    "doing",
    "done",
    "each",
    "here",
    "it's",
    "i'm",
    "please",
    "want",
    "know",
    "think",
    "look",
    "check",
    "find",
    "show",
    "tell",
    "give",
    "take",
    "help",
    "work",
    "thing",
    "things",
    "something",
    "anything",
    "everything",
    "really",
    "maybe",
    "still",
    "yet",
    "ok",
    "okay",
    "yes",
    "lets",
];

/// Generic intent / meta-conversation words: they may match, never anchor.
pub const WEAK: &[&str] = &[
    "read",
    "reading",
    "reads",
    "write",
    "writing",
    "project",
    "projects",
    "context",
    "without",
    "additional",
    "dont",
    "don",
    "didnt",
    "doesnt",
    "isnt",
    "already",
    "nothing",
    "explain",
    "describe",
    "summary",
    "summarize",
    "summarise",
    "conversation",
    "chat",
    "question",
    "answer",
    "info",
    "information",
];

/// A word in more than this share (percent) of the project's passages is weak...
pub const GENERIC_PCT: i64 = 10;
/// ...as is one in more than this share of all indexed passages (fix, run, code).
pub const GLOBAL_PCT: i64 = 5;
/// ...but never below this many passages (small projects).
pub const GENERIC_MIN_DF: i64 = 25;
/// Minimum share of the prompt's anchor weight a passage must cover.
pub const COVERAGE: f64 = 0.5;
/// Conversations sharing an 8-word window before it counts as a template.
pub const TEMPLATE_CONVS: i64 = 5;
/// Word-set Jaccard similarity at which two passages are near-identical.
pub const NEAR: f64 = 0.8;
/// A word in at most this many passages may be a typo of a longer word...
const RARE_TYPO: i64 = 3;
/// ...that is at least this many times more frequent in the project.
const TYPO_RATIO: i64 = 20;
/// Anchors further apart than this many words do not support each other.
const NEARBY: usize = 24;
/// Window length (words) for template and echo checks.
const WIN: usize = 8;
/// Matches this deep into a message are not covered by its opening.
const HEAD: usize = 64;
/// Candidates fetched from the full-text index.
const CANDIDATES: usize = 40;
/// Most passages offered for one prompt.
const MAX_ITEMS: usize = 6;

pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

fn is_stop(w: &str) -> bool {
    w.chars().count() < 3 || STOP.contains(&w) || (w.chars().all(|c| c.is_ascii_digit()) && w.len() < 4)
}

/// Topic terms of a prompt: distinct, no stop-words, at least 3 chars, at most 12.
pub fn prompt_terms(prompt: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for w in words(prompt) {
        if is_stop(&w) {
            continue;
        }
        if seen.insert(w.clone()) {
            out.push(w);
        }
        if out.len() >= 12 {
            break;
        }
    }
    out
}

/// Path-like tokens (`ledger/webhooks.py`, `charges.py`) named in a prompt.
fn prompt_paths(prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in prompt.split_whitespace() {
        let t = t.trim_matches(|c: char| "`'\"()[]{},;:!?<>".contains(c)).trim_end_matches('.');
        let t = t.trim_start_matches("./");
        let looks = t.len() >= 4
            && t.len() <= 200
            && (t.contains('/') || t.rsplit_once('.').map(|(a, b)| !a.is_empty() && (1..=5).contains(&b.len()) && b.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or(false));
        if looks && !t.contains("://") && !t.starts_with('/') && !out.iter().any(|o: &String| o == t) {
            out.push(t.to_string());
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Weak,
    Anchor,
}

/// One prompt term after classification.
#[derive(Clone, Debug)]
pub struct Term {
    /// As typed (lowercased).
    pub typed: String,
    /// What is matched: the typed word or its typo completion.
    pub form: String,
    pub class: Class,
    /// Passages of this project containing `form`.
    pub df: i64,
    /// Identifier-shaped in the prompt (snake_case, CamelCase, sha).
    pub ident: bool,
    /// An anchor frequent in this project: low weight, never enough alone.
    pub common: bool,
    /// Why it is weak, or how it was completed (for `--explain`).
    pub note: String,
}

impl Term {
    fn shown(&self) -> String {
        if self.form == self.typed {
            self.typed.clone()
        } else {
            format!("{}→{}", self.typed, self.form)
        }
    }
}

/// The analysed prompt: terms, adjacent term pairs and project statistics.
pub struct Analysis {
    pub terms: Vec<Term>,
    /// Adjacent prompt terms (indices into `terms`) with at least one known anchor.
    pub pairs: Vec<(usize, usize)>,
    /// Passages (doc/message/commit) of the project.
    pub n: i64,
}

impl Analysis {
    fn weight(&self, t: &Term) -> f64 {
        ((self.n as f64 + 1.0) / (t.df as f64 + 0.5)).ln().max(0.1)
    }
    fn anchors(&self) -> impl Iterator<Item = &Term> {
        self.terms.iter().filter(|t| t.class == Class::Anchor)
    }
    /// A known anchor that is not common in the project.
    fn distinctive(&self, i: usize) -> bool {
        let t = &self.terms[i];
        t.class == Class::Anchor && !t.common && t.df > 0
    }
    /// Known anchors and phrases, as the text of an any-word full-text query.
    fn query(&self) -> String {
        let mut q: Vec<String> = (0..self.terms.len()).filter(|i| self.distinctive(*i)).map(|i| self.terms[i].form.clone()).collect();
        for (a, b) in &self.pairs {
            q.push(format!("\"{} {}\"", self.terms[*a].form, self.terms[*b].form));
        }
        q.join(" ")
    }
}

fn count_df(c: &Connection, repo: &str, form: &str) -> i64 {
    let Some(m) = search::fts_query(form, false) else { return 0 };
    c.query_row(
        "SELECT count(*) FROM passages p WHERE p.repo_id=?1 AND p.kind IN ('doc','message','commit') AND p.id IN (SELECT rowid FROM passages_fts WHERE passages_fts MATCH ?2)",
        params![repo, m],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Indexed passages (any project) containing `w`, from the full-text vocabulary.
fn global_df(c: &Connection, w: &str) -> Option<i64> {
    vocab(c).ok()?;
    c.query_row("SELECT doc FROM temp.workmap_vocab WHERE term=?1", [w], |r| r.get(0)).ok()
}

fn vocab(c: &Connection) -> Result<(), String> {
    c.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS temp.workmap_vocab USING fts5vocab(main, 'passages_fts', 'row');").map_err(|e| e.to_string())
}

/// Prefix completion of an unknown word against the whole index vocabulary:
/// the most frequent indexed word that extends it by 1–2 letters, unless a
/// second candidate is nearly as frequent (then it is ambiguous: none).
fn complete(c: &Connection, w: &str) -> Result<Option<String>, String> {
    if w.len() < 5 || !w.chars().all(|c| c.is_ascii_lowercase()) {
        return Ok(None);
    }
    vocab(c)?;
    let mut st = c.prepare_cached("SELECT term, doc FROM temp.workmap_vocab WHERE term > ?1 AND term < ?2 AND length(term) <= ?3 ORDER BY doc DESC, term LIMIT 2").map_err(|e| e.to_string())?;
    let rows: Vec<(String, i64)> = st.query_map(params![w, format!("{w}{{"), w.len() as i64 + 2], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?.flatten().collect();
    match rows.as_slice() {
        [] => Ok(None),
        [(best, _)] => Ok(Some(best.clone())),
        [(best, d1), (second, d2), ..] => {
            if d2 * 2 > *d1 {
                Err(format!("ambiguous: {best} or {second}"))
            } else {
                Ok(Some(best.clone()))
            }
        }
    }
}

/// Classify the prompt's terms with this project's statistics.
pub fn analyse(c: &Connection, repo: &str, prompt: &str) -> Analysis {
    let n: i64 = c.query_row("SELECT count(*) FROM passages WHERE repo_id=?1 AND kind IN ('doc','message','commit')", [repo], |r| r.get(0)).unwrap_or(0);
    let generic = GENERIC_MIN_DF.max(n * GENERIC_PCT / 100);
    let all: i64 = c.query_row("SELECT count(*) FROM passages", [], |r| r.get(0)).unwrap_or(0);
    let global_generic = GENERIC_MIN_DF.max(all * GLOBAL_PCT / 100);
    let idents: Vec<String> = search::identifier_words(prompt).iter().map(|w| w.to_lowercase()).collect();
    let mut terms = Vec::new();
    for typed in prompt_terms(prompt) {
        let mut form = typed.clone();
        let mut df = count_df(c, repo, &typed);
        let mut note = String::new();
        // Unknown, or a rare literal typo of a much more frequent word.
        if df <= RARE_TYPO {
            match complete(c, &typed) {
                Ok(Some(x)) => {
                    let d = count_df(c, repo, &x);
                    if d > 0 && d >= TYPO_RATIO * df {
                        note = format!("{} in this project; completed to {x}", if df == 0 { "unknown".to_string() } else { format!("rare ({df})") });
                        form = x;
                        df = d;
                    }
                }
                Ok(None) => {}
                Err(e) => note = format!("unknown; not completed ({e})"),
            }
            if df == 0 && note.is_empty() {
                note = "unknown in this project".into();
            }
        }
        let mut common = false;
        let class = if WEAK.contains(&typed.as_str()) || WEAK.contains(&form.as_str()) {
            note = if note.is_empty() { "generic intent word".into() } else { format!("{note}; generic intent word") };
            Class::Weak
        } else if let Some(g) = global_df(c, &form).filter(|g| *g > global_generic) {
            note = format!("frequent: in {}% of all indexed passages", g * 100 / all.max(1));
            Class::Weak
        } else {
            if df > generic {
                common = true;
                note = format!("common here: in {}% of this project's passages", df * 100 / n.max(1));
            }
            Class::Anchor
        };
        let ident = idents.iter().any(|i| *i == typed);
        terms.push(Term { typed, form, class, df, ident, common, note });
    }
    // Adjacent terms in the prompt (no word or sentence punctuation between
    // them) form a phrase when one of them is a known anchor and both are indexed.
    let idx = |w: &str| terms.iter().position(|t: &Term| t.typed == w);
    let mut pairs = Vec::new();
    for clause in prompt.split(|c: char| ".,;:!?()[]{}<>\n\"`".contains(c)) {
        let toks = words(clause);
        for w in toks.windows(2) {
            if let (Some(a), Some(b)) = (idx(&w[0]), idx(&w[1])) {
                let (ta, tb) = (&terms[a], &terms[b]);
                let anchor = [ta, tb].iter().any(|t| t.class == Class::Anchor && !t.common && t.df > 0);
                if a != b && anchor && ta.df > 0 && tb.df > 0 && !pairs.contains(&(a, b)) {
                    pairs.push((a, b));
                }
            }
        }
    }
    Analysis { terms, pairs, n }
}

/// What one passage matched, and whether that is enough.
#[derive(Debug, Default)]
pub struct Judged {
    /// Indices of matched terms (prompt order).
    pub matched: Vec<usize>,
    pub phrases: Vec<(usize, usize)>,
    pub anchors: usize,
    pub coverage: f64,
    /// Share of the prompt's common anchors in that window (1 without any).
    pub common: f64,
    /// Word index of the first matched anchor or phrase in the passage.
    pub first: Option<usize>,
    pub ok: bool,
    pub why: &'static str,
}

pub fn judge(a: &Analysis, toks: &[String]) -> Judged {
    let set: BTreeSet<&str> = toks.iter().map(String::as_str).collect();
    let mut j = Judged::default();
    for (i, t) in a.terms.iter().enumerate() {
        if set.contains(t.form.as_str()) {
            j.matched.push(i);
        }
    }
    let mut first: Option<usize> = None;
    let mut note = |p: usize| first = Some(first.map_or(p, |f| f.min(p)));
    for &(x, y) in &a.pairs {
        let (fx, fy) = (&a.terms[x].form, &a.terms[y].form);
        if let Some(p) = toks.windows(2).position(|w| &w[0] == fx && &w[1] == fy) {
            j.phrases.push((x, y));
            note(p);
        }
    }
    // Anchors count together only when they occur close to each other: the
    // best window of NEARBY words decides (scattered generic words in a long
    // section do not add up).
    // Distinctive (incl. unknown) and common anchors are covered separately,
    // so neither part of the prompt can be carried by the other.
    let total: f64 = a.anchors().filter(|t| !t.common).map(|t| a.weight(t)).sum();
    let commons = a.anchors().filter(|t| t.common).count();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for &i in &j.matched {
        if a.terms[i].class == Class::Anchor {
            hits.extend(toks.iter().enumerate().filter(|(_, w)| **w == a.terms[i].form).map(|(p, _)| (p, i)));
        }
    }
    hits.sort();
    let (mut best, mut best_at, mut best_set, mut best_common) = ((false, 0.0), None, BTreeSet::new(), 0.0);
    for (k, &(p0, _)) in hits.iter().enumerate() {
        let set: BTreeSet<usize> = hits[k..].iter().take_while(|(p, _)| *p < p0 + NEARBY).map(|(_, i)| *i).collect();
        if !set.iter().any(|i| a.distinctive(*i)) {
            continue; // common words alone never qualify a passage
        }
        let w: f64 = set.iter().filter(|i| !a.terms[**i].common).map(|i| a.weight(&a.terms[*i])).sum();
        let cov = if total > 0.0 { w / total } else { 0.0 };
        let cc = if commons == 0 { 1.0 } else { set.iter().filter(|i| a.terms[**i].common).count() as f64 / commons as f64 };
        // With a single distinctive anchor in the prompt, the common ones must all be there.
        let need_common = if a.anchors().filter(|t| !t.common).count() <= 1 { 1.0 } else { COVERAGE };
        let key = (set.len() >= 2 && cov >= COVERAGE && cc >= need_common, cov);
        if key > best {
            (best, best_at, best_set, best_common) = (key, Some(p0), set, cc);
        }
    }
    if let Some(p) = best_at {
        note(p);
    }
    j.anchors = best_set.len();
    j.first = first;
    j.coverage = best.1;
    j.common = best_common;
    let n_anchors = a.anchors().count();
    let only = best_set.iter().next().map(|i| &a.terms[*i]);
    (j.ok, j.why) = if !j.phrases.is_empty() {
        (true, "phrase")
    } else if best.0 {
        (true, "nearby anchors")
    } else if j.anchors == 1 && n_anchors == 1 && only.map(|t| !t.common && t.ident).unwrap_or(false) {
        (true, "only anchor, identifier-shaped")
    } else if j.anchors == 0 {
        (false, "no anchor matched (generic words only)")
    } else {
        (false, "too little of the prompt matched")
    };
    j
}

/// Words of one indexed message / doc section / commit (all its parts).
fn unit_words(c: &Connection, kind: &str, node: &str, loc: &str) -> Vec<String> {
    let mut st = match c.prepare_cached("SELECT part, heading, text, title FROM passages WHERE node_id=?1 AND loc=?2 GROUP BY part ORDER BY part LIMIT 8") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let rows = st.query_map(params![node, loc], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)));
    let mut out = Vec::new();
    for (i, (heading, text, title)) in rows.into_iter().flatten().flatten().enumerate() {
        // A message's title is its conversation's label, not its content.
        let t = match kind {
            "message" => text,
            "doc" if i == 0 => format!("{heading} {text}"),
            "commit" if i == 0 => format!("{title} {text}"),
            _ => text,
        };
        out.extend(words(&t));
    }
    out
}

/// A message that is one JSON object (`{"risk_level":…,"rationale":…}`) is
/// harness or tool metadata recorded under a speaker, not conversation.
fn structured(c: &Connection, node: &str, loc: &str) -> bool {
    let t: String = c.query_row("SELECT text FROM passages WHERE node_id=?1 AND loc=?2 ORDER BY part LIMIT 1", params![node, loc], |r| r.get(0)).unwrap_or_default();
    let t = t.trim_start();
    t.starts_with("{\"") && t.contains("\":")
}

/// Other conversations whose messages contain these words as a phrase (capped).
fn recurs(c: &Connection, node: &str, win: &[String]) -> i64 {
    let Some(m) = search::fts_query(&format!("\"{}\"", win.join(" ")), false) else { return 0 };
    c.query_row(
        "SELECT count(*) FROM (SELECT DISTINCT p.node_id FROM passages p WHERE p.kind='message' AND p.node_id<>?1 AND p.id IN (SELECT rowid FROM passages_fts WHERE passages_fts MATCH ?2) LIMIT ?3)",
        params![node, m, TEMPLATE_CONVS - 1],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Harness boilerplate: the matched window, or a short message's opening,
/// recurs verbatim in other conversations.
fn boilerplate(c: &Connection, node: &str, toks: &[String], first: usize) -> Option<&'static str> {
    if toks.len() < 6 {
        return None;
    }
    let s = first.saturating_sub(3).min(toks.len().saturating_sub(WIN));
    let win = &toks[s..(s + WIN).min(toks.len())];
    if recurs(c, node, win) >= TEMPLATE_CONVS - 1 {
        return Some("matched text recurs verbatim in other conversations (template)");
    }
    if first < HEAD && s > 0 {
        let head = &toks[..WIN.min(toks.len())];
        if recurs(c, node, head) >= TEMPLATE_CONVS - 1 {
            return Some("message opens with text recurring in other conversations (template)");
        }
    }
    None
}

fn jaccard(a: &BTreeSet<&str>, b: &BTreeSet<&str>) -> f64 {
    let i = a.intersection(b).count() as f64;
    let u = (a.len() + b.len()) as f64 - i;
    if u == 0.0 {
        1.0
    } else {
        i / u
    }
}

fn contains_seq(hay: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

pub struct Item {
    pub r: String,
    pub lines: Vec<String>,
}

struct Picked {
    node: String,
    /// Source line of the kept copy (the earliest of identical copies in one conversation).
    line: i64,
    win: Vec<String>,
    /// Normalized texts in this cluster; the first is the kept passage's.
    keys: Vec<String>,
    toks: Vec<String>,
    strength: usize,
    /// The item line after "- <ref> — ".
    rest: String,
    excerpt: String,
    r: String,
    same: usize,
    near: usize,
}

/// Counts for the opt-in trace log (no prompt or passage text).
#[derive(Default, Debug)]
pub struct Tally {
    pub terms: usize,
    pub anchors: usize,
    pub unknown: usize,
    pub weak: usize,
    pub candidates: usize,
    pub not_relevant: usize,
    pub own: usize,
    pub template: usize,
    pub duplicate: usize,
    pub same_source: usize,
    pub seen: usize,
    pub budget: usize,
    pub emitted: usize,
}

impl Tally {
    pub fn line(&self) -> String {
        format!(
            "terms={} anchors={} unknown={} weak={} candidates={} not_relevant={} own={} template={} duplicate={} same_source={} seen={} budget={} emitted={}",
            self.terms, self.anchors, self.unknown, self.weak, self.candidates, self.not_relevant, self.own, self.template, self.duplicate, self.same_source, self.seen, self.budget, self.emitted
        )
    }
}

/// Bounded `additionalContext` for one prompt (empty: say nothing), the
/// `--explain` trace and the counts.
pub fn context(env: &Env, c: &Connection, p: &Project, session_id: &str, prompt: &str, ctx: &Ctx) -> Result<(String, Vec<String>, Tally), String> {
    let mut tr: Vec<String> = Vec::new();
    let mut tally = Tally::default();
    let mut items: Vec<Item> = Vec::new();
    // Exact path hits inside this project.
    for t in prompt_paths(prompt) {
        let like = format!("%/{}", t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let cands = nodes(c, "repo_id=?1 AND kind IN ('file','doc') AND path LIKE ?2 ESCAPE '\\' LIMIT 20", &[&p.repo.id, &like]).unwrap_or_default();
        let n_conv =
            |id: &str| c.query_row("SELECT count(DISTINCT src) FROM edges WHERE dst=?1 AND kind IN ('read','write') AND strength IN ('observed','parsed')", [id], |r| r.get::<_, i64>(0)).unwrap_or(0);
        let best = cands
            .iter()
            .filter_map(|n| n.checkout_id.as_deref().and_then(|id| p.checkouts.iter().find(|x| x.id == id)).map(|co| (n_conv(&n.id), co.id == p.focus.id, n, co)))
            .max_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        match best {
            Some((k, _, n, co)) => {
                let relp = rel(n.path.as_deref().unwrap_or(""), &p.top(co)).to_string();
                let r = sh_quote(&view::qualified_raw(p, co, &relp));
                tr.push(format!("path {t}: file {r}"));
                items.push(Item { r: r.clone(), lines: vec![format!("- {r} — file named in the prompt · {k} recorded conversation(s) read/wrote it")] });
            }
            None => tr.push(format!("path {t}: no such file in this project")),
        }
    }
    let a = analyse(c, &p.repo.id, prompt);
    tally.terms = a.terms.len();
    tally.anchors = (0..a.terms.len()).filter(|i| a.distinctive(*i)).count();
    tally.unknown = a.anchors().filter(|t| t.df == 0).count();
    tally.weak = a.terms.iter().filter(|t| t.class == Class::Weak).count();
    if ctx.explain {
        tr.push(format!("project {} · {} indexed passages (doc/message/commit) · weak above {GENERIC_PCT}% here or {GLOBAL_PCT}% of all passages", p.handle, a.n));
        for t in &a.terms {
            let class = match (t.class, t.df) {
                (Class::Weak, _) => "weak",
                (Class::Anchor, 0) => "anchor, unknown",
                _ if t.common => "anchor, common",
                _ => "anchor",
            };
            tr.push(format!("term {:<16} {class} · df {}{}", t.shown(), t.df, if t.note.is_empty() { String::new() } else { format!(" · {}", t.note) }));
        }
        for (x, y) in &a.pairs {
            tr.push(format!("phrase \"{} {}\"", a.terms[*x].form, a.terms[*y].form));
        }
    }
    let query = a.query();
    let mut picked: Vec<Picked> = Vec::new();
    if query.is_empty() {
        tr.push("no known anchor or phrase in the prompt: nothing searched".into());
    } else {
        // Text only: conversation labels repeat across templated sessions and
        // would fill the candidate slots with passages that never match.
        let q = search::Query { text: query.clone(), kinds: vec!["doc".into(), "message".into(), "commit".into()], any: true, limit: CANDIDATES, ..Default::default() };
        let (hits, total) = search::run_pref_in(c, &q, Some(&p.repo.id), None, Some(&p.focus.id), true).map_err(|e| e.to_string())?;
        tally.candidates = hits.len();
        tr.push(format!("query {query:?}: {total} passage(s), {} candidate(s)", hits.len()));
        let mut sessions: Option<Vec<Node>> = None;
        for h in hits {
            if picked.len() + items.len() >= MAX_ITEMS {
                break;
            }
            let at = format!("{} {}:{}", h.kind, &h.node_id[h.node_id.len().saturating_sub(12)..], h.loc);
            let toks = unit_words(c, &h.kind, &h.node_id, &h.loc);
            let j = judge(&a, &toks);
            if !j.ok {
                tally.not_relevant += 1;
                if ctx.explain {
                    tr.push(format!("drop {at}: {} ({} anchor(s), coverage {:.2}, common {:.2})", j.why, j.anchors, j.coverage, j.common));
                }
                continue;
            }
            let co = h.checkout_id.as_deref().and_then(|id| p.checkouts.iter().find(|x| x.id == id)).unwrap_or(&p.focus);
            let (r, desc) = match h.kind.as_str() {
                "message" => {
                    let all = sessions.get_or_insert_with(|| repo_sessions(c, &p.repo.id));
                    let Some(s) = all.iter().find(|s| s.id == h.node_id) else { continue };
                    if !session_id.is_empty() && meta(s, "native_id") == session_id {
                        tally.own += 1;
                        tr.push(format!("drop {at}: this very conversation"));
                        continue;
                    }
                    let rest = format!("message:{}:{}", view::more::session_prefix(all, s), h.loc);
                    let who = if h.speaker == "assistant" { "assistant message (past answer, unverified)".to_string() } else { format!("{} message", h.speaker) };
                    (sh_quote(&view::qualified_raw(p, co, &rest)), format!("{} {who} {} · conversation \"{}\"", meta(s, "provider"), fmt_date(h.ts), clip(&s.label, 50)))
                }
                "doc" => {
                    let relp = if h.title.starts_with("claude-memory/") { h.title.clone() } else { rel(&h.source_path, &p.top(co)).to_string() };
                    let rest = if h.loc.is_empty() { relp.clone() } else { format!("{relp}#{}", h.loc) };
                    (sh_quote(&view::qualified_raw(p, co, &rest)), format!("doc {relp}{} l.{}", if h.heading.is_empty() { String::new() } else { format!(" § {}", clip(&h.heading, 40)) }, h.line))
                }
                "commit" => {
                    let main = p.checkouts.first().unwrap_or(&p.focus);
                    let sha = &h.loc[..h.loc.len().min(8)];
                    (sh_quote(&view::qualified_raw(p, main, &format!("commit:{sha}"))), format!("commit {sha} by {} {}", h.speaker, fmt_date(h.ts)))
                }
                _ => continue,
            };
            let first = j.first.unwrap_or(0);
            let key = format!("{}\0{}", h.speaker, toks.join(" "));
            let set: BTreeSet<&str> = toks.iter().map(String::as_str).collect();
            let s = first.saturating_sub(3).min(toks.len().saturating_sub(WIN));
            let win = &toks[s..(s + WIN).min(toks.len())];
            // Duplicates first (so the kept passage can say how many it stands for).
            if let Some(o) = picked.iter_mut().find(|o| o.keys.contains(&key)) {
                o.same += 1;
                tally.duplicate += 1;
                if o.keys[0] == key && o.node == h.node_id && h.line < o.line {
                    // Identical copies of one message: point at the first one.
                    tr.push(format!("drop {}: identical to {r}, which is kept instead", o.r));
                    (o.r, o.line) = (r, h.line);
                } else {
                    tr.push(format!("drop {r}: identical to {}", o.r));
                }
                continue;
            }
            let echo = |o: &Picked| (win.len() == WIN && contains_seq(&o.toks, win)) || (o.win.len() == WIN && contains_seq(&toks, &o.win));
            if let Some(o) = picked.iter_mut().find(|o| echo(o) || jaccard(&set, &o.toks.iter().map(String::as_str).collect()) >= NEAR) {
                o.near += 1;
                o.keys.push(key);
                tally.duplicate += 1;
                tr.push(format!("drop {r}: near-identical to / echo of {}", o.r));
                continue;
            }
            if picked.iter().any(|o| o.node == h.node_id) || items.iter().any(|i| i.r == r) {
                tally.same_source += 1;
                tr.push(format!("drop {r}: one passage per conversation/doc already chosen"));
                continue;
            }
            if h.kind == "message" {
                if structured(c, &h.node_id, &h.loc) {
                    tally.template += 1;
                    tr.push(format!("drop {r}: message is a JSON object (tool/harness metadata)"));
                    continue;
                }
                if let Some(why) = boilerplate(c, &h.node_id, &toks, first) {
                    tally.template += 1;
                    tr.push(format!("drop {r}: {why}"));
                    continue;
                }
            }
            let mut names: Vec<String> = Vec::new();
            for (k, i) in j.matched.iter().enumerate() {
                if !j.matched[..k].iter().any(|x| a.terms[*x].form == a.terms[*i].form) {
                    names.push(a.terms[*i].shown());
                }
            }
            let phr: Vec<String> = j.phrases.iter().map(|(x, y)| format!("\"{} {}\"", a.terms[*x].form, a.terms[*y].form)).collect();
            let rest = format!("{desc} · matched: {}{}", names.join(", "), if phr.is_empty() { String::new() } else { format!(" · phrase {}", phr.join(", ")) });
            tr.push(format!("keep {r}: {} ({} anchor(s), coverage {:.2}, common {:.2})", j.why, j.anchors, j.coverage, j.common));
            picked.push(Picked {
                node: h.node_id.clone(),
                line: h.line,
                win: win.to_vec(),
                keys: vec![key],
                toks: toks.clone(),
                strength: 2 * j.phrases.len() + j.anchors,
                rest,
                excerpt: clip(&h.snippet.replace('\n', " "), 200),
                r,
                same: 0,
                near: 0,
            });
        }
    }
    // Strongest first; the full-text rank breaks ties (stable sort).
    picked.sort_by(|x, y| y.strength.cmp(&x.strength));
    for o in picked {
        let mut head = format!("- {} — {}", o.r, o.rest);
        match (o.same, o.near) {
            (0, 0) => {}
            (s, 0) => {
                let _ = write!(head, " · +{s} identical cop{}", if s == 1 { "y" } else { "ies" });
            }
            (0, n) => {
                let _ = write!(head, " · +{n} near-identical");
            }
            (s, n) => {
                let _ = write!(head, " · +{s} identical, +{n} near-identical");
            }
        }
        items.push(Item { r: o.r, lines: vec![head, format!("  \"{}\"", o.excerpt)] });
    }
    if items.is_empty() {
        return Ok((String::new(), tr, tally));
    }
    let seen_path = hook::seen_file(env, session_id);
    let seen: BTreeSet<String> = seen_path.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
    let before = items.len();
    items.retain(|i| !seen.contains(&i.r));
    tally.seen = before - items.len();
    if tally.seen > 0 {
        tr.push(format!("{} passage(s) already shown in this session", tally.seen));
    }
    if items.is_empty() {
        return Ok((String::new(), tr, tally));
    }
    let mut o = format!("workmap: indexed evidence in project {} lexically matching this prompt (historical text, not decisions):\n", p.handle);
    let foot = format!("Expand a reference for the full text and neighbours: {} expand '<ref>'\n", ctx.cmd);
    let budget = ctx.budget.saturating_sub(foot.len());
    let mut emitted = Vec::new();
    for it in items {
        let block: String = it.lines.iter().map(|l| format!("{l}\n")).collect();
        if o.len() + block.len() > budget {
            tally.budget += 1;
            tr.push(format!("budget: {} does not fit", it.r));
            continue; // a smaller later item may still fit; never cut one
        }
        o.push_str(&block);
        emitted.push(it.r);
    }
    tally.emitted = emitted.len();
    if emitted.is_empty() {
        return Ok((String::new(), tr, tally));
    }
    o.push_str(&foot);
    if let (Some(f), false) = (seen_path, ctx.explain) {
        hook::remember(&f, &seen, &emitted);
    }
    Ok((o, tr, tally))
}
