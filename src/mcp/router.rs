//! Tool routers: a name a model request uses like a gateway MCP server's, whose decision model
//! picks, per request, which of its routes the model gets the tools of. A route is a group of one
//! MCP server's tools, with a description of when they are needed. This keeps the model's tools
//! few when many servers are behind the gateway.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use uuid::Uuid;

use super::McpServer;
use crate::{model::VirtualKey, registry::Snapshot};

/// A tool router as stored.
#[derive(Debug, Clone)]
pub struct ToolRouterRow {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub decision_model: String,
    pub instructions: Option<String>,
    pub routes: Json<Vec<ToolRoute>>,
    pub threshold: f64,
    pub max_routes: Option<i32>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A group of one MCP server's tools, offered when the decision model finds them needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolRoute {
    /// Unique in its router: letters, digits, `-` and `_`.
    pub name: String,
    /// When the tools are needed, for the decision model ("questions about invoices and payments").
    pub description: String,
    /// The MCP server whose tools the route offers.
    pub server_id: Uuid,
    /// The server's tools it offers, after the server's own filters; empty for all of them.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// A tool router, ready to use.
#[derive(Debug, Clone)]
pub struct ToolRouter {
    pub id: Uuid,
    pub name: String,
    pub decision_model: String,
    pub instructions: Option<String>,
    pub routes: Vec<ToolRoute>,
    pub threshold: f64,
    pub max_routes: Option<usize>,
}

impl ToolRouter {
    pub fn build(row: ToolRouterRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            decision_model: row.decision_model,
            instructions: row.instructions.filter(|i| !i.trim().is_empty()),
            routes: row.routes.0,
            threshold: row.threshold,
            max_routes: row.max_routes.and_then(|m| usize::try_from(m).ok()),
        }
    }

    /// The routes `key` may use, each with its server: those whose server is enabled and open to the key.
    pub fn usable<'a>(&'a self, snap: &Snapshot, key: &VirtualKey) -> Vec<(&'a ToolRoute, Arc<McpServer>)> {
        self.routes
            .iter()
            .filter_map(|r| {
                let server = snap.mcp_servers.values().find(|s| s.id == r.server_id && s.allows(key))?;
                Some((r, server.clone()))
            })
            .collect()
    }

    /// The routes to offer, by how sure the decision model is that each is needed (`sure`, in
    /// the order of `routes`): those at or above the threshold, surest first, at most `max_routes`.
    pub fn pick<'a>(&self, routes: &[&'a ToolRoute], sure: &[f64]) -> Vec<&'a ToolRoute> {
        let mut picked: Vec<(&ToolRoute, f64)> =
            routes.iter().zip(sure).filter(|(_, p)| **p >= self.threshold).map(|(r, p)| (*r, *p)).collect();
        picked.sort_by(|a, b| b.1.total_cmp(&a.1));
        picked.truncate(self.max_routes.unwrap_or(usize::MAX));
        picked.into_iter().map(|(r, _)| r).collect()
    }
}

/// The tools of the picked routes by server: `None` for all of a server's tools.
pub fn merge<'a>(routes: impl IntoIterator<Item = (&'a ToolRoute, &'a str)>) -> BTreeMap<String, Option<Vec<String>>> {
    let mut by_server: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
    for (r, server) in routes {
        let entry = by_server.entry(server.to_string()).or_insert_with(|| Some(Vec::new()));
        match entry {
            // All of the server's tools already.
            None => {}
            Some(_) if r.tools.is_empty() => *entry = None,
            Some(tools) => {
                for t in &r.tools {
                    if !tools.contains(t) {
                        tools.push(t.clone());
                    }
                }
            }
        }
    }
    by_server
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(name: &str, tools: &[&str]) -> ToolRoute {
        ToolRoute {
            name: name.into(),
            description: String::new(),
            server_id: Uuid::nil(),
            tools: tools.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn routes_are_picked_by_threshold_surest_first() {
        let router = ToolRouter {
            id: Uuid::nil(),
            name: "r".into(),
            decision_model: "d".into(),
            instructions: None,
            routes: vec![],
            threshold: 0.5,
            max_routes: Some(2),
        };
        let (a, b, c, d) = (route("a", &[]), route("b", &[]), route("c", &[]), route("d", &[]));
        let picked = router.pick(&[&a, &b, &c, &d], &[0.6, 0.2, 0.9, 0.5]);
        assert_eq!(picked.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["c", "a"]);
        assert!(router.pick(&[&a, &b], &[0.1, 0.4]).is_empty());
    }

    #[test]
    fn routes_of_one_server_merge() {
        let (a, b, all) = (route("a", &["x", "y"]), route("b", &["y", "z"]), route("all", &[]));
        let merged = merge([(&a, "s"), (&b, "s")]);
        assert_eq!(merged["s"], Some(vec!["x".into(), "y".into(), "z".into()]));
        let merged = merge([(&a, "s"), (&all, "s"), (&b, "t")]);
        assert_eq!(merged["s"], None);
        assert_eq!(merged["t"], Some(vec!["y".into(), "z".into()]));
    }
}
