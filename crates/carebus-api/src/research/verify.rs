//! Citation verification: deterministic code, no model. A citation counts only when its quote (at least
//! [`MIN_QUOTE_CHARS`] characters and [`MIN_QUOTE_WORDS`] words) appears in the result of the tool step it names,
//! and that step is the source call for its locator (provenance: [`source_tools`] + [`locator_matches`]). Then the
//! register re-resolves `eli` / `saos` / `eurlex`. `literature` quotes must be in the title or abstract of the cited
//! paper's own record in that step. `other` sources, `stats` (computed numbers, not a source) and citations without
//! a step are never verified. A claim is supported iff at least one of its citations is verified.
use serde_json::{Value, json};

use super::SourceKind;
use super::prompts::{
    LEGAL_DISCLAIMER, LEGAL_REPORT_TITLE, RESEARCH_DISCLAIMER, RESEARCH_REPORT_TITLE,
    UNVERIFIED_HEADING,
};
use super::tools::{Resolution, Resolver, eli_base, strata_result_ids, strata_texts_for};

/// A normalised quote shorter than this cannot verify anything (a few characters occur in any text).
pub const MIN_QUOTE_CHARS: usize = 20;
pub const MIN_QUOTE_WORDS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyStatus {
    Verified,
    QuoteMismatch,
    SourceUnresolved,
    NotInForce,
    Unverifiable,
}

impl VerifyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::QuoteMismatch => "quote_mismatch",
            Self::SourceUnresolved => "source_unresolved",
            Self::NotInForce => "not_in_force",
            Self::Unverifiable => "unverifiable",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub status: VerifyStatus,
    pub detail: Value,
}

impl Verdict {
    fn new(status: VerifyStatus, detail: Value) -> Self {
        Self { status, detail }
    }
}

/// A successful tool step of the run (never `submit_report`): what a quote may come from.
#[derive(Clone, Debug)]
pub struct ToolStepView {
    pub ordinal: i32,
    pub name: String,
    pub arguments: Value,
    pub text: String,
    /// The stored result was cut to the cap (`response.truncated`): for a literature search, trailing items were
    /// omitted (`mod::cap_tool_text`); otherwise the text was cut mid-way.
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct CitationIn {
    pub source_kind: SourceKind,
    pub locator: String,
    pub title: Option<String>,
    pub quote: String,
    pub step_ordinal: Option<i32>,
}

/// Only whitespace, quote marks and dashes are unified: every whitespace run (nbsp included) becomes one space,
/// typographic single/double quotes become `'` / `"`, hyphen/dash variants become `-`, and the ends are trimmed.
/// Nothing else changes: case is kept, no punctuation or other character is dropped or rewritten.
pub fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        let mapped = match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' | '\u{0060}'
            | '\u{00B4}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{00AB}' | '\u{00BB}'
            | '\u{2033}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            c if c.is_whitespace() => {
                space = true;
                continue;
            }
            c => c,
        };
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(mapped);
    }
    out
}

/// The normalised texts a quote may be found in: the result text itself and, when it is JSON, its string values
/// (so a quote of a passage matches even though the passage is JSON-escaped in the text).
fn haystacks(text: &str) -> Vec<String> {
    let mut out = vec![normalise(text)];
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let mut leaves = vec![];
        collect_strings(&v, &mut leaves);
        out.extend(leaves.into_iter().map(|s| normalise(&s)));
    }
    out
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

fn contains_normalised(text: &str, needle_norm: &str) -> bool {
    haystacks(text).iter().any(|h| h.contains(needle_norm))
}

pub const LITERATURE_RECORD: &str = "research_literature_record";
pub const LITERATURE_SEARCH: &str = "research_literature_search";

/// Scrape-only legal sources whose document call is a `web` source: the citation's locator is the result's
/// canonical `url`, and the quote must be in its plain `text`. Their search tools are not sources.
pub const LEGAL_DOCUMENT_TOOLS: [&str; 3] = [
    "legal_kio_document",
    "legal_uokik_document",
    "legal_duw_document",
];

/// The parsed result of a [`LEGAL_DOCUMENT_TOOLS`] step; `None` for any other tool or a result that is not JSON
/// (e.g. cut to the cap).
fn legal_document(step: &ToolStepView) -> Option<Value> {
    if !LEGAL_DOCUMENT_TOOLS.contains(&step.name.as_str()) {
        return None;
    }
    serde_json::from_str(&step.text).ok()
}

/// The only tools whose result may carry a quote of this kind of source; empty = no tool can (`other`, `stats`).
pub fn source_tools(kind: SourceKind) -> &'static [&'static str] {
    match kind {
        SourceKind::Eli => &["legal_eli_act_text"],
        SourceKind::Saos => &["legal_saos_judgment"],
        SourceKind::Eurlex => &["legal_eurlex_text"],
        SourceKind::Krs => &["legal_krs_extract"],
        SourceKind::Web => &[
            "web_scrape",
            "legal_kio_document",
            "legal_uokik_document",
            "legal_duw_document",
        ],
        SourceKind::Strata => &[super::tools::STRATA_SEARCH],
        SourceKind::Literature => &[LITERATURE_RECORD, LITERATURE_SEARCH],
        SourceKind::Stats | SourceKind::Other => &[],
    }
}

/// A `literature` locator: the discovery-engine paper id (uuid) and/or `doi:<doi>` / `pmid:<pmid>` (a bare
/// `10.…` token is a DOI), whitespace-separated. Every part given must match the same record.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LiteratureLocator {
    pub id: Option<String>,
    pub doi: Option<String>,
    pub pmid: Option<String>,
}

/// A DOI as compared: `https://doi.org/` / `doi:` prefixes dropped, ASCII-lowercased (DOIs are case-insensitive).
fn doi_key(s: &str) -> String {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let mut k = lower.as_str();
    for p in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "doi:",
    ] {
        if let Some(r) = k.strip_prefix(p) {
            k = r;
        }
    }
    k.trim().to_string()
}

