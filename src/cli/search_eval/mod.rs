//! Score search rankings against queries whose answers are known.
//!
//! Two runs use this: [`synthetic`], a small corpus kept in the repo and run
//! by `cargo test`, and [`live`], your own knowledge base and queries, which
//! stay out of the repo.
//!
//! Queries come from a TOML file:
//!
//! ```toml
//! [[query]]
//! q = "why did we drop CoreML"
//! kind = "natural_language"
//! relevant = ["memory:gpu-coreml", "todo:12", "code:src/embed/mod.rs"]
//!
//! [[query]]
//! q = "kubernetes autoscaling"
//! kind = "no_answer"        # or expect_none = true with any kind
//! ```
//!
//! A label is `<type>:<ref>` (see [`label`]): `memory:<key>`,
//! `person:<id>`, `meeting:<id>`, `project:<id>`, `todo:<id>`,
//! `research:<slug>`, `repo:<name>`, `symbol:<name>` and `code:<file>`, which
//! matches any chunk or line of that file. `kind` is one of `known_item`,
//! `identifier`, `natural_language`, `temporal` and `no_answer`; reports
//! break the scores down by it. A query with no labels and no `expect_none`
//! is only printed, so a first run lists labels to copy.
//!
//! Each [`Ranker`] runs every query. The first is the baseline: the others
//! are compared with it query by query, with paired bootstrap confidence
//! intervals. A later change plugs in a ranker of its own beside
//! [`current`].

mod live;
mod synthetic;

use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::db::Database;
use crate::search::{rank, search_hits_embedded, HitSource, SearchResult};

/// A result's [`SearchResult::item`] as one `<type>:<ref>` label, such as
/// `todo:12`.
pub fn label(r: &SearchResult) -> String {
    let (kind, item) = r.item();
    format!("{kind}:{item}")
}

/// What a query is testing, for breaking scores down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    /// Names one record, as a person would remember it.
    KnownItem,
    /// An exact name: a symbol, a key, a file.
    Identifier,
    /// A question in ordinary words.
    NaturalLanguage,
    /// Asks by time: a month, a meeting, "last week".
    Temporal,
    /// Nothing in the knowledge base answers it.
    NoAnswer,
}

impl QueryKind {
    fn name(self) -> &'static str {
        match self {
            QueryKind::KnownItem => "known_item",
            QueryKind::Identifier => "identifier",
            QueryKind::NaturalLanguage => "natural_language",
            QueryKind::Temporal => "temporal",
            QueryKind::NoAnswer => "no_answer",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalQuery {
    pub q: String,
    #[serde(default)]
    pub kind: Option<QueryKind>,
    #[serde(default)]
    pub relevant: Vec<String>,
    #[serde(default)]
    pub expect_none: bool,
}

impl EvalQuery {
    /// The right answer is no results.
    pub fn expects_none(&self) -> bool {
        self.expect_none || self.kind == Some(QueryKind::NoAnswer)
    }

    /// Scored for recall and ranking: it has labels.
    fn labelled(&self) -> bool {
        !self.relevant.is_empty()
    }
}

/// Read the `[[query]]` entries of an eval file.
pub fn parse_queries(text: &str) -> Result<Vec<EvalQuery>> {
    #[derive(Deserialize)]
    struct File {
        query: Vec<EvalQuery>,
    }
    let file: File = toml::from_str(text)?;
    for q in &file.query {
        if q.expects_none() && q.labelled() {
            bail!("query {:?} expects no results but lists relevant ones", q.q);
        }
    }
    Ok(file.query)
}

/// The search settings a ranker runs with.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub threshold: f32,
    pub margin: Option<f32>,
    pub semantic_limit: usize,
    pub code_text_limit: usize,
}

impl Params {
    pub fn from_settings(s: &crate::config::Settings) -> Self {
        Self {
            threshold: s.semantic_threshold() as f32,
            margin: s.semantic_margin().map(|m| m as f32),
            semantic_limit: s.search.semantic_limit,
            code_text_limit: s.search.code_text_limit,
        }
    }

