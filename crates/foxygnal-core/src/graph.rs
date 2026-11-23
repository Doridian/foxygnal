//! Serializable flowgraph description (what the UI edits and `.fox.toml` stores).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::registry::Params;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphSpec {
    /// Graph variables (`[vars]`): a number or an expression (see
    /// [`crate::expr`]), referenced from params as `"$name"`. In a graph used
    /// as a block, they are its parameters. See [`crate::resolve`].
    #[serde(default, skip_serializing_if = "Params::is_empty")]
    pub vars: Params,
    /// Ports this graph exposes when used as a block in another graph.
    #[serde(default, skip_serializing_if = "Exports::is_empty")]
    pub export: Exports,
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

/// `[export]`: what a graph looks like when used as a hierarchical block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Exports {
    /// Palette label (default: the file name without `.fox.toml`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Help text shown in the palette and inspector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<ExportInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<ExportOutput>,
}

impl Exports {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the graph exposes any port, i.e. can be used as a block.
    pub fn has_ports(&self) -> bool {
        !self.input.is_empty() || !self.output.is_empty()
    }
}

/// `[[export.input]] name = "in", to = "lp.in"`: an input port of the
/// hierarchical block, feeding one inner input (or several: `to = [...]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportInput {
    pub name: String,
    pub to: Targets,
}

/// `[[export.output]] name = "out", from = "demod.out"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportOutput {
    pub name: String,
    pub from: String,
}

/// One `block.port` endpoint, or several.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Targets {
    One(String),
    Many(Vec<String>),
}

impl Targets {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        let s: &[String] = match self {
            Targets::One(s) => std::slice::from_ref(s),
            Targets::Many(v) => v,
        };
        s.iter().map(String::as_str)
    }
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

pub fn parse_endpoint(s: &str) -> Result<Endpoint<'_>, Error> {
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
