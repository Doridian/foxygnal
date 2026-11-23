//! Graph files used as blocks: loading them from the project directory (or
//! the bundled library), describing them for the palette, and mapping the
//! flattened graph the engine runs back to the graph the user drew.

use std::collections::HashMap;
use std::path::PathBuf;

use foxygnal_core::resolve::{
    GRAPH_EXT, GraphDescriptor, GraphLoader, Resolved, check_graph_name, describe, split_id,
};
use foxygnal_core::{EdgeMeta, Error, GraphSpec, Registry};

use crate::probe::Stats;
use crate::session::{EdgeRef, ErrorInfo};

/// Sub-graphs bundled with the examples: available to every project, unless
/// the project has a file of the same name.
pub(crate) const LIBRARY: &[(&str, &str)] = &[(
    "tone-and-noise.fox.toml",
    include_str!("../../../examples/tone-and-noise.fox.toml"),
)];

pub(crate) fn library(name: &str) -> Option<&'static str> {
    LIBRARY.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// Loads `graph:NAME.fox.toml` from the project directory, then the library.
pub struct ProjectGraphs {
    pub dir: PathBuf,
}

impl GraphLoader for ProjectGraphs {
    fn load(&self, name: &str) -> Result<GraphSpec, Error> {
        check_graph_name(name)?;
        let text = match std::fs::read_to_string(self.dir.join(name)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => library(name)
                .ok_or_else(|| {
                    Error::Other(format!("there is no `{name}` in the project directory"))
                })?
                .to_owned(),
            Err(e) => return Err(Error::Other(format!("can't read `{name}`: {e}"))),
        };
        GraphSpec::from_toml(&text).map_err(|e| Error::Other(format!("`{name}`: {e}")))
    }
}

impl ProjectGraphs {
    /// Descriptors of every graph file that exports ports. Files that don't
    /// resolve are left out (opening them shows why).
    pub fn descriptors(&self, registry: &Registry) -> Vec<GraphDescriptor> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(GRAPH_EXT) && check_graph_name(n).is_ok())
            .collect();
        for (n, _) in LIBRARY {
            if !names.iter().any(|x| x == n) {
                names.push((*n).to_owned());
            }
        }
        names.sort();
        names
            .iter()
            .filter_map(|n| match describe(n, registry, self) {
                Ok(d) => d,
                Err(e) => {
                    tracing::debug!("{n} can't be used as a block: {e}");
                    None
                }
            })
            .collect()
    }
}

/// Maps wires and blocks of the flattened graph (`fm/lp`) back to the graph
/// as drawn, where `fm` is one hierarchical block.
#[derive(Debug, Default)]
pub struct EdgeMap {
    /// Flattened `(from, to)` (as written, and with port names) → as drawn.
    up: HashMap<(String, String), (String, String)>,
    /// As drawn → the flattened output endpoint feeding it.
    source: HashMap<(String, String), String>,
}

fn has_hier(ep: &str) -> bool {
    ep.contains('/')
}

impl EdgeMap {
    /// The connections of `r` as written.
    pub fn new(r: &Resolved) -> Self {
        let mut m = Self::default();
        for (c, o) in r.graph.connections.iter().zip(&r.origins) {
            if let Some(o) = o {
                m.insert(
                    (c.from.clone(), c.to.clone()),
                    (o.from.clone(), o.to.clone()),
                );
            }
        }
        m
    }

    fn insert(&mut self, flat: (String, String), drawn: (String, String)) {
        self.source.entry(drawn.clone()).or_insert(flat.0.clone());
        self.up.entry(flat).or_insert(drawn);
    }

