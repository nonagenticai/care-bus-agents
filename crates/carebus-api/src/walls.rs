//! Ethical walls on knowledge stores (migration 0026). A strata store attached to a legal matter is walled, and so
//! is everything below it in the tenant's strata tree (a query routed from a store searches its whole subtree):
//! - only runs filed under THAT matter may search it (routes::research);
//! - a store belongs to at most one matter, and attaching a store whose ancestors or descendants belong to another
//!   matter is refused (routes::legal_matters), so walls never nest across matters;
//! - the tenant-level knowledge routes (routes::knowledge, routes::knowledge_eval, routes::ingest) hide it from
//!   whoever is not a member of its matter, a tenant_admin included: 404, absent from lists, never searched.
//!
//! - the customer chat (`faq_ask`) searches only the leaves outside EVERY wall ([`chat_leaves`], served to the bus
//!   worker by `POST /api/internal/faq/scope`, routes::knowledge);
//! - sources writing into it, ingest jobs into it, its documents and eval runs over it are 404 / left out for
//!   non-members (routes::sources, routes::ingest, routes::knowledge, routes::knowledge_eval);
//! - the researcher re-checks a run's stores every time it claims it (research::runner, `store_walled`).
//!
//! Which stores are walled, and for whom, comes from `carebus_store_walls()` (tenant-wide, every job); the tree from
//! strata_rag's `GET /api/stores`. The tree is fetched only when the tenant has a wall the check needs.
use std::collections::{HashMap, HashSet};

use reqwest::Method;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::strata::{self, NAME};
use crate::tenancy::TenantCtx;
use crate::upstream::Reply;

/// One walled store: its matter, and whether the transaction's subject may see it (a member of that matter).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Wall {
    pub store_id: String,
    pub matter_id: Uuid,
    pub visible: bool,
}

/// Every walled store of the transaction's tenant, in every job.
pub async fn walls(conn: &mut PgConnection) -> ApiResult<Vec<Wall>> {
    Ok(
        sqlx::query_as("SELECT store_id, matter_id, visible FROM carebus_store_walls()")
            .fetch_all(&mut *conn)
            .await?,
    )
}

/// A store id in canonical (hyphenated, lower-case) form; anything that is not a uuid as given.
pub fn canonical(id: &str) -> String {
    id.trim()
        .parse::<Uuid>()
        .map(|u| u.hyphenated().to_string())
        .unwrap_or_else(|_| id.to_string())
}

#[derive(Debug, Default)]
struct Node {
    parent: Option<String>,
    children: Vec<String>,
    leaf: bool,
}

/// The tenant's strata store tree (`GET /api/stores`: `{stores: [{id, parent_id, kind, children}]}`).
#[derive(Debug, Default)]
pub struct Tree {
    nodes: HashMap<String, Node>,
}

impl Tree {
    pub fn from_stores(body: &Value) -> ApiResult<Self> {
        let stores = body["stores"].as_array().ok_or(ApiError::Upstream(NAME))?;
        let mut nodes = HashMap::new();
        for st in stores {
            let id = st["id"].as_str().ok_or(ApiError::Upstream(NAME))?;
            let children: Vec<String> = st["children"]
                .as_array()
                .map(|c| c.iter().filter_map(Value::as_str).map(canonical).collect())
                .unwrap_or_default();
            nodes.insert(
                canonical(id),
                Node {
                    parent: st["parent_id"].as_str().map(canonical),
                    children,
                    leaf: st["kind"] == "leaf",
                },
            );
        }
        // A child named only in its parent's `children` still knows its parent.
        let links: Vec<(String, String)> = nodes
            .iter()
            .flat_map(|(p, n)| n.children.iter().map(move |c| (c.clone(), p.clone())))
            .collect();
        for (c, p) in links {
            if let Some(n) = nodes.get_mut(&c)
                && n.parent.is_none()
            {
                n.parent = Some(p);
            }
        }
        Ok(Self { nodes })
    }

