//! Serializable flowgraph description (what the UI edits and `.fox.toml` stores).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::registry::Params;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphSpec {
    #[serde(default, rename = "block")]
    pub blocks: Vec<BlockSpec>,
    #[serde(default, rename = "connect")]
    pub connections: Vec<ConnectionSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockSpec {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub params: Params,
    /// Editor-only metadata (canvas position, colour, notes). Ignored by the runtime.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ui: BTreeMap<String, serde_json::Value>,
}

/// `from = "src.out"`, `to = "sink.in"`; the port part may be a name or an index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionSpec {
    pub from: String,
    pub to: String,
}

/// A parsed `block.port` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint<'a> {
    pub block: &'a str,
    pub port: &'a str,
}

impl ConnectionSpec {
    pub fn endpoints(&self) -> Result<(Endpoint<'_>, Endpoint<'_>), Error> {
        Ok((parse_endpoint(&self.from)?, parse_endpoint(&self.to)?))
    }
}

fn parse_endpoint(s: &str) -> Result<Endpoint<'_>, Error> {
    let (block, port) = s
        .rsplit_once('.')
        .ok_or_else(|| Error::Graph(format!("endpoint `{s}` must look like `block.port`")))?;
    Ok(Endpoint { block, port })
}

impl GraphSpec {
    pub fn from_toml(s: &str) -> Result<Self, Error> {
        toml::from_str(s).map_err(|e| Error::Graph(e.to_string()))
    }

    pub fn to_toml(&self) -> Result<String, Error> {
        toml::to_string_pretty(self).map_err(|e| Error::Graph(e.to_string()))
    }
}