/// `None` = not a locator of this form (an unknown token, a repeated part, or nothing).
pub fn parse_literature_locator(locator: &str) -> Option<LiteratureLocator> {
    let mut l = LiteratureLocator::default();
    for tok in locator.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        let (slot, value) = if let Some(d) = lower.strip_prefix("doi:") {
            (&mut l.doi, doi_key(d))
        } else if lower.starts_with("10.") || lower.starts_with("https://doi.org/") {
            (&mut l.doi, doi_key(tok))
        } else if let Some(p) = lower.strip_prefix("pmid:") {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (&mut l.pmid, p.to_string())
        } else if let Ok(u) = uuid::Uuid::parse_str(tok) {
            (&mut l.id, u.to_string())
        } else {
            return None;
        };
        if value.is_empty() || slot.is_some() {
            return None;
        }
        *slot = Some(value);
    }
    (l.id.is_some() || l.doi.is_some() || l.pmid.is_some()).then_some(l)
}

/// The records of a stored `research_literature_*` result that the locator names: the record itself
/// (`research_literature_record`) or the search `items` whose id / doi / pmid equal every part the locator gives.
pub fn literature_records(step: &ToolStepView, locator: &str) -> Vec<Value> {
    let Some(l) = parse_literature_locator(locator) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<Value>(&step.text) else {
        return vec![];
    };
    let records = match step.name.as_str() {
        LITERATURE_RECORD => vec![v],
        LITERATURE_SEARCH => v["items"].as_array().cloned().unwrap_or_default(),
        _ => vec![],
    };
    let field = |r: &Value, k: &str| arg_text(&r[k]).filter(|s| !s.is_empty());
    records
        .into_iter()
        .filter(|r| {
            l.id.as_ref().is_none_or(|want| {
                field(r, "id")
                    .and_then(|g| uuid::Uuid::parse_str(&g).ok())
                    .is_some_and(|g| g.to_string() == *want)
            }) && l
                .doi
                .as_ref()
                .is_none_or(|want| field(r, "doi").is_some_and(|g| doi_key(&g) == *want))
                && l.pmid
                    .as_ref()
                    .is_none_or(|want| field(r, "pmid").is_some_and(|g| g == *want))
        })
        .collect()
}

/// A JSON string or number argument as trimmed text.
fn arg_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A KRS number as the register keys it: 1..=10 digits, left-padded with zeros to 10.
pub fn krs_padded(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && t.len() <= 10 && t.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("{t:0>10}"))
}

/// An article number in the forms the ELI tool accepts (`133`, `133a`, `449^1`, `449(1)`, `449_1`, `449¹`, with
/// or without a leading `art.`), as `N[letters][^k]`. `None` when it is none of them.
pub fn normalise_article(s: &str) -> Option<String> {
    let lower = s.trim().to_lowercase();
    let t = lower
        .strip_prefix("art.")
        .or_else(|| lower.strip_prefix("art"))
        .unwrap_or(&lower)
        .trim();
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    let mut number = String::new();
    while i < chars.len() && chars[i].is_ascii_digit() && number.len() < 5 {
        number.push(chars[i]);
        i += 1;
    }
    if number.is_empty() {
        return None;
    }
    let mut letters = String::new();
    while i < chars.len() && chars[i].is_ascii_lowercase() && letters.len() < 3 {
        letters.push(chars[i]);
        i += 1;
    }
    let digits = |from: usize| -> (String, usize) {
        let mut d = String::new();
        let mut j = from;
        while j < chars.len() && chars[j].is_ascii_digit() && d.len() < 2 {
            d.push(chars[j]);
            j += 1;
        }
        (d, j)
    };
    let mut sup = String::new();
    if i < chars.len() {
        match chars[i] {
            '^' | '_' => {
                let (d, j) = digits(i + 1);
                sup = d;
                i = j;
            }
            '(' => {
                let (d, j) = digits(i + 1);
                if j < chars.len() && chars[j] == ')' {
                    sup = d;
                    i = j + 1;
                } else {
                    return None;
                }
            }
            _ => {
                while i < chars.len() && sup.len() < 2 {
                    let d = match chars[i] {
                        '⁰' => '0',
                        '¹' => '1',
                        '²' => '2',
                        '³' => '3',
                        '⁴' => '4',
                        '⁵' => '5',
                        '⁶' => '6',
                        '⁷' => '7',
                        '⁸' => '8',
                        '⁹' => '9',
                        _ => break,
                    };
                    sup.push(d);
                    i += 1;
                }
                if sup.is_empty() {
                    return None;
                }
            }
        }
        if sup.is_empty() || i != chars.len() {
            return None;
        }
    }
    Some(if sup.is_empty() {
        format!("{number}{letters}")
    } else {
        format!("{number}{letters}^{sup}")
    })
}

/// The article an `eli` locator names (`DU/1964/59 art. 133 § 1` → `art. 133`): `None` = no article (the whole
/// act), `Some(None)` = an article that is not in a known form.
fn locator_article(locator: &str) -> Option<Option<String>> {
    let rest = locator.trim().split_once(char::is_whitespace)?.1.trim();
    let lower = rest.to_lowercase();
    let after = lower
        .strip_prefix("art.")
        .or_else(|| lower.strip_prefix("art"))?;
    let token: String = after
        .trim_start()
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '§' | ',' | ';'))
        .collect();
    Some(normalise_article(&token))
}

/// An `eli` citation that names an article binds to a step fetched for that same article.
pub fn article_matches(locator: &str, args: &Value) -> bool {
    match locator_article(locator) {
        None => true,
        Some(None) => false,
        Some(Some(want)) => args["article"]
            .as_str()
            .and_then(normalise_article)
            .is_some_and(|got| got == want),
    }
}

/// A URL compared as given: trimmed, one trailing slash ignored.
fn url_key(s: &str) -> &str {
    let t = s.trim();
    t.strip_suffix('/').unwrap_or(t)
}