    /// The tenant's tree from strata_rag. Unconfigured strata_rag is an empty tree: nothing can be searched then (the
    /// researcher reads the same `STRATA_RAG_URL`), and the direct store checks still hold.
    pub async fn load(s: &AppState, tenant: Uuid) -> ApiResult<Self> {
        if s.cfg.strata_rag_url.is_none() {
            return Ok(Self::default());
        }
        let Reply { status, body } =
            strata::call(s, tenant, Method::GET, "/api/stores", None).await?;
        if !status.is_success() {
            return Err(strata::refusal(status, &body, "stores"));
        }
        Self::from_stores(&body)
    }

    /// `id` and every store below it (just `id` when the tree does not know it).
    pub fn subtree(&self, id: &str) -> HashSet<String> {
        let mut out = HashSet::from([id.to_string()]);
        let mut queue = vec![id.to_string()];
        while let Some(n) = queue.pop() {
            for c in self
                .nodes
                .get(&n)
                .map(|n| n.children.as_slice())
                .unwrap_or_default()
            {
                if out.insert(c.clone()) {
                    queue.push(c.clone());
                }
            }
        }
        out
    }

    /// The union of the subtrees of `ids`.
    pub fn closure(&self, ids: &[String]) -> HashSet<String> {
        ids.iter().flat_map(|i| self.subtree(i)).collect()
    }

    /// The stores above `id`, nearest first.
    pub fn ancestors(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        let mut at = self.nodes.get(id).and_then(|n| n.parent.clone());
        while let Some(p) = at {
            if p == id || out.contains(&p) {
                break;
            }
            at = self.nodes.get(&p).and_then(|n| n.parent.clone());
            out.push(p);
        }
        out
    }

    /// The stores without a parent.
    pub fn roots(&self) -> Vec<String> {
        let mut r: Vec<String> = self
            .nodes
            .iter()
            .filter(|(_, n)| {
                n.parent
                    .as_ref()
                    .is_none_or(|p| !self.nodes.contains_key(p))
            })
            .map(|(id, _)| id.clone())
            .collect();
        r.sort();
        r
    }

    fn is_leaf(&self, id: &str) -> bool {
        self.nodes.get(id).is_some_and(|n| n.leaf)
    }

    /// A leaf, or a store the tree does not know (it may hold documents).
    pub fn leaf_or_unknown(&self, id: &str) -> bool {
        self.nodes.get(id).is_none_or(|n| n.leaf)
    }
}

/// The stores the caller of a tenant-level request may not see: every store walled for a matter they are not a member
/// of, and everything below it. Empty (and no strata_rag call) when there is no such wall.
pub async fn hidden(s: &AppState, ctx: &TenantCtx) -> ApiResult<(HashSet<String>, Tree)> {
    let mut tx = ctx.begin(&s.db).await?;
    let ws = walls(&mut tx).await?;
    drop(tx);
    let blocked: Vec<String> = ws
        .into_iter()
        .filter(|w| !w.visible)
        .map(|w| w.store_id)
        .collect();
    if blocked.is_empty() {
        return Ok((HashSet::new(), Tree::default()));
    }
    let tree = Tree::load(s, ctx.tenant).await?;
    let hs = tree.closure(&blocked);
    Ok((hs, tree))
}

/// What the customer chat (`faq_ask` through the bus) may search in `tenant`. Chat has no subject and answers
/// customers, so EVERY walled store is out of its reach, with everything below it, whoever's matter it is:
/// - `None`: the tenant has no wall; strata_rag's default routing (the tenant's only root) is unchanged;
/// - `Some(leaves)`: the leaves of the tenant's only root outside every wall, sorted. Empty means nothing may be
///   searched (every leaf walled, several roots, or no strata_rag): the caller must not search at all.
pub async fn chat_leaves(s: &AppState, tenant: Uuid) -> ApiResult<Option<Vec<String>>> {
    let mut tx = crate::tenancy::begin_scoped(&s.db, tenant, None).await?;
    let ws = walls(&mut tx).await?;
    drop(tx);
    if ws.is_empty() {
        return Ok(None);
    }
    let blocked: Vec<String> = ws.into_iter().map(|w| w.store_id).collect();
    let tree = Tree::load(s, tenant).await?;
    Ok(Some(chat_leaves_of(&tree, &blocked)))
}