    fn hits(&self, db: &Database, q: &str, emb: Option<&[f32]>) -> Result<Vec<crate::search::Hit>> {
        search_hits_embedded(
            db,
            q,
            emb,
            self.threshold,
            self.margin,
            self.semantic_limit,
            self.code_text_limit,
        )
    }
}

type RankFn = dyn Fn(&Database, &str, Option<&[f32]>) -> Result<Vec<SearchResult>>;

/// A way to turn a query, and its vector when there is one, into ordered
/// results.
pub struct Ranker {
    pub name: &'static str,
    run: Box<RankFn>,
}

impl Ranker {
    pub fn new(
        name: &'static str,
        run: impl Fn(&Database, &str, Option<&[f32]>) -> Result<Vec<SearchResult>> + 'static,
    ) -> Self {
        Self {
            name,
            run: Box::new(run),
        }
    }
}

/// `sclerox search` as it is: [`rank`] over every tier.
pub fn current(p: Params) -> Ranker {
    Ranker::new("current", move |db, q, emb| {
        Ok(rank(p.hits(db, q, emb)?)
            .into_iter()
            .map(|h| h.result)
            .collect())
    })
}

/// The keyword tiers alone, in the order they return.
pub fn keyword(p: Params) -> Ranker {
    Ranker::new("keyword", move |db, q, emb| {
        Ok(p.hits(db, q, emb)?
            .into_iter()
            .filter(|h| h.source == HitSource::Keyword)
            .map(|h| h.result)
            .collect())
    })
}

/// Every hit with a cosine score, by that score: the semantic tiers plus
/// keyword hits that have a stored vector.
pub fn cosine(p: Params) -> Ranker {
    Ranker::new("cosine", move |db, q, emb| {
        Ok(rank(p.hits(db, q, emb)?)
            .into_iter()
            .filter(|h| h.score.is_some())
            .map(|h| h.result)
            .collect())
    })
}

/// The rankers every eval compares, baseline first.
pub fn rankers(p: Params) -> Vec<Ranker> {
    vec![current(p), keyword(p), cosine(p)]
}

/// One ranker's result on one query.
#[derive(Debug, Clone, Default)]
pub struct QueryScore {
    pub r5: f64,
    pub r10: f64,
    /// Reciprocal rank of the first relevant hit; 0 when none is returned.
    pub rr: f64,
    pub ndcg10: f64,
    /// A query that expects nothing got at least one result.
    pub false_result: bool,
    /// Output tokens up to and including the first relevant hit.
    pub tokens_to_first: Option<usize>,
    /// Output tokens of every result.
    pub tokens: usize,
}

/// Score `results` for `q`. `render` is the text an agent reads for each
/// result, for counting tokens. Only the first hit for each label counts
/// toward nDCG, so the chunks of one file are not counted as several
/// relevant results.
pub fn score(
    q: &EvalQuery,
    results: &[SearchResult],
    render: &dyn Fn(&SearchResult) -> String,
) -> QueryScore {
    let relevant: HashSet<&str> = q.relevant.iter().map(String::as_str).collect();
    let labels: Vec<String> = results.iter().map(label).collect();
    let mut s = QueryScore {
        false_result: q.expects_none() && !results.is_empty(),
        ..Default::default()
    };

    let mut tokens = 0;
    for (r, label) in results.iter().zip(&labels) {
        tokens += crate::embed::tokens::count_tokens(&render(r));
        if s.tokens_to_first.is_none() && relevant.contains(label.as_str()) {
            s.tokens_to_first = Some(tokens);
        }
    }
    s.tokens = tokens;

    if relevant.is_empty() {
        return s;
    }
    let found = |n: usize| {
        let top: HashSet<&str> = labels.iter().take(n).map(String::as_str).collect();
        relevant.iter().filter(|r| top.contains(*r)).count() as f64 / relevant.len() as f64
    };
    s.r5 = found(5);
    s.r10 = found(10);
    if let Some(i) = labels.iter().position(|l| relevant.contains(l.as_str())) {
        s.rr = 1.0 / (i + 1) as f64;
    }
    let gain = |i: usize| 1.0 / ((i + 2) as f64).log2();
    let mut seen = HashSet::new();
    let dcg: f64 = labels
        .iter()
        .take(10)
        .enumerate()
        .filter(|(_, l)| relevant.contains(l.as_str()) && seen.insert(l.as_str()))
        .map(|(i, _)| gain(i))
        .sum();
    let ideal: f64 = (0..relevant.len().min(10)).map(gain).sum();
    s.ndcg10 = dcg / ideal;
    s
}

/// Every ranker's results and scores for every query.
pub struct Evaluation {
    pub rankers: Vec<&'static str>,
    /// `[ranker][query]`.
    pub results: Vec<Vec<Vec<SearchResult>>>,
    /// `[ranker][query]`.
    pub scores: Vec<Vec<QueryScore>>,
}

/// Run every ranker on every query. `embed` turns a query into its vector,
/// once per query, or `None` for keyword search only.
pub fn evaluate(
    db: &Database,
    queries: &[EvalQuery],
    rankers: &[Ranker],
    embed: &mut dyn FnMut(&str) -> Option<Vec<f32>>,
    render: &dyn Fn(&SearchResult) -> String,
) -> Result<Evaluation> {
    let mut results = vec![Vec::with_capacity(queries.len()); rankers.len()];
    let mut scores = vec![Vec::with_capacity(queries.len()); rankers.len()];
    for q in queries {
        let emb = embed(&q.q);
        for (i, r) in rankers.iter().enumerate() {
            let hits = (r.run)(db, &q.q, emb.as_deref())?;
            scores[i].push(score(q, &hits, render));
            results[i].push(hits);
        }
    }
    Ok(Evaluation {
        rankers: rankers.iter().map(|r| r.name).collect(),
        results,
        scores,
    })
}

/// Means over a group of queries.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Queries with labels.
    pub labelled: usize,
    pub r5: f64,
    pub r10: f64,
    pub mrr: f64,
    pub ndcg10: f64,
    /// Queries that expect nothing.
    pub no_answer: usize,
    /// Share of [`Self::no_answer`] queries that returned something.
    pub false_rate: f64,
    /// Labelled queries that returned a relevant hit.
    pub found: usize,
    /// Mean of [`QueryScore::tokens_to_first`] over [`Self::found`].
    pub tokens_to_first: f64,
    /// Mean output tokens over every query in the group.
    pub tokens: f64,
}

