//! Block catalog: metadata the UI renders, plus factories that build blocks
//! from parameters.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::Error;
use crate::block::Block;
use crate::sample::StreamType;

/// Parameter values as given by a graph file or the UI.
pub type Params = serde_json::Map<String, Value>;

/// How the UI should present / validate a parameter.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamKind {
    Float {
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        /// Display unit, e.g. "Hz", "dB", "s". The UI adds SI prefixes.
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<&'static str>,
    },
    Int {
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    Bool,
    Enum {
        options: Vec<&'static str>,
    },
    String,
    /// A filesystem path (UI shows a file picker).
    Path,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParamSpec {
    pub name: &'static str,
    pub label: &'static str,
    #[serde(flatten)]
    pub kind: ParamKind,
    pub default: Value,
    pub help: &'static str,
}

/// Static description of a port for the palette. `None` = depends on params.
#[derive(Debug, Clone, Serialize)]
pub struct PortSpec {
    pub name: &'static str,
    #[serde(rename = "type")]
    pub ty: Option<StreamType>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockDescriptor {
    /// Stable machine id used in graph files, e.g. `signal_source`.
    pub kind: &'static str,
    pub label: &'static str,
    /// Palette category, e.g. "Sources".
    pub category: &'static str,
    pub help: &'static str,
    pub inputs: Vec<PortSpec>,
    pub outputs: Vec<PortSpec>,
    pub params: Vec<ParamSpec>,
}

/// A built block with its concrete port types.
pub struct Instance {
    pub block: Box<dyn Block>,
    pub inputs: Vec<(String, StreamType)>,
    pub outputs: Vec<(String, StreamType)>,
}

pub type Factory = fn(&ParamReader<'_>) -> Result<Instance, Error>;

struct Entry {
    descriptor: BlockDescriptor,
    factory: Factory,
}

#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<&'static str, Entry>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, descriptor: BlockDescriptor, factory: Factory) {
        let kind = descriptor.kind;
        let prev = self.entries.insert(
            kind,
            Entry {
                descriptor,
                factory,
            },
        );
        assert!(prev.is_none(), "block kind `{kind}` registered twice");
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &BlockDescriptor> {
        self.entries.values().map(|e| &e.descriptor)
    }

    pub fn get(&self, kind: &str) -> Option<&BlockDescriptor> {
        self.entries.get(kind).map(|e| &e.descriptor)
    }

    /// Build a block of `kind`; missing params fall back to their defaults.
    pub fn build(&self, kind: &str, params: &Params) -> Result<Instance, Error> {
        let e = self
            .entries
            .get(kind)
            .ok_or_else(|| Error::UnknownBlock(kind.to_owned()))?;
        (e.factory)(&ParamReader {
            spec: &e.descriptor,
            values: params,
        })
    }
}

/// Typed access to parameters with defaults applied from the descriptor.
pub struct ParamReader<'a> {
    spec: &'a BlockDescriptor,
    values: &'a Params,
}

impl ParamReader<'_> {
    fn raw(&self, name: &str) -> Result<&Value, Error> {
        if let Some(v) = self.values.get(name) {
            return Ok(v);
        }
        self.spec
            .params
            .iter()
            .find(|p| p.name == name)
            .map(|p| &p.default)
            .ok_or_else(|| self.err(name, "unknown parameter"))
    }

    fn err(&self, name: &str, msg: &str) -> Error {
        Error::Param {
            block: self.spec.kind.to_owned(),
            param: name.to_owned(),
            msg: msg.to_owned(),
        }
    }

    pub fn f64(&self, name: &str) -> Result<f64, Error> {
        self.raw(name)?
            .as_f64()
            .ok_or_else(|| self.err(name, "expected a number"))
    }

    pub fn u64(&self, name: &str) -> Result<u64, Error> {
        self.raw(name)?
            .as_u64()
            .ok_or_else(|| self.err(name, "expected a non-negative integer"))
    }

    pub fn bool(&self, name: &str) -> Result<bool, Error> {
        self.raw(name)?
            .as_bool()
            .ok_or_else(|| self.err(name, "expected true/false"))
    }

    pub fn str(&self, name: &str) -> Result<&str, Error> {
        self.raw(name)?
            .as_str()
            .ok_or_else(|| self.err(name, "expected a string"))
    }

    pub fn stream_type(&self, name: &str) -> Result<StreamType, Error> {
        serde_json::from_value(self.raw(name)?.clone())
            .map_err(|_| self.err(name, "expected one of f32, c32, i16, u8"))
    }
}