/// Whether the step (already known to be one of the kind's [`source_tools`]) is the call for this locator: its
/// arguments name it (`strata`: its result lists it as a chunk or document id; a legal document step: its result's
/// `url`).
pub fn locator_matches(kind: SourceKind, locator: &str, step: &ToolStepView) -> bool {
    let loc = locator.trim();
    let args = &step.arguments;
    match kind {
        SourceKind::Eli => {
            arg_text(&args["eli"]).is_some_and(|e| !e.is_empty() && e == eli_base(loc))
        }
        SourceKind::Saos => arg_text(&args["id"]).is_some_and(|id| !id.is_empty() && id == loc),
        SourceKind::Eurlex => {
            arg_text(&args["celex"]).is_some_and(|c| !c.is_empty() && c.eq_ignore_ascii_case(loc))
        }
        SourceKind::Krs => match (
            arg_text(&args["krs"]).as_deref().and_then(krs_padded),
            krs_padded(loc),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
        // a legal document step: the canonical `url` in its result; `web_scrape`: the url it was called with
        SourceKind::Web if LEGAL_DOCUMENT_TOOLS.contains(&step.name.as_str()) => {
            legal_document(step).is_some_and(|v| {
                v["url"]
                    .as_str()
                    .is_some_and(|u| !url_key(u).is_empty() && url_key(u) == url_key(loc))
            })
        }
        SourceKind::Web => args["url"]
            .as_str()
            .is_some_and(|u| !url_key(u).is_empty() && url_key(u) == url_key(loc)),
        SourceKind::Strata => strata_result_ids(&step.text).iter().any(|id| id == loc),
        SourceKind::Literature => !literature_records(step, loc).is_empty(),
        SourceKind::Stats | SourceKind::Other => false,
    }
}

/// Every check that needs no register: quote length, provenance (the named step is the source call for the
/// locator) and the quote in that step's result. `Ok(None)` = passed, and the kind needs [`Resolver::resolve`].
pub fn verify_local(c: &CitationIn, steps: &[ToolStepView]) -> Result<Option<Verdict>, Verdict> {
    // a `research_stats_*` result is a computation over the model's own inputs, not a source: never verified, so
    // a claim resting on it alone lands under "Not verified" with this reason (before the quote-length check: its
    // quote is usually just a number)
    if c.source_kind == SourceKind::Stats {
        return Err(Verdict::new(
            VerifyStatus::Unverifiable,
            json!({"reason": "computed_not_a_source"}),
        ));
    }
    let q = normalise(&c.quote);
    if q.chars().count() < MIN_QUOTE_CHARS || q.split_whitespace().count() < MIN_QUOTE_WORDS {
        return Err(Verdict::new(
            VerifyStatus::Unverifiable,
            json!({"reason": "quote_too_short"}),
        ));
    }
    let tools = source_tools(c.source_kind);
    if tools.is_empty() {
        return Err(Verdict::new(
            VerifyStatus::Unverifiable,
            json!({"reason": "no_source_check"}),
        ));
    }
    let Some(n) = c.step_ordinal else {
        return Err(Verdict::new(
            VerifyStatus::Unverifiable,
            json!({"reason": "no_step"}),
        ));
    };
    let Some(step) = steps.iter().find(|s| s.ordinal == n) else {
        return Err(Verdict::new(
            VerifyStatus::QuoteMismatch,
            json!({"reason": "step_not_found", "step_ordinal": n}),
        ));
    };
    if !tools.contains(&step.name.as_str()) {
        return Err(Verdict::new(
            VerifyStatus::QuoteMismatch,
            json!({"reason": "wrong_step_kind", "step_ordinal": n, "step_tool": step.name, "expected_tool": tools.join("|")}),
        ));
    }
    if !locator_matches(c.source_kind, &c.locator, step) {
        // a paper missing from a cut result may have been in the part that was cut: say so, not "wrong paper"
        if c.source_kind == SourceKind::Literature && step.truncated {
            return Err(Verdict::new(
                VerifyStatus::Unverifiable,
                json!({"reason": "source_truncated", "step_ordinal": n,
                       "hint": "cite the paper's own research_literature_record step"}),
            ));
        }
        // a legal document cut to the cap is no longer JSON: its `url` cannot be read back
        if LEGAL_DOCUMENT_TOOLS.contains(&step.name.as_str()) && step.truncated {
            return Err(Verdict::new(
                VerifyStatus::Unverifiable,
                json!({"reason": "source_truncated", "step_ordinal": n,
                       "hint": "fetch the document again with a smaller max_chars"}),
            ));
        }
        return Err(Verdict::new(
            VerifyStatus::QuoteMismatch,
            json!({"reason": "locator_mismatch", "step_ordinal": n}),
        ));
    }
    if c.source_kind == SourceKind::Eli && !article_matches(&c.locator, &step.arguments) {
        return Err(Verdict::new(
            VerifyStatus::QuoteMismatch,
            json!({"reason": "article_mismatch", "step_ordinal": n}),
        ));
    }
    // strata: the quote must be in the cited chunk (or document) itself, not anywhere in the result;
    // literature: in the cited paper's own title or abstract
    let found = match c.source_kind {
        SourceKind::Strata => strata_texts_for(&step.text, c.locator.trim())
            .iter()
            .any(|t| normalise(t).contains(&q)),
        SourceKind::Literature => literature_records(step, &c.locator).iter().any(|r| {
            ["title", "abstract"]
                .iter()
                .any(|k| r[*k].as_str().is_some_and(|t| normalise(t).contains(&q)))
        }),
        // a legal document: in its plain `text`, not its metadata
        SourceKind::Web if LEGAL_DOCUMENT_TOOLS.contains(&step.name.as_str()) => {
            legal_document(step).is_some_and(|v| {
                v["text"]
                    .as_str()
                    .is_some_and(|t| normalise(t).contains(&q))
            })
        }
        _ => contains_normalised(&step.text, &q),
    };
    if !found {
        return Err(Verdict::new(
            VerifyStatus::QuoteMismatch,
            json!({"reason": "quote_not_found", "step_ordinal": n}),
        ));
    }
    Ok(match c.source_kind {
        SourceKind::Eli | SourceKind::Saos | SourceKind::Eurlex => None,
        _ => Some(Verdict::new(
            VerifyStatus::Verified,
            json!({"step_ordinal": n}),
        )),
    })
}

/// The register's answer as a verdict (the quote already matched).
pub fn verdict_of(r: &Resolution) -> Verdict {
    match r {
        Resolution::Found {
            in_force: Some(false),
        } => Verdict::new(VerifyStatus::NotInForce, json!({"reason": "not_in_force"})),
        Resolution::Found { in_force } => {
            Verdict::new(VerifyStatus::Verified, json!({"in_force": in_force}))
        }
        Resolution::Missing(m) => Verdict::new(
            VerifyStatus::SourceUnresolved,
            json!({"reason": "not_in_register", "error": m}),
        ),
        Resolution::Unavailable(m) => Verdict::new(
            VerifyStatus::Unverifiable,
            json!({"reason": "register_unavailable", "error": m}),
        ),
    }
}

/// `tenant`: the run's tenant, whose MCP credentials the register lookup uses.
pub async fn verify_citation<R: Resolver>(
    tenant: uuid::Uuid,
    c: &CitationIn,
    steps: &[ToolStepView],
    resolver: &R,
) -> Verdict {
    match verify_local(c, steps) {
        Err(v) => v,
        Ok(Some(v)) => v,
        Ok(None) => verdict_of(&resolver.resolve(tenant, c.source_kind, &c.locator).await),
    }
}

/// A claim with its citations and their verdicts, in citation order.
pub struct ClaimReport<'a> {
    pub text: &'a str,
    pub citations: Vec<(&'a CitationIn, &'a Verdict)>,
}

