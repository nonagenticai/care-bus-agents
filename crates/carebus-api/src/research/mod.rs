//! Plan 18: long-running research / Polish legal-research runs (migration 0020).
//!
//! The `researcher` bin claims a queued run (`carebus_research_claim`), drives a model-chosen tool loop over an
//! OpenAI-compatible chat endpoint ([`llm`]), checkpoints every model turn and every tool call as one
//! `research_steps` row before the next action ([`runner`]), and ends when the model calls `submit_report`. The
//! citations are then checked by deterministic code ([`verify`]), never by a model, and the final memo keeps
//! unverified claims apart. Every read and write of a run goes through `tenancy::begin_scoped` (RLS).
use serde_json::Value;

pub mod llm;
pub mod prompts;
pub mod runner;
pub mod store;
pub mod tools;
pub mod verify;

/// Tool results are kept (and shown to the model) up to this many characters.
pub const TOOL_TEXT_CAP: usize = 40_000;

/// MCP tools the `legal` profile may call (contract with nonagenticai_mcp_server).
pub const LEGAL_TOOLS: [&str; 16] = [
    "legal_eli_search",
    "legal_eli_act",
    "legal_eli_act_text",
    "legal_saos_search",
    "legal_saos_judgment",
    "legal_eurlex_act",
    "legal_eurlex_text",
    "legal_krs_extract",
    "legal_vat_whitelist",
    "legal_kio_search",
    "legal_kio_document",
    "legal_uokik_search",
    "legal_uokik_document",
    "legal_duw_search",
    "legal_duw_document",
    "web_scrape",
];

/// MCP tools the generic `research` profile may call (literature: discovery-engine; stats: scientific-processor).
pub const RESEARCH_TOOLS: [&str; 6] = [
    "web_scrape",
    "research_literature_search",
    "research_literature_record",
    "research_stats_meta_analysis",
    "research_stats_confidence_interval",
    "research_stats_trend",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Research,
    Legal,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "research" => Some(Self::Research),
            "legal" => Some(Self::Legal),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Legal => "legal",
        }
    }
    /// The MCP tools this profile may be offered (those absent from the live `tools/list` are not offered).
    pub fn mcp_allow_list(self) -> &'static [&'static str] {
        match self {
            Self::Research => &RESEARCH_TOOLS,
            Self::Legal => &LEGAL_TOOLS,
        }
    }
    pub fn system_prompt(self) -> &'static str {
        match self {
            Self::Research => prompts::RESEARCH_SYSTEM,
            Self::Legal => prompts::LEGAL_SYSTEM,
        }
    }
}

/// `research_citations.source_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Strata,
    Web,
    Eli,
    Saos,
    Eurlex,
    Krs,
    Literature,
    Stats,
    Other,
}

impl SourceKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "strata" => Self::Strata,
            "web" => Self::Web,
            "eli" => Self::Eli,
            "saos" => Self::Saos,
            "eurlex" => Self::Eurlex,
            "krs" => Self::Krs,
            "literature" => Self::Literature,
            "stats" => Self::Stats,
            "other" => Self::Other,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strata => "strata",
            Self::Web => "web",
            Self::Eli => "eli",
            Self::Saos => "saos",
            Self::Eurlex => "eurlex",
            Self::Krs => "krs",
            Self::Literature => "literature",
            Self::Stats => "stats",
            Self::Other => "other",
        }
    }
}

/// A run's limits (`research_runs.budget`); missing or invalid keys take the defaults. They bound the research phase:
/// once one is reached the run gets up to `runner::WRITE_TURNS` forced write turns beyond it (model calls offering
/// only `submit_report`), so `steps_used` can end at `max_steps + WRITE_TURNS` and tokens / wall time past theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Model turns.
    pub max_steps: u32,
    pub max_tool_calls: u32,
    /// Since the run first started (`started_at`), across restarts.
    pub max_wall_s: u64,
    /// Sum of the provider-reported `total_tokens`.
    pub max_tokens: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_steps: 40,
            max_tool_calls: 80,
            max_wall_s: 1800,
            max_tokens: 400_000,
        }
    }
}

impl Budget {
    pub fn from_json(v: &Value) -> Self {
        let d = Self::default();
        let get = |k: &str| v.get(k).and_then(Value::as_u64).filter(|n| *n > 0);
        Self {
            max_steps: get("max_steps")
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(d.max_steps),
            max_tool_calls: get("max_tool_calls")
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(d.max_tool_calls),
            max_wall_s: get("max_wall_s").unwrap_or(d.max_wall_s),
            max_tokens: get("max_tokens").unwrap_or(d.max_tokens),
        }
    }
}

/// `text` cut to [`TOOL_TEXT_CAP`] characters; the flag says whether anything was cut.
pub fn cap_text(text: &str) -> (String, bool) {
    match text.char_indices().nth(TOOL_TEXT_CAP) {
        None => (text.to_string(), false),
        Some((i, _)) => (text[..i].to_string(), true),
    }
}

/// The fields of a `research_literature_search` item that are kept (all a `literature` citation is checked against).
pub const LITERATURE_ITEM_FIELDS: [&str; 6] = ["id", "doi", "pmid", "title", "abstract", "url"];