/// A group of queries: all of them, or one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    All,
    Kind(QueryKind),
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::All => "all",
            Group::Kind(k) => k.name(),
        }
    }

    fn contains(self, q: &EvalQuery) -> bool {
        match self {
            Group::All => true,
            Group::Kind(k) => q.kind == Some(k),
        }
    }

    /// `all`, then each kind present in `queries`.
    pub fn present(queries: &[EvalQuery]) -> Vec<Group> {
        let kinds: std::collections::BTreeSet<QueryKind> =
            queries.iter().filter_map(|q| q.kind).collect();
        std::iter::once(Group::All)
            .chain(kinds.into_iter().map(Group::Kind))
            .collect()
    }
}

pub fn summarize(queries: &[EvalQuery], scores: &[QueryScore], group: Group) -> Summary {
    let mut s = Summary::default();
    let mut members = 0usize;
    for (q, sc) in queries
        .iter()
        .zip(scores)
        .filter(|(q, _)| group.contains(q))
    {
        members += 1;
        s.tokens += sc.tokens as f64;
        if q.expects_none() {
            s.no_answer += 1;
            s.false_rate += f64::from(u8::from(sc.false_result));
        }
        if q.labelled() {
            s.labelled += 1;
            s.r5 += sc.r5;
            s.r10 += sc.r10;
            s.mrr += sc.rr;
            s.ndcg10 += sc.ndcg10;
            if let Some(t) = sc.tokens_to_first {
                s.found += 1;
                s.tokens_to_first += t as f64;
            }
        }
    }
    let mean = |sum: &mut f64, n: usize| *sum = if n == 0 { 0.0 } else { *sum / n as f64 };
    let labelled = s.labelled;
    mean(&mut s.r5, labelled);
    mean(&mut s.r10, labelled);
    mean(&mut s.mrr, labelled);
    mean(&mut s.ndcg10, labelled);
    mean(&mut s.false_rate, s.no_answer);
    mean(&mut s.tokens_to_first, s.found);
    mean(&mut s.tokens, members);
    s
}