/// The leaves of `tree`'s only root outside the subtrees of `blocked`, sorted; empty unless there is exactly one root.
pub fn chat_leaves_of(tree: &Tree, blocked: &[String]) -> Vec<String> {
    let roots = tree.roots();
    if roots.len() != 1 {
        return vec![];
    }
    let hidden = tree.closure(blocked);
    let mut leaves: Vec<String> = tree
        .closure(&roots)
        .into_iter()
        .filter(|i| !hidden.contains(i) && tree.is_leaf(i))
        .collect();
    leaves.sort();
    leaves
}

/// Whether a search scoped to `root` (none: the tenant's roots) reaches a store in `hidden`.
pub fn reaches(tree: &Tree, hidden: &HashSet<String>, root: Option<&str>) -> bool {
    if hidden.is_empty() {
        return false;
    }
    let roots = match root {
        Some(r) => vec![canonical(r)],
        None => tree.roots(),
    };
    roots.iter().any(|r| hidden.contains(r)) || !tree.closure(&roots).is_disjoint(hidden)
}

fn store_not_found(id: &str) -> ApiError {
    ApiError::NotFound(format!("store {id}"))
}

/// The ids of a JSON value that is a string or an array of strings, canonical.
fn ids_of(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![canonical(s)],
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(canonical).collect(),
        _ => vec![],
    }
}