    /// Add the connections with port names, as configure reports them
    /// (`edges` comes in the order of `r.graph.connections`).
    pub fn add_meta(&mut self, r: &Resolved, edges: &[EdgeMeta]) {
        debug_assert_eq!(edges.len(), r.graph.connections.len());
        for ((c, o), e) in r.graph.connections.iter().zip(&r.origins).zip(edges) {
            let Some(o) = o else { continue };
            // A side that isn't a hierarchical port keeps its port name.
            let from = if o.from == c.from {
                e.from.clone()
            } else {
                o.from.clone()
            };
            let to = if o.to == c.to {
                e.to.clone()
            } else {
                o.to.clone()
            };
            self.insert((e.from.clone(), e.to.clone()), (from, to));
        }
    }

    /// The wire as drawn, or `None` for a wire inside a hierarchical block.
    pub fn edge(&self, from: &str, to: &str) -> Option<(String, String)> {
        if let Some(d) = self.up.get(&(from.to_owned(), to.to_owned())) {
            return Some(d.clone());
        }
        (!has_hier(from) && !has_hier(to)).then(|| (from.to_owned(), to.to_owned()))
    }

    /// The output endpoint in the running graph that feeds wire `e`.
    pub fn source<'a>(&'a self, e: &'a EdgeRef) -> &'a str {
        self.source
            .get(&(e.from.clone(), e.to.clone()))
            .map_or(&e.from, String::as_str)
    }

    /// An error about the flattened graph, pinned to what the user sees.
    pub fn error(&self, e: &Error) -> ErrorInfo {
        let mut info = ErrorInfo::from(e);
        if let Some(edge) = info.edge.take() {
            if let Some((from, to)) = self.edge(&edge.from, &edge.to) {
                info.edge = Some(EdgeRef { from, to });
            } else {
                // A wire inside a hierarchical block: blame the block.
                let inner = if has_hier(&edge.from) {
                    &edge.from
                } else {
                    &edge.to
                };
                let (top, _) = split_id(inner);
                let strip = |s: &str| s.strip_prefix(&format!("{top}/")).unwrap_or(s).to_owned();
                info.message = format!(
                    "{} → {}: {}",
                    strip(&edge.from),
                    strip(&edge.to),
                    info.message
                );
                info.block = Some(top.to_owned());
            }
        }
        if let Some(block) = info.block.take() {
            let (top, inner) = split_id(&block);
            if let Some(inner) = inner {
                info.message = format!("{inner}: {}", info.message);
            }
            info.block = Some(top.to_owned());
        }
        info
    }

    /// Edge metadata as drawn: wires inside hierarchical blocks left out.
    pub fn meta(&self, edges: Vec<EdgeMeta>) -> Vec<EdgeMeta> {
        let mut out: Vec<EdgeMeta> = Vec::with_capacity(edges.len());
        for mut e in edges {
            let Some((from, to)) = self.edge(&e.from, &e.to) else {
                continue;
            };
            if out.iter().any(|o| o.from == from && o.to == to) {
                continue;
            }
            e.from = from;
            e.to = to;
            out.push(e);
        }
        out
    }

    /// Stats as drawn: a hierarchical block's CPU is the sum of its blocks'.
    pub fn stats(&self, s: Stats) -> Stats {
        let mut blocks: Vec<crate::probe::BlockStat> = Vec::with_capacity(s.blocks.len());
        for mut b in s.blocks {
            let top = split_id(&b.block).0.to_owned();
            match blocks.iter_mut().find(|x| x.block == top) {
                Some(x) => x.cpu += b.cpu,
                None => {
                    b.block = top;
                    blocks.push(b);
                }
            }
        }
        let mut edges: Vec<crate::probe::EdgeStat> = Vec::with_capacity(s.edges.len());
        for mut e in s.edges {
            let Some((from, to)) = self.edge(&e.from, &e.to) else {
                continue;
            };
            // An exported input feeding several inner blocks: the fullest.
            match edges.iter_mut().find(|x| x.from == from && x.to == to) {
                Some(x) => x.fill = x.fill.max(e.fill),
                None => {
                    e.from = from;
                    e.to = to;
                    edges.push(e);
                }
            }
        }
        Stats { blocks, edges }
    }
}