/// SplitMix64: a small seeded generator, so bootstrap intervals repeat run
/// to run without a dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A difference between two rankers on the same queries, `b - a`, with a
/// 95% interval.
#[derive(Debug, Clone, Copy)]
pub struct Diff {
    pub mean: f64,
    pub lo: f64,
    pub hi: f64,
}

impl Diff {
    /// The interval leaves out zero.
    pub fn significant(&self) -> bool {
        self.lo > 0.0 || self.hi < 0.0
    }
}

/// Paired bootstrap of `mean(b - a)`: resample the queries with replacement,
/// keeping each query's pair together. `None` with no queries.
pub fn paired_bootstrap(a: &[f64], b: &[f64], resamples: usize, seed: u64) -> Option<Diff> {
    assert_eq!(a.len(), b.len());
    let n = a.len();
    if n == 0 {
        return None;
    }
    let diffs: Vec<f64> = a.iter().zip(b).map(|(a, b)| b - a).collect();
    let mean = diffs.iter().sum::<f64>() / n as f64;
    let mut rng = SplitMix64(seed);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..n).map(|_| diffs[rng.below(n)]).sum::<f64>() / n as f64)
        .collect();
    means.sort_by(f64::total_cmp);
    let at = |p: f64| means[((resamples - 1) as f64 * p).round() as usize];
    Some(Diff {
        mean,
        lo: at(0.025),
        hi: at(0.975),
    })
}

const RESAMPLES: usize = 2000;
const SEED: u64 = 0x5C1E_7055;

/// A metric compared between rankers, and the queries it applies to.
struct Metric {
    name: &'static str,
    value: fn(&QueryScore) -> f64,
    no_answer: bool,
}

const COMPARED: [Metric; 4] = [
    Metric {
        name: "R@10",
        value: |s| s.r10,
        no_answer: false,
    },
    Metric {
        name: "MRR",
        value: |s| s.rr,
        no_answer: false,
    },
    Metric {
        name: "nDCG@10",
        value: |s| s.ndcg10,
        no_answer: false,
    },
    Metric {
        name: "false",
        value: |s| f64::from(u8::from(s.false_result)),
        no_answer: true,
    },
];

/// The report printed after an eval: a table per query group, then each
/// ranker against the baseline.
pub fn report(queries: &[EvalQuery], ev: &Evaluation) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for group in Group::present(queries) {
        let _ = writeln!(out, "\n[{}]", group.name());
        let _ = writeln!(
            out,
            "{:<10} {:>4} {:>6} {:>6} {:>6} {:>8} {:>4} {:>6} {:>8} {:>7}",
            "ranker", "n", "R@5", "R@10", "MRR", "nDCG@10", "none", "false", "tok→1st", "tokens"
        );
        for (name, scores) in ev.rankers.iter().zip(&ev.scores) {
            let s = summarize(queries, scores, group);
            let _ = writeln!(
                out,
                "{name:<10} {:>4} {:>6.3} {:>6.3} {:>6.3} {:>8.3} {:>4} {:>6.3} {:>8.0} {:>7.0}",
                s.labelled,
                s.r5,
                s.r10,
                s.mrr,
                s.ndcg10,
                s.no_answer,
                s.false_rate,
                s.tokens_to_first,
                s.tokens
            );
        }
    }

    if ev.rankers.len() > 1 {
        let _ = writeln!(
            out,
            "\nAgainst {} (mean difference, 95% paired bootstrap interval; * excludes 0)",
            ev.rankers[0]
        );
        for (name, scores) in ev.rankers.iter().zip(&ev.scores).skip(1) {
            for group in Group::present(queries) {
                let mut cells = Vec::new();
                for m in &COMPARED {
                    let (a, b): (Vec<f64>, Vec<f64>) = queries
                        .iter()
                        .zip(ev.scores[0].iter().zip(scores))
                        .filter(|(q, _)| group.contains(q))
                        .filter(|(q, _)| {
                            if m.no_answer {
                                q.expects_none()
                            } else {
                                q.labelled()
                            }
                        })
                        .map(|(_, (a, b))| ((m.value)(a), (m.value)(b)))
                        .unzip();
                    if let Some(d) = paired_bootstrap(&a, &b, RESAMPLES, SEED) {
                        let star = if d.significant() { "*" } else { " " };
                        cells.push(format!(
                            "{} {:+.3} [{:+.3}, {:+.3}]{star}",
                            m.name, d.mean, d.lo, d.hi
                        ));
                    }
                }
                if !cells.is_empty() {
                    let _ = writeln!(out, "{name:<10} {:<17} {}", group.name(), cells.join("  "));
                }
            }
        }
    }
    out
}