impl ClaimReport<'_> {
    pub fn supported(&self) -> bool {
        self.citations
            .iter()
            .any(|(_, v)| v.status == VerifyStatus::Verified)
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The final memo, built from the claims only (the model's free-text summary never reaches it): a title and the
/// disclaimer, the supported claims with their verified sources under "Findings", then every unsupported claim
/// under [`UNVERIFIED_HEADING`] with why.
pub fn build_report(legal: bool, claims: &[ClaimReport<'_>]) -> String {
    let mut out = String::new();
    out.push_str(if legal {
        LEGAL_REPORT_TITLE
    } else {
        RESEARCH_REPORT_TITLE
    });
    out.push_str("\n\n");
    out.push_str(if legal {
        LEGAL_DISCLAIMER
    } else {
        RESEARCH_DISCLAIMER
    });
    out.push_str("\n\n## Ustalenia / Findings\n\n");
    let mut sources: Vec<String> = vec![];
    let supported: Vec<&ClaimReport> = claims.iter().filter(|c| c.supported()).collect();
    if supported.is_empty() {
        out.push_str("_Brak zweryfikowanych ustaleń. / No verified findings._\n\n");
    } else {
        for (i, c) in supported.iter().enumerate() {
            let mut refs = vec![];
            for (cit, v) in &c.citations {
                if v.status != VerifyStatus::Verified {
                    continue;
                }
                let title = cit
                    .title
                    .as_deref()
                    .map(|t| format!(" — {}", one_line(t)))
                    .unwrap_or_default();
                let line = format!(
                    "{} `{}`{title}: „{}”",
                    cit.source_kind.as_str(),
                    one_line(&cit.locator),
                    one_line(&cit.quote)
                );
                let n = match sources.iter().position(|s| *s == line) {
                    Some(p) => p + 1,
                    None => {
                        sources.push(line);
                        sources.len()
                    }
                };
                refs.push(format!("[{n}]"));
            }
            out.push_str(&format!(
                "{}. {} {}\n",
                i + 1,
                one_line(c.text),
                refs.join("")
            ));
        }
        out.push_str("\n## Źródła / Sources\n\n");
        for (i, s) in sources.iter().enumerate() {
            out.push_str(&format!("[{}] {s}\n", i + 1));
        }
        out.push('\n');
    }
    let unsupported: Vec<&ClaimReport> = claims.iter().filter(|c| !c.supported()).collect();
    if !unsupported.is_empty() {
        out.push_str(UNVERIFIED_HEADING);
        out.push_str("\n\n");
        for c in unsupported {
            let reasons: Vec<String> = c
                .citations
                .iter()
                .map(|(cit, v)| {
                    let why = v.detail["reason"]
                        .as_str()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default();
                    format!(
                        "{} `{}`: {}{why}",
                        cit.source_kind.as_str(),
                        one_line(&cit.locator),
                        v.status.as_str()
                    )
                })
                .collect();
            out.push_str(&format!(
                "- {} — reason: {}\n",
                one_line(c.text),
                if reasons.is_empty() {
                    "no citation".to_string()
                } else {
                    reasons.join("; ")
                }
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeResolver(Resolution);
    impl Resolver for FakeResolver {
        async fn resolve(&self, _: uuid::Uuid, _: SourceKind, _: &str) -> Resolution {
            self.0.clone()
        }
    }

    fn step(ordinal: i32, name: &str, args: Value, text: &str) -> ToolStepView {
        ToolStepView {
            ordinal,
            name: name.into(),
            arguments: args,
            text: text.into(),
            truncated: false,
        }
    }

    fn cit(kind: SourceKind, locator: &str, quote: &str, step: Option<i32>) -> CitationIn {
        CitationIn {
            source_kind: kind,
            locator: locator.into(),
            title: None,
            quote: quote.into(),
            step_ordinal: step,
        }
    }

    const ELI_QUOTE: &str = "Ojciec nie może \"żądać\" zwrotu";
    const WEB_QUOTE: &str = "Termin wynosi 14 dni od doręczenia";
    const STRATA_QUOTE: &str = "Umowa zostaje zawarta na czas nieokreślony";

    fn steps() -> Vec<ToolStepView> {
        vec![
            step(
                1,
                "legal_eli_act_text",
                json!({"eli": "DU/1964/59", "article": "133"}),
                // JSON-escaped newline + nbsp + typographic quotes in the source text
                "{\"text\": \"Art. 133.\\n§ 1. Ojciec\u{00A0}nie może „żądać” zwrotu\"}",
            ),
            step(
                2,
                "legal_eli_act",
                json!({"eli": "DU/1964/59"}),
                "{\"eli\": \"DU/1964/59\", \"title\": \"Ojciec nie może \\\"żądać\\\" zwrotu\"}",
            ),
            step(
                3,
                "strata_search",
                json!({"query": "x"}),
                &crate::research::tools::format_strata_results(&json!({"results": [
                    {"chunk_id": "c-10", "document_id": "d-1", "title": "c-1", "source": "s",
                     "text": "Umowa zostaje zawarta na czas nieokreślony. c-1"},
                    {"chunk_id": "c-11", "document_id": "d-2", "title": "T2", "source": "s",
                     "text": "Wypowiedzenie wymaga formy pisemnej pod rygorem nieważności."}
                ]})),
            ),
            step(
                5,
                "web_scrape",
                json!({"url": "https://example.pl/a/"}),
                "Strona: Termin wynosi 14 dni od doręczenia. Ojciec nie może \"żądać\" zwrotu",
            ),
            step(
                6,
                "legal_saos_judgment",
                json!({"id": 12345}),
                "{\"id\": 12345, \"text\": \"Sąd Najwyższy uznał, że roszczenie jest zasadne.\"}",
            ),
            step(
                7,
                "legal_eurlex_text",
                json!({"celex": "32016r0679"}),
                "{\"celex\": \"32016R0679\", \"text\": \"Niniejsze rozporządzenie chroni podstawowe prawa.\"}",
            ),
            step(
                9,
                "legal_eli_act_text",
                json!({"eli": "DU/1964/59", "article": "art. 449^1"}),
                "{\"text\": \"Art. 449¹. § 1. Producent ponosi odpowiedzialność za szkodę.\"}",
            ),
            step(
                10,
                "legal_eli_act_text",
                json!({"eli": "DU/1964/59"}),
                "{\"text\": \"Art. 449¹. § 1. Producent ponosi odpowiedzialność za szkodę.\"}",
            ),
            step(
                8,
                "legal_krs_extract",
                json!({"krs": "12345"}),
                "{\"krs\": \"0000012345\", \"name\": \"Przykładowa Spółka z ograniczoną odpowiedzialnością\"}",
            ),
            step(
                11,
                LITERATURE_RECORD,
                json!({"id": PAPER_A}),
                &serde_json::to_string_pretty(&json!({
                    "id": PAPER_A, "title": "Exercise and depression in adults", "doi": "10.1000/ABC.1",
                    "pmid": "111", "abstract": "Aerobic exercise\nreduced depressive symptoms in adults."
                }))
                .unwrap(),
            ),
            step(
                12,
                LITERATURE_SEARCH,
                json!({"query": "exercise depression"}),
                &serde_json::to_string_pretty(&json!({"query": "exercise depression", "items": [
                    {"id": PAPER_A, "title": "Exercise and depression in adults", "doi": "10.1000/abc.1",
                     "pmid": "111", "abstract": "Aerobic exercise reduced depressive symptoms in adults."},
                    {"id": PAPER_B, "title": "Sleep and mood", "doi": "10.1000/xyz.2", "pmid": "222",
                     "abstract": "Short sleep predicted low mood in adolescents."}
                ]}))
                .unwrap(),
            ),
            step(
                13,
                "research_stats_meta_analysis",
                json!({"studies": []}),
                "{\"pooled_effect\": 0.42}",
            ),
        ]
    }

    const PAPER_A: &str = "0195f48b-d000-7000-8000-00000000000a";
    const PAPER_B: &str = "0195f48b-d000-7000-8000-00000000000b";
    const LIT_QUOTE: &str = "reduced depressive symptoms in adults";

    fn found() -> FakeResolver {
        FakeResolver(Resolution::Found {
            in_force: Some(true),
        })
    }

    async fn status(c: &CitationIn) -> (VerifyStatus, Value) {
        let v = verify_citation(uuid::Uuid::nil(), c, &steps(), &found()).await;
        (v.status, v.detail)
    }

    #[test]
    fn normalisation_unifies_only_whitespace_quotes_and_dashes() {
        assert_eq!(
            normalise("  a\u{00A0}\u{00A0}b\n\tc  „x” – y ‘z’ "),
            "a b c \"x\" - y 'z'"
        );
        assert_eq!(normalise("Ab"), "Ab");
        assert_ne!(normalise("ab"), normalise("Ab"), "case-sensitive");
        // nothing else is rewritten or dropped: ellipsis, ligatures, soft hyphens, punctuation stay
        assert_eq!(normalise("y…"), "y…");
        assert_eq!(normalise("\u{FB01}"), "\u{FB01}");
        assert_eq!(normalise("po\u{00AD}dział"), "po\u{00AD}dział");
        assert_eq!(normalise("a, b; c."), "a, b; c.");
    }

    #[tokio::test]
    async fn eli_quote_from_the_matching_act_text_step_is_verified() {
        let c = cit(SourceKind::Eli, "DU/1964/59 art. 133", ELI_QUOTE, Some(1));
        assert_eq!(status(&c).await.0, VerifyStatus::Verified);
        // register answers still apply after provenance
        let s = steps();
        let v = verify_citation(
            uuid::Uuid::nil(),
            &c,
            &s,
            &FakeResolver(Resolution::Found {
                in_force: Some(false),
            }),
        )
        .await;
        assert_eq!(v.status, VerifyStatus::NotInForce);
        let v = verify_citation(
            uuid::Uuid::nil(),
            &c,
            &s,
            &FakeResolver(Resolution::Missing("404".into())),
        )
        .await;
        assert_eq!(v.status, VerifyStatus::SourceUnresolved);
        let v = verify_citation(
            uuid::Uuid::nil(),
            &c,
            &s,
            &FakeResolver(Resolution::Unavailable("down".into())),
        )
        .await;
        assert_eq!(v.status, VerifyStatus::Unverifiable);
    }

    #[tokio::test]
    async fn a_quote_from_another_tool_step_does_not_count() {
        // the same words are in the web_scrape result (step 5): credited to an eli locator, rejected
        let (st, d) = status(&cit(SourceKind::Eli, "DU/1964/59", ELI_QUOTE, Some(5))).await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
        assert_eq!(d["reason"], "wrong_step_kind");
        // eli metadata (legal_eli_act) is not the act text
        let (st, d) = status(&cit(SourceKind::Eli, "DU/1964/59", ELI_QUOTE, Some(2))).await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
        assert_eq!(d["reason"], "wrong_step_kind");
        // the right tool, another act
        let (st, d) = status(&cit(
            SourceKind::Eli,
            "DU/1965/1 art. 1",
            ELI_QUOTE,
            Some(1),
        ))
        .await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
        assert_eq!(d["reason"], "locator_mismatch");
        // the right step, a paraphrase
        let (st, d) = status(&cit(
            SourceKind::Eli,
            "DU/1964/59",
            "Ojciec nie może żądać zapłaty",
            Some(1),
        ))
        .await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
        assert_eq!(d["reason"], "quote_not_found");
        // case matters
        let (st, _) = status(&cit(
            SourceKind::Eli,
            "DU/1964/59",
            "ojciec nie może \"żądać\" zwrotu",
            Some(1),
        ))
        .await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
        // a step that does not exist
        let (st, d) = status(&cit(SourceKind::Eli, "DU/1964/59", ELI_QUOTE, Some(42))).await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
        assert_eq!(d["reason"], "step_not_found");
    }

    #[tokio::test]
    async fn no_step_short_quotes_and_other_sources_are_unverifiable() {
        let (st, d) = status(&cit(
            SourceKind::Web,
            "https://example.pl/a",
            WEB_QUOTE,
            None,
        ))
        .await;
        assert_eq!(st, VerifyStatus::Unverifiable);
        assert_eq!(d["reason"], "no_step");
        let (st, d) = status(&cit(SourceKind::Other, "x", "T", None)).await;
        assert_eq!(st, VerifyStatus::Unverifiable);
        assert_eq!(d["reason"], "quote_too_short");
        // long enough in characters, too few words
        let (st, d) = status(&cit(
            SourceKind::Eli,
            "DU/1964/59",
            "Ojciec\u{00A0}nie-może-żądać-zwrotu",
            Some(1),
        ))
        .await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::Unverifiable, Some("quote_too_short"))
        );
        let (st, d) = status(&cit(SourceKind::Other, "x", " \u{00A0} ", Some(1))).await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::Unverifiable, Some("quote_too_short"))
        );
        // `other`: never verified, even with a real quote and step
        let (st, _) = status(&cit(SourceKind::Other, "x", WEB_QUOTE, Some(5))).await;
        assert_eq!(st, VerifyStatus::Unverifiable);
    }

    #[test]
    fn article_forms_normalise_like_the_eli_tool() {
        for a in ["449^1", "449(1)", "449_1", "449¹", "art. 449^1", "Art.449¹"] {
            assert_eq!(normalise_article(a).as_deref(), Some("449^1"), "{a}");
        }
        assert_eq!(normalise_article("133").as_deref(), Some("133"));
        assert_eq!(normalise_article("133a").as_deref(), Some("133a"));
        assert_eq!(normalise_article("4491").as_deref(), Some("4491"));
        assert!(normalise_article("x").is_none() && normalise_article("449^").is_none());
        assert!(article_matches("DU/1964/59", &json!({})));
        assert!(!article_matches("DU/1964/59 art. 133", &json!({})));
        assert!(article_matches(
            "DU/1964/59 art. 133 § 1",
            &json!({"article": "133"})
        ));
    }

    #[tokio::test]
    async fn an_eli_article_binds_to_a_step_for_that_article() {
        let q = "Producent ponosi odpowiedzialność za szkodę";
        for loc in [
            "DU/1964/59 art. 449¹",
            "DU/1964/59 art. 449(1) § 1",
            "DU/1964/59",
        ] {
            assert_eq!(
                status(&cit(SourceKind::Eli, loc, q, Some(9))).await.0,
                VerifyStatus::Verified,
                "{loc}"
            );
        }
        for (loc, step) in [
            ("DU/1964/59 art. 4491", 9),
            ("DU/1964/59 art. 449", 9),
            // the whole-act step does not bind an article citation
            ("DU/1964/59 art. 449^1", 10),
        ] {
            let (st, d) = status(&cit(SourceKind::Eli, loc, q, Some(step))).await;
            assert_eq!(
                (st, d["reason"].as_str()),
                (VerifyStatus::QuoteMismatch, Some("article_mismatch")),
                "{loc}"
            );
        }
        // an act citation without an article may come from the whole-act step
        assert_eq!(
            status(&cit(SourceKind::Eli, "DU/1964/59", q, Some(10)))
                .await
                .0,
            VerifyStatus::Verified
        );
    }

    #[tokio::test]
    async fn a_strata_quote_must_be_in_the_cited_chunk() {
        let other = "Wypowiedzenie wymaga formy pisemnej";
        assert_eq!(
            status(&cit(SourceKind::Strata, "c-11", other, Some(3)))
                .await
                .0,
            VerifyStatus::Verified
        );
        // in the same result, but in chunk c-11 / document d-2
        for loc in ["c-10", "d-1"] {
            let (st, d) = status(&cit(SourceKind::Strata, loc, other, Some(3))).await;
            assert_eq!(
                (st, d["reason"].as_str()),
                (VerifyStatus::QuoteMismatch, Some("quote_not_found")),
                "{loc}"
            );
        }
    }

    #[tokio::test]
    async fn strata_locator_must_be_one_of_the_result_ids_exactly() {
        let (st, _) = status(&cit(SourceKind::Strata, "c-10", STRATA_QUOTE, Some(3))).await;
        assert_eq!(st, VerifyStatus::Verified);
        let (st, _) = status(&cit(SourceKind::Strata, "d-1", STRATA_QUOTE, Some(3))).await;
        assert_eq!(st, VerifyStatus::Verified);
        // `c-1` is a prefix of `c-10` and occurs in the title and text: not an id
        let (st, d) = status(&cit(SourceKind::Strata, "c-1", STRATA_QUOTE, Some(3))).await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
        assert_eq!(d["reason"], "locator_mismatch");
    }

    #[tokio::test]
    async fn web_saos_eurlex_and_krs_match_their_call_arguments() {
        // trailing slash ignored, nothing else
        let (st, _) = status(&cit(
            SourceKind::Web,
            "https://example.pl/a",
            WEB_QUOTE,
            Some(5),
        ))
        .await;
        assert_eq!(st, VerifyStatus::Verified);
        let (st, d) = status(&cit(
            SourceKind::Web,
            "https://example.pl/ab",
            WEB_QUOTE,
            Some(5),
        ))
        .await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("locator_mismatch"))
        );
        let saos_q = "roszczenie jest zasadne";
        assert_eq!(
            status(&cit(SourceKind::Saos, "12345", saos_q, Some(6)))
                .await
                .0,
            VerifyStatus::Verified
        );
        assert_eq!(
            status(&cit(SourceKind::Saos, "1234", saos_q, Some(6)))
                .await
                .0,
            VerifyStatus::QuoteMismatch
        );
        let eu_q = "rozporządzenie chroni podstawowe prawa";
        assert_eq!(
            status(&cit(SourceKind::Eurlex, " 32016R0679 ", eu_q, Some(7)))
                .await
                .0,
            VerifyStatus::Verified
        );
        assert_eq!(
            status(&cit(SourceKind::Eurlex, "32016R0680", eu_q, Some(7)))
                .await
                .0,
            VerifyStatus::QuoteMismatch
        );
        let krs_q = "Przykładowa Spółka z ograniczoną";
        assert_eq!(
            status(&cit(SourceKind::Krs, "0000012345", krs_q, Some(8)))
                .await
                .0,
            VerifyStatus::Verified
        );
        assert_eq!(
            status(&cit(SourceKind::Krs, "0000012346", krs_q, Some(8)))
                .await
                .0,
            VerifyStatus::QuoteMismatch
        );
        assert_eq!(krs_padded(" 12345 ").as_deref(), Some("0000012345"));
        assert!(krs_padded("12a").is_none() && krs_padded("12345678901").is_none());
    }

    #[tokio::test]
    async fn a_literature_quote_from_the_cited_papers_record_or_search_result_is_verified() {
        let with_pmid = format!("{PAPER_A} pmid:111");
        for (loc, step) in [
            (PAPER_A, 11),
            (PAPER_A, 12),
            // uppercase uuid, doi in another case / as a URL, pmid: all the same paper
            ("0195F48B-D000-7000-8000-00000000000A doi:10.1000/abc.1", 11),
            ("https://doi.org/10.1000/ABC.1", 12),
            ("pmid:111", 12),
            (with_pmid.as_str(), 11),
        ] {
            assert_eq!(
                status(&cit(SourceKind::Literature, loc, LIT_QUOTE, Some(step)))
                    .await
                    .0,
                VerifyStatus::Verified,
                "{loc} @ {step}"
            );
        }
        // the title counts too
        let t = "Exercise and depression in adults";
        assert_eq!(
            status(&cit(SourceKind::Literature, PAPER_A, t, Some(12)))
                .await
                .0,
            VerifyStatus::Verified
        );
    }

    #[tokio::test]
    async fn a_literature_quote_from_a_different_paper_fails() {
        // the quote is in paper A's abstract in the same search result; credited to paper B it is rejected
        let (st, d) = status(&cit(SourceKind::Literature, PAPER_B, LIT_QUOTE, Some(12))).await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("quote_not_found"))
        );
        // paper B is not in the record step of paper A
        let (st, d) = status(&cit(SourceKind::Literature, PAPER_B, LIT_QUOTE, Some(11))).await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("locator_mismatch"))
        );
        // the same words in a web page do not make a literature citation
        let (st, d) = status(&cit(SourceKind::Literature, PAPER_A, LIT_QUOTE, Some(5))).await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("wrong_step_kind"))
        );
    }

    #[tokio::test]
    async fn a_literature_quote_not_in_the_abstract_fails() {
        let (st, d) = status(&cit(
            SourceKind::Literature,
            PAPER_A,
            "exercise cured depression in all adults",
            Some(11),
        ))
        .await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("quote_not_found"))
        );
        // a quote of a non-text field (the query echoed in the result) does not count either
        let (st, _) = status(&cit(
            SourceKind::Literature,
            PAPER_A,
            "exercise depression query words",
            Some(12),
        ))
        .await;
        assert_eq!(st, VerifyStatus::QuoteMismatch);
    }

    #[tokio::test]
    async fn a_literature_doi_or_pmid_that_does_not_match_the_paper_fails() {
        let id_doi = format!("{PAPER_A} doi:10.1000/xyz.2");
        let id_pmid = format!("{PAPER_A} pmid:222");
        let two_ids = format!("{PAPER_A} {PAPER_B}");
        for loc in [
            "doi:10.1000/abc.2",
            "10.1000/xyz.2 pmid:111",
            id_doi.as_str(),
            id_pmid.as_str(),
            // not a locator of this form
            "Exercise and depression",
            "pmid:11x",
            two_ids.as_str(),
        ] {
            let (st, d) = status(&cit(SourceKind::Literature, loc, LIT_QUOTE, Some(12))).await;
            assert_eq!(
                (st, d["reason"].as_str()),
                (VerifyStatus::QuoteMismatch, Some("locator_mismatch")),
                "{loc}"
            );
        }
    }

    #[tokio::test]
    async fn a_citation_into_an_over_cap_literature_search_is_checked_against_the_kept_items() {
        // 25 papers with ~4000-character abstracts: over the cap, so trailing papers are dropped whole
        let ids: Vec<String> = (0..25)
            .map(|i| format!("0195f48b-d000-7000-8000-{i:012}"))
            .collect();
        let items: Vec<Value> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                json!({"id": id, "title": format!("Paper {i}"), "doi": format!("10.1000/p.{i}"),
                       "abstract": format!("Paper {i} found that exercise reduced symptoms. {}", "z".repeat(4000))})
            })
            .collect();
        let raw = json!({"query": "q", "items": items}).to_string();
        let (text, truncated) = crate::research::cap_tool_text(LITERATURE_SEARCH, &raw);
        assert!(truncated);
        let mut search = step(1, LITERATURE_SEARCH, json!({"query": "q"}), &text);
        search.truncated = truncated;
        let steps = [search];
        // the first paper is kept: its quote verifies
        let v = verify_citation(
            uuid::Uuid::nil(),
            &cit(
                SourceKind::Literature,
                &ids[0],
                "Paper 0 found that exercise reduced symptoms",
                Some(1),
            ),
            &steps,
            &found(),
        )
        .await;
        assert_eq!(v.status, VerifyStatus::Verified, "{}", v.detail);
        // the last paper was dropped: a clear reason, not a locator mismatch
        let v = verify_citation(
            uuid::Uuid::nil(),
            &cit(
                SourceKind::Literature,
                &ids[24],
                "Paper 24 found that exercise reduced symptoms",
                Some(1),
            ),
            &steps,
            &found(),
        )
        .await;
        assert_eq!(
            (v.status, v.detail["reason"].as_str()),
            (VerifyStatus::Unverifiable, Some("source_truncated"))
        );
        // an untruncated step still says locator_mismatch for a paper it never had
        let (st, d) = status(&cit(SourceKind::Literature, &ids[24], LIT_QUOTE, Some(12))).await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("locator_mismatch"))
        );
    }

    const KIO_URL: &str = "https://orzeczenia.uzp.gov.pl/Home/Details/12345";
    const KIO_QUOTE: &str = "Izba uwzględnia odwołanie i nakazuje zamawiającemu";

    fn kio_steps() -> Vec<ToolStepView> {
        let doc = json!({
            "id": 12345,
            "signature": "KIO 1234/25",
            "date": "2025-06-02",
            "contracting_authority": "Gmina Wrocław ogłosiła przetarg nieograniczony",
            "text": "Wyrok. Izba\u{00A0}uwzględnia odwołanie i nakazuje zamawiającemu powtórzenie oceny ofert.",
            "url": KIO_URL,
            "truncated": false,
        });
        vec![
            step(
                20,
                "legal_kio_search",
                json!({"query": "odwołanie"}),
                &json!({"total": 1, "page": 1, "items": [{"id": 12345, "url": KIO_URL,
                    "title": KIO_QUOTE}]})
                .to_string(),
            ),
            step(
                21,
                "legal_kio_document",
                json!({"id": 12345}),
                &doc.to_string(),
            ),
        ]
    }

    async fn kio_status(c: &CitationIn) -> (VerifyStatus, Value) {
        let v = verify_citation(uuid::Uuid::nil(), c, &kio_steps(), &found()).await;
        (v.status, v.detail)
    }

    #[tokio::test]
    async fn a_kio_document_quote_with_its_result_url_is_verified() {
        let (st, _) = kio_status(&cit(SourceKind::Web, KIO_URL, KIO_QUOTE, Some(21))).await;
        assert_eq!(st, VerifyStatus::Verified);
        // trailing slash ignored, as for web_scrape
        let loc = format!("{KIO_URL}/");
        let (st, _) = kio_status(&cit(SourceKind::Web, &loc, KIO_QUOTE, Some(21))).await;
        assert_eq!(st, VerifyStatus::Verified);
    }

    #[tokio::test]
    async fn a_kio_document_with_a_different_url_fails() {
        let (st, d) = kio_status(&cit(
            SourceKind::Web,
            "https://orzeczenia.uzp.gov.pl/Home/Details/12346",
            KIO_QUOTE,
            Some(21),
        ))
        .await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("locator_mismatch"))
        );
    }

    #[tokio::test]
    async fn a_quote_not_in_the_kio_document_text_fails() {
        for q in [
            "Izba oddala odwołanie w całości",
            // in the result's metadata, but not in its `text`
            "Gmina Wrocław ogłosiła przetarg nieograniczony",
        ] {
            let (st, d) = kio_status(&cit(SourceKind::Web, KIO_URL, q, Some(21))).await;
            assert_eq!(
                (st, d["reason"].as_str()),
                (VerifyStatus::QuoteMismatch, Some("quote_not_found")),
                "{q}"
            );
        }
    }

    #[tokio::test]
    async fn a_quote_from_a_kio_search_step_fails() {
        let (st, d) = kio_status(&cit(SourceKind::Web, KIO_URL, KIO_QUOTE, Some(20))).await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::QuoteMismatch, Some("wrong_step_kind"))
        );
    }

    #[tokio::test]
    async fn a_truncated_kio_document_is_unverifiable() {
        let mut s = kio_steps();
        let cut: String = s[1].text.chars().take(40).collect();
        s[1].text = cut;
        s[1].truncated = true;
        let v = verify_citation(
            uuid::Uuid::nil(),
            &cit(SourceKind::Web, KIO_URL, KIO_QUOTE, Some(21)),
            &s,
            &found(),
        )
        .await;
        assert_eq!(
            (v.status, v.detail["reason"].as_str()),
            (VerifyStatus::Unverifiable, Some("source_truncated"))
        );
    }

    #[tokio::test]
    async fn stats_citations_are_never_verified() {
        let (st, d) = status(&cit(
            SourceKind::Stats,
            "research_stats_meta_analysis",
            "0.42",
            Some(13),
        ))
        .await;
        assert_eq!(
            (st, d["reason"].as_str()),
            (VerifyStatus::Unverifiable, Some("computed_not_a_source"))
        );
    }

    #[test]
    fn report_is_built_from_claims_only() {
        let a = cit(SourceKind::Eli, "DU/1964/59 art. 133", ELI_QUOTE, Some(1));
        let b = cit(SourceKind::Web, "https://x.pl", "zmyślone", Some(5));
        let ok = Verdict::new(VerifyStatus::Verified, json!({}));
        let bad = Verdict::new(
            VerifyStatus::QuoteMismatch,
            json!({"reason": "wrong_step_kind"}),
        );
        let claims = [
            ClaimReport {
                text: "Teza poparta.",
                citations: vec![(&a, &ok), (&b, &bad)],
            },
            ClaimReport {
                text: "Teza niepoparta.",
                citations: vec![(&b, &bad)],
            },
        ];
        assert!(claims[0].supported() && !claims[1].supported());
        let r = build_report(true, &claims);
        assert!(r.starts_with(LEGAL_REPORT_TITLE), "{r}");
        assert!(r.contains(LEGAL_DISCLAIMER));
        let (top, unverified) = r.split_once(UNVERIFIED_HEADING).expect("section");
        assert!(top.contains("## Ustalenia / Findings"), "{r}");
        assert!(top.contains("1. Teza poparta. [1]"), "{r}");
        assert!(top.contains("[1] eli `DU/1964/59 art. 133`"), "{r}");
        assert!(!top.contains("Teza niepoparta"), "{r}");
        assert!(
            unverified.contains(
                "Teza niepoparta. — reason: web `https://x.pl`: quote_mismatch (wrong_step_kind)"
            ),
            "{r}"
        );
        let plain = build_report(false, &claims[..1]);
        assert!(plain.starts_with(RESEARCH_REPORT_TITLE) && plain.contains(RESEARCH_DISCLAIMER));
        assert!(!plain.contains(UNVERIFIED_HEADING) && !plain.contains("pomocniczo"));
        let none = build_report(false, &claims[1..]);
        assert!(none.contains("No verified findings") && none.contains(UNVERIFIED_HEADING));
    }
}