/// A tool result as stored (and shown to the model): [`cap_text`], except for a `research_literature_search` result
/// whose JSON has an `items` array. That one is compacted (each item keeps only [`LITERATURE_ITEM_FIELDS`]) and, if
/// still over [`TOOL_TEXT_CAP`], loses whole trailing items (counted in `omitted_items`) so it stays valid JSON that
/// `verify::literature_records` can read. A cut mid-string would make every citation of the step unverifiable.
pub fn cap_tool_text(tool: &str, text: &str) -> (String, bool) {
    if tool != verify::LITERATURE_SEARCH {
        return cap_text(text);
    }
    let Ok(Value::Object(mut top)) = serde_json::from_str::<Value>(text) else {
        return cap_text(text);
    };
    let Some(Value::Array(items)) = top.remove("items") else {
        return cap_text(text);
    };
    let mut items: Vec<Value> = items
        .into_iter()
        .map(|it| match it {
            Value::Object(o) => Value::Object(
                o.into_iter()
                    .filter(|(k, _)| LITERATURE_ITEM_FIELDS.contains(&k.as_str()))
                    .collect(),
            ),
            other => other,
        })
        .collect();
    let mut omitted = 0usize;
    loop {
        let mut out = top.clone();
        out.insert("items".into(), Value::Array(items.clone()));
        if omitted > 0 {
            out.insert("omitted_items".into(), Value::from(omitted));
        }
        let s = Value::Object(out).to_string();
        if s.chars().count() <= TOOL_TEXT_CAP {
            return (s, omitted > 0);
        }
        if items.pop().is_none() {
            // not the items: something else in the result is too long
            return cap_text(text);
        }
        omitted += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_defaults_fill_missing_and_invalid_keys() {
        assert_eq!(Budget::from_json(&json!({})), Budget::default());
        let b = Budget::from_json(&json!({"max_steps": 3, "max_tokens": "x", "max_wall_s": 0}));
        assert_eq!(b.max_steps, 3);
        assert_eq!(b.max_tokens, 400_000);
        assert_eq!(b.max_wall_s, 1800);
    }

    #[test]
    fn cap_is_by_characters() {
        let s = "ą".repeat(TOOL_TEXT_CAP + 5);
        let (c, t) = cap_text(&s);
        assert!(t);
        assert_eq!(c.chars().count(), TOOL_TEXT_CAP);
        assert_eq!(cap_text("abc"), ("abc".into(), false));
    }

    fn search_result(n: usize, abstract_len: usize) -> String {
        let items: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "id": format!("0195f48b-d000-7000-8000-{i:012}"),
                    "doi": format!("10.1000/p.{i}"),
                    "pmid": (1000 + i).to_string(),
                    "title": format!("Paper {i}"),
                    "abstract": format!("Finding {i}: {}", "x".repeat(abstract_len)),
                    "url": format!("https://example.org/{i}"),
                    "authors": ["A", "B"],
                    "venue": "J",
                    "references": "r".repeat(500),
                })
            })
            .collect();
        json!({"query": "q", "items": items}).to_string()
    }

    #[test]
    fn an_over_cap_literature_search_keeps_valid_json_and_the_leading_items() {
        // 25 items x ~4000-char abstracts: about 100k characters, over the 40k cap
        let raw = search_result(25, 4000);
        assert!(raw.chars().count() > TOOL_TEXT_CAP);
        let (text, truncated) = cap_tool_text(verify::LITERATURE_SEARCH, &raw);
        assert!(truncated);
        assert!(text.chars().count() <= TOOL_TEXT_CAP);
        let v: Value = serde_json::from_str(&text).expect("still valid JSON");
        let items = v["items"].as_array().unwrap();
        assert!(!items.is_empty() && items.len() < 25, "{}", items.len());
        assert_eq!(v["omitted_items"], 25 - items.len());
        assert_eq!(v["query"], "q");
        for (i, it) in items.iter().enumerate() {
            // the leading items, in order, with only the citation fields
            assert_eq!(it["title"], format!("Paper {i}"));
            let mut keys: Vec<&str> = it.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["abstract", "doi", "id", "pmid", "title", "url"]);
        }
    }

    #[test]
    fn a_literature_search_under_the_cap_is_compacted_but_not_truncated() {
        let (text, truncated) = cap_tool_text(verify::LITERATURE_SEARCH, &search_result(3, 100));
        assert!(!truncated);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 3);
        assert!(v.get("omitted_items").is_none());
        assert!(v["items"][0].get("references").is_none());
        // other tools and non-JSON search results keep the plain character cap
        let long = "y".repeat(TOOL_TEXT_CAP + 1);
        assert_eq!(cap_tool_text("web_scrape", &long), cap_text(&long));
        assert_eq!(
            cap_tool_text(verify::LITERATURE_SEARCH, &long),
            cap_text(&long)
        );
        assert_eq!(
            cap_tool_text("web_scrape", "{\"items\": []}").0,
            "{\"items\": []}"
        );
    }

    #[test]
    fn kinds_round_trip() {
        for k in [
            "strata",
            "web",
            "eli",
            "saos",
            "eurlex",
            "krs",
            "literature",
            "stats",
            "other",
        ] {
            assert_eq!(SourceKind::parse(k).unwrap().as_str(), k);
        }
        assert!(SourceKind::parse("lex").is_none());
        assert_eq!(Profile::parse("legal"), Some(Profile::Legal));
        assert!(Profile::Legal.mcp_allow_list().contains(&"legal_eli_act"));
    }
}