/// The summary numbers kept as a baseline: `ranker.group` to metric to
/// value, rounded so the file stays readable.
pub fn baseline(queries: &[EvalQuery], ev: &Evaluation) -> BTreeMap<String, BTreeMap<String, f64>> {
    let round = |v: f64| (v * 1000.0).round() / 1000.0;
    let mut out = BTreeMap::new();
    for (name, scores) in ev.rankers.iter().zip(&ev.scores) {
        for group in Group::present(queries) {
            let s = summarize(queries, scores, group);
            let mut m = BTreeMap::new();
            if s.labelled > 0 {
                m.insert("r5".to_string(), round(s.r5));
                m.insert("r10".to_string(), round(s.r10));
                m.insert("mrr".to_string(), round(s.mrr));
                m.insert("ndcg10".to_string(), round(s.ndcg10));
            }
            if s.no_answer > 0 {
                m.insert("false_rate".to_string(), round(s.false_rate));
            }
            out.insert(format!("{name}.{}", group.name()), m);
        }
    }
    out
}

/// Results printed per query by the eval runs.
const SHOWN: usize = 10;

/// One query's top results for each ranker, `*` on the relevant ones.
pub fn print_query(q: &EvalQuery, ev: &Evaluation, qi: usize) {
    let relevant: HashSet<&str> = q.relevant.iter().map(String::as_str).collect();
    let kind = q.kind.map_or("-", QueryKind::name);
    println!("\n{}  ({kind})", q.q);
    for (name, results) in ev.rankers.iter().zip(&ev.results) {
        let hits = &results[qi];
        println!("  {name} ({} hits)", hits.len());
        for (i, r) in hits.iter().take(SHOWN).enumerate() {
            let label = label(r);
            let mark = if relevant.contains(label.as_str()) {
                '*'
            } else {
                ' '
            };
            println!("   {mark}{:>2}. {label}  {}", i + 1, title(r));
        }
    }
}

/// A short description of a result, for reading eval output.
pub fn title(r: &SearchResult) -> String {
    let t = match r {
        SearchResult::Memory { snippet, .. } => snippet,
        SearchResult::Person { name, .. } => name,
        SearchResult::Meeting { title, .. } => title,
        SearchResult::Project { name, .. } => name,
        SearchResult::Todo { title, .. } => title,
        SearchResult::Investigation { name, .. } => name,
        SearchResult::Repo { name, .. } => name,
        SearchResult::Symbol {
            signature, name, ..
        } => signature.as_ref().unwrap_or(name),
        SearchResult::CodeText { text, .. } => text,
        SearchResult::CodeChunk { snippet, .. } => snippet,
    };
    t.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(70)
        .collect()
}