/// Confines a strata_rag `/api/query` body to what the caller may see (`hidden` non-empty):
/// - a hidden store named in `root_id` / `root_ids` / `leaf_ids` is 404, as if it did not exist;
/// - a routed query (`root_id` / `root_ids`, or none = the tenant's only root) whose subtrees reach a hidden store
///   becomes a `leaf_ids` query over the visible leaves of those subtrees (404 when there is none).
///
/// Several roots and nothing named is left to strata_rag (its 409 `ambiguous_root`), as is a body that is not an
/// object (its 4xx).
pub fn confine_query(tree: &Tree, hidden: &HashSet<String>, body: &mut Value) -> ApiResult<()> {
    if hidden.is_empty() {
        return Ok(());
    }
    let Some(o) = body.as_object_mut() else {
        return Ok(());
    };
    let mut named: Vec<String> = vec![];
    for k in ["root_id", "root_ids", "leaf_ids"] {
        named.extend(o.get(k).map(ids_of).unwrap_or_default());
    }
    if let Some(h) = named.iter().find(|i| hidden.contains(*i)) {
        return Err(store_not_found(h));
    }
    if o.contains_key("leaf_ids") {
        return Ok(());
    }
    let roots = if named.is_empty() {
        let r = tree.roots();
        if r.len() != 1 {
            return Ok(());
        }
        r
    } else {
        named
    };
    let reach = tree.closure(&roots);
    if reach.is_disjoint(hidden) {
        return Ok(());
    }
    let mut leaves: Vec<String> = reach
        .into_iter()
        .filter(|i| !hidden.contains(i) && tree.is_leaf(i))
        .collect();
    if leaves.is_empty() {
        return Err(ApiError::NotFound(
            "no store you may search under that store".into(),
        ));
    }
    leaves.sort();
    o.remove("root_id");
    o.remove("root_ids");
    o.insert("leaf_ids".into(), serde_json::json!(leaves));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const R: &str = "00000000-0000-4000-8000-0000000000a0";
    const A: &str = "00000000-0000-4000-8000-0000000000a1";
    const B: &str = "00000000-0000-4000-8000-0000000000a2";
    const S: &str = "00000000-0000-4000-8000-0000000000a3";
    const L: &str = "00000000-0000-4000-8000-0000000000a4";

    /// R (router) -> A (leaf), S (router) -> L (leaf); R -> B (leaf).
    fn tree() -> Tree {
        Tree::from_stores(&json!({"stores": [
            {"id": R, "parent_id": null, "kind": "router", "children": [A, B, S]},
            {"id": A, "parent_id": R, "kind": "leaf", "children": []},
            {"id": B, "parent_id": R, "kind": "leaf", "children": []},
            {"id": S, "parent_id": R, "kind": "router", "children": [L]},
            {"id": L, "parent_id": S, "kind": "leaf", "children": []},
        ]}))
        .unwrap()
    }

    #[test]
    fn subtrees_ancestors_and_roots() {
        let t = tree();
        assert_eq!(t.subtree(S), HashSet::from([S.to_string(), L.to_string()]));
        assert_eq!(t.subtree("unknown"), HashSet::from(["unknown".to_string()]));
        assert_eq!(t.ancestors(L), vec![S.to_string(), R.to_string()]);
        assert_eq!(t.roots(), vec![R.to_string()]);
    }

    #[test]
    fn chat_searches_only_leaves_outside_every_wall() {
        let t = tree();
        // a wall on the router S removes its leaf L; the other leaves stay
        assert_eq!(
            chat_leaves_of(&t, &[S.to_string()]),
            vec![A.to_string(), B.to_string()]
        );
        // a walled leaf goes alone
        assert_eq!(
            chat_leaves_of(&t, &[A.to_string()]),
            vec![B.to_string(), L.to_string()]
        );
        // everything walled, or no single root: nothing may be searched
        assert!(chat_leaves_of(&t, &[R.to_string()]).is_empty());
        assert!(chat_leaves_of(&Tree::default(), &[A.to_string()]).is_empty());
        let two_roots = Tree::from_stores(&json!({"stores": [
            {"id": A, "parent_id": null, "kind": "leaf", "children": []},
            {"id": B, "parent_id": null, "kind": "leaf", "children": []},
        ]}))
        .unwrap();
        assert!(chat_leaves_of(&two_roots, &[A.to_string()]).is_empty());
    }

    #[test]
    fn reach_of_a_scoped_search() {
        let t = tree();
        let hidden = t.closure(&[L.to_string()]);
        assert!(reaches(&t, &hidden, None));
        assert!(reaches(&t, &hidden, Some(R)));
        assert!(reaches(&t, &hidden, Some(&S.to_uppercase())));
        assert!(reaches(&t, &hidden, Some(L)));
        assert!(!reaches(&t, &hidden, Some(A)));
        assert!(!reaches(&t, &HashSet::new(), None));
    }

    #[test]
    fn a_query_never_reaches_a_hidden_store() {
        let t = tree();
        let hidden = t.closure(&[S.to_string()]);
        // naming a hidden store, in any form and case, is 404
        for b in [
            json!({"query": "q", "root_id": L.to_uppercase()}),
            json!({"query": "q", "root_ids": [B, S]}),
            json!({"query": "q", "leaf_ids": [L]}),
        ] {
            let mut b = b;
            assert!(matches!(
                confine_query(&t, &hidden, &mut b),
                Err(ApiError::NotFound(_))
            ));
        }
        // the default (only root) query becomes the visible leaves
        let mut b = json!({"query": "q", "route_mode": "hybrid"});
        confine_query(&t, &hidden, &mut b).unwrap();
        assert_eq!(b["leaf_ids"], json!([A, B]));
        let mut b = json!({"query": "q", "root_id": R});
        confine_query(&t, &hidden, &mut b).unwrap();
        assert_eq!(b, json!({"query": "q", "leaf_ids": [A, B]}));
        // a query that cannot reach a hidden store is untouched
        let mut b = json!({"query": "q", "root_ids": [A]});
        confine_query(&t, &hidden, &mut b).unwrap();
        assert_eq!(b, json!({"query": "q", "root_ids": [A]}));
        let mut b = json!({"query": "q", "leaf_ids": [A]});
        confine_query(&t, &hidden, &mut b).unwrap();
        assert_eq!(b, json!({"query": "q", "leaf_ids": [A]}));
        // nothing hidden: nothing changes
        let mut b = json!({"query": "q"});
        confine_query(&t, &HashSet::new(), &mut b).unwrap();
        assert_eq!(b, json!({"query": "q"}));
        // everything hidden: 404, never an unconfined query
        let all = t.closure(&[R.to_string()]);
        let mut b = json!({"query": "q"});
        assert!(confine_query(&t, &all, &mut b).is_err());
    }
}
