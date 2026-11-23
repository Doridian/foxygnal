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
    /// An SDR device string (`rtlsdr:serial=…`, empty = first found). The UI
    /// offers the devices discovery finds.
    Device,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParamSpec {
    pub name: &'static str,
    pub label: &'static str,
    #[serde(flatten)]
    pub kind: ParamKind,
    pub default: Value,
    pub help: &'static str,
    /// Can change while the graph runs (delivered via [`Block::set_param`]).
    /// Other parameter changes need a restart.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,
}

impl ParamSpec {
    /// Mark this parameter as changeable while running.
    pub fn live(mut self) -> Self {
        self.live = true;
        self
    }

    /// Check that `value` has the right JSON shape for this parameter.
    pub fn check(&self, value: &Value) -> Result<(), String> {
        let ok = match &self.kind {
            ParamKind::Float { min, max, .. } => match value.as_f64() {
                Some(v) if min.is_some_and(|m| v < m) => {
                    return Err(format!("must be at least {}", min.unwrap_or_default()));
                }
                Some(v) if max.is_some_and(|m| v > m) => {
                    return Err(format!("must be at most {}", max.unwrap_or_default()));
                }
                Some(v) => v.is_finite(),
                None => false,
            },
            ParamKind::Int { min, max } => match value.as_i64() {
                Some(v) if min.is_some_and(|m| v < m) => {
                    return Err(format!("must be at least {}", min.unwrap_or_default()));
                }
                Some(v) if max.is_some_and(|m| v > m) => {
                    return Err(format!("must be at most {}", max.unwrap_or_default()));
                }
                Some(_) => true,
                None => false,
            },
            ParamKind::Bool => value.is_boolean(),
            ParamKind::Enum { options } => value.as_str().is_some_and(|v| options.contains(&v)),
            ParamKind::String | ParamKind::Path | ParamKind::Device => value.is_string(),
        };
        if ok {
            Ok(())
        } else {
            Err(format!("invalid value {value}"))
        }
    }
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

/// A filter's magnitude response, for previews in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct FrequencyResponse {
    /// Frequencies in Hz, increasing.
    pub freqs: Vec<f64>,
    /// Gain at each frequency, in dB.
    pub db: Vec<f32>,
    /// Number of taps the design has (0 for an IIR filter).
    pub taps: usize,
}

/// Computes the frequency response a block would have with these parameters
/// at this input sample rate. Pure, like a factory: no side effects.
pub type ResponseFn = fn(&ParamReader<'_>, f64) -> Result<FrequencyResponse, Error>;

struct Entry {
    descriptor: BlockDescriptor,
    factory: Factory,
    response: Option<ResponseFn>,
    tx: bool,
    keywords: &'static [&'static str],
}

/// A descriptor as the catalog API serves it: the descriptor plus what the
/// registry knows about the block beyond it.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry<'a> {
    #[serde(flatten)]
    pub descriptor: &'a BlockDescriptor,
    /// The block can preview its frequency response ([`Registry::response`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub response: bool,
    /// The block transmits RF ([`Registry::register_tx`]): runs that contain
    /// it must arm TX first.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tx: bool,
    /// Extra words the palette search matches ([`Registry::set_keywords`]),
    /// e.g. `abs` and `envelope` for a block labelled "Complex → Float".
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub keywords: &'static [&'static str],
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
        self.insert(descriptor, factory, None, false);
    }

    /// Register a block that transmits RF. Servers and the CLI refuse to
    /// start a graph containing one unless TX was explicitly armed.
    pub fn register_tx(&mut self, descriptor: BlockDescriptor, factory: Factory) {
        self.insert(descriptor, factory, None, true);
    }

    /// Whether blocks of `kind` transmit (see [`Self::register_tx`]).
    pub fn is_tx(&self, kind: &str) -> bool {
        self.entries.get(kind).is_some_and(|e| e.tx)
    }

    /// Register a block that can also preview its frequency response.
    pub fn register_with_response(
        &mut self,
        descriptor: BlockDescriptor,
        factory: Factory,
        response: ResponseFn,
    ) {
        self.insert(descriptor, factory, Some(response), false);
    }

    fn insert(
        &mut self,
        descriptor: BlockDescriptor,
        factory: Factory,
        response: Option<ResponseFn>,
        tx: bool,
    ) {
        let kind = descriptor.kind;
        let prev = self.entries.insert(
            kind,
            Entry {
                descriptor,
                factory,
                response,
                tx,
                keywords: &[],
            },
        );
        assert!(prev.is_none(), "block kind `{kind}` registered twice");
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &BlockDescriptor> {
        self.entries.values().map(|e| &e.descriptor)
    }

    /// Every block, as the catalog API serves it.
    pub fn catalog(&self) -> impl Iterator<Item = CatalogEntry<'_>> {
        self.entries.values().map(|e| CatalogEntry {
            descriptor: &e.descriptor,
            response: e.response.is_some(),
            tx: e.tx,
            keywords: e.keywords,
        })
    }

    /// Search keywords for an already registered block: other names people
    /// may look for it by (aliases, the operation it performs). The palette
    /// matches them along with the label, kind, category and help.
    pub fn set_keywords(&mut self, kind: &str, keywords: &'static [&'static str]) {
        let e = self
            .entries
            .get_mut(kind)
            .unwrap_or_else(|| panic!("set_keywords: block kind `{kind}` is not registered"));
        e.keywords = keywords;
    }

    /// The search keywords of `kind` (empty if none or unknown).
    pub fn keywords(&self, kind: &str) -> &'static [&'static str] {
        self.entries.get(kind).map_or(&[], |e| e.keywords)
    }

    /// The frequency response a block of `kind` would have with `params` at
    /// input rate `sample_rate`. `Ok(None)` if the block has no preview.
    pub fn response(
        &self,
        kind: &str,
        params: &Params,
        sample_rate: f64,
    ) -> Result<Option<FrequencyResponse>, Error> {
        let e = self
            .entries
            .get(kind)
            .ok_or_else(|| Error::UnknownBlock(kind.to_owned()))?;
        let Some(f) = e.response else {
            return Ok(None);
        };
        f(
            &ParamReader {
                spec: &e.descriptor,
                values: params,
            },
            sample_rate,
        )
        .map(Some)
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
    /// Whether this block kind has a parameter called `name` (for factories
    /// shared by several kinds).
    pub fn has(&self, name: &str) -> bool {
        self.spec.params.iter().any(|p| p.name == name)
    }

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