/// The union of each ranker's top `depth` results for one query, for
/// labelling without favouring any one ranker. Each label once, with the
/// rankers that returned it and its best rank among them.
pub fn pool(
    ev: &Evaluation,
    qi: usize,
    depth: usize,
) -> Vec<(String, String, Vec<&'static str>, usize)> {
    let mut by_label: BTreeMap<String, (String, Vec<&'static str>, usize)> = BTreeMap::new();
    for (name, results) in ev.rankers.iter().zip(&ev.results) {
        for (rank, r) in results[qi].iter().take(depth).enumerate() {
            let e = by_label
                .entry(label(r))
                .or_insert_with(|| (title(r), Vec::new(), rank));
            if !e.1.contains(name) {
                e.1.push(name);
            }
            e.2 = e.2.min(rank);
        }
    }
    let mut out: Vec<_> = by_label
        .into_iter()
        .map(|(label, (title, from, best))| (label, title, from, best))
        .collect();
    out.sort_by_key(|e| e.3);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(key: &str) -> SearchResult {
        SearchResult::Memory {
            id: 0,
            key: key.into(),
            snippet: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn query(relevant: &[&str]) -> EvalQuery {
        EvalQuery {
            q: String::new(),
            kind: None,
            relevant: relevant.iter().map(|s| s.to_string()).collect(),
            expect_none: false,
        }
    }

    fn render(_: &SearchResult) -> String {
        String::new()
    }

    #[test]
    fn scores_rank_metrics() {
        let results = [memory("x"), memory("a"), memory("y"), memory("b")];
        let s = score(&query(&["memory:a", "memory:b"]), &results, &render);
        assert_eq!(s.r5, 1.0);
        assert_eq!(s.rr, 0.5);
        let dcg = 1.0 / 3f64.log2() + 1.0 / 5f64.log2();
        let ideal = 1.0 + 1.0 / 3f64.log2();
        assert!((s.ndcg10 - dcg / ideal).abs() < 1e-12);
    }

    #[test]
    fn ndcg_counts_each_label_once() {
        let results = [memory("a"), memory("a")];
        let s = score(&query(&["memory:a", "memory:b"]), &results, &render);
        let ideal = 1.0 + 1.0 / 3f64.log2();
        assert!((s.ndcg10 - 1.0 / ideal).abs() < 1e-12);
    }

    #[test]
    fn no_answer_queries_score_any_result_as_false() {
        let q = EvalQuery {
            kind: Some(QueryKind::NoAnswer),
            ..query(&[])
        };
        assert!(score(&q, &[memory("a")], &render).false_result);
        assert!(!score(&q, &[], &render).false_result);
        let summary = summarize(&[q], &[score(&query(&[]), &[], &render)], Group::All);
        assert_eq!((summary.no_answer, summary.labelled), (1, 0));
    }

    #[test]
    fn parse_rejects_labels_on_no_answer_queries() {
        let ok = "[[query]]\nq = \"a\"\nkind = \"no_answer\"\n";
        assert!(parse_queries(ok).unwrap()[0].expects_none());
        let bad = "[[query]]\nq = \"a\"\nexpect_none = true\nrelevant = [\"memory:a\"]\n";
        assert!(parse_queries(bad).is_err());
        assert!(parse_queries("[[query]]\nq = \"a\"\nkind = \"other\"\n").is_err());
    }

    #[test]
    fn tokens_count_up_to_the_first_relevant_hit() {
        let results = [memory("x"), memory("a"), memory("y")];
        let s = score(&query(&["memory:a"]), &results, &|r| label(r));
        let each = crate::embed::tokens::count_tokens("memory:x");
        assert_eq!(s.tokens_to_first, Some(2 * each));
        assert_eq!(s.tokens, 3 * each);
    }

    #[test]
    fn bootstrap_brackets_a_clear_gain_and_repeats() {
        let a = vec![0.0; 30];
        let b: Vec<f64> = (0..30)
            .map(|i| if i % 3 == 0 { 0.0 } else { 1.0 })
            .collect();
        let d = paired_bootstrap(&a, &b, 1000, 7).unwrap();
        assert!((d.mean - 20.0 / 30.0).abs() < 1e-12);
        assert!(d.significant() && d.lo < d.mean && d.mean < d.hi);
        let again = paired_bootstrap(&a, &b, 1000, 7).unwrap();
        assert_eq!((d.lo, d.hi), (again.lo, again.hi));
        let same = paired_bootstrap(&a, &a, 1000, 7).unwrap();
        assert!(!same.significant());
        assert!(paired_bootstrap(&[], &[], 10, 1).is_none());
    }

    #[test]
    fn pool_merges_rankers_by_label() {
        let ev = Evaluation {
            rankers: vec!["a", "b"],
            results: vec![
                vec![vec![memory("x"), memory("y")]],
                vec![vec![memory("y"), memory("z")]],
            ],
            scores: vec![vec![QueryScore::default()], vec![QueryScore::default()]],
        };
        let pooled = pool(&ev, 0, 2);
        let labels: Vec<(&str, Vec<&str>)> = pooled
            .iter()
            .map(|(l, _, from, _)| (l.as_str(), from.clone()))
            .collect();
        assert_eq!(
            labels,
            [
                ("memory:x", vec!["a"]),
                ("memory:y", vec!["a", "b"]),
                ("memory:z", vec!["b"])
            ]
        );
    }
}
