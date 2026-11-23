//! Plugin descriptors and values ↔ the engine's.
//!
//! The registry's descriptors use `&'static str`. Plugins load once at
//! start-up, so their strings are leaked (a few KB per plugin).

use foxygnal_core::{BlockDescriptor, Error, ParamKind, ParamSpec, PortSpec, StreamType, TagValue};
use serde_json::Value;

use crate::block::MAX_PORTS;
use crate::wit_types as w;

pub fn leak(s: impl Into<String>) -> &'static str {
    Box::leak(s.into().into_boxed_str())
}

fn err(msg: impl std::fmt::Display) -> Error {
    Error::Other(msg.to_string())
}

/// A plugin name: lowercase letters, digits, `_` and `-`, starting with a
/// letter, at most 32 characters, and not `graph` (taken by graph files).
pub fn check_plugin_name(name: &str) -> Result<&str, Error> {
    let ok = name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        return Err(err(format!(
            "invalid plugin name `{name}` (lowercase letters, digits, `_` and `-`, starting \
             with a letter)"
        )));
    }
    if name == "graph" {
        return Err(err("a plugin can't be called `graph`"));
    }
    Ok(name)
}

/// What the engine needs to hold true of a plugin's descriptor.
pub fn check_block(b: &w::BlockDescriptor) -> Result<(), Error> {
    let ok = !b.kind.is_empty()
        && b.kind.len() <= 64
        && b.kind
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        return Err(err("invalid kind (letters, digits, `_` and `-`)"));
    }
    if b.inputs.len() > MAX_PORTS || b.outputs.len() > MAX_PORTS {
        return Err(err(format!("more than {MAX_PORTS} inputs or outputs")));
    }
    let mut names: Vec<&str> = b.params.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err(err("two parameters with the same name"));
    }
    Ok(())
}

pub fn stream_type(t: w::StreamType) -> StreamType {
    match t {
        w::StreamType::F32 => StreamType::F32,
        w::StreamType::C32 => StreamType::C32,
        w::StreamType::I16 => StreamType::I16,
        w::StreamType::U8 => StreamType::U8,
    }
}

fn port(p: &w::PortSpec) -> PortSpec {
    PortSpec {
        name: leak(p.name.as_str()),
        ty: p.type_.map(stream_type),
    }
}

fn param(p: &w::ParamSpec) -> Result<ParamSpec, Error> {
    let kind = match &p.kind {
        w::ParamKind::Float(r) => ParamKind::Float {
            min: r.min,
            max: r.max,
            unit: r.unit.as_deref().map(leak),
        },
        w::ParamKind::Int(r) => ParamKind::Int {
            min: r.min,
            max: r.max,
        },
        w::ParamKind::Boolean => ParamKind::Bool,
        w::ParamKind::Choice(options) => ParamKind::Enum {
            options: options.iter().map(|o| leak(o.as_str())).collect(),
        },
        w::ParamKind::Text => ParamKind::String,
    };
    let default = match &p.default {
        w::ParamValue::Float(v) => serde_json::json!(v),
        w::ParamValue::Int(v) => serde_json::json!(v),
        w::ParamValue::Boolean(v) => Value::Bool(*v),
        w::ParamValue::Text(v) => Value::String(v.clone()),
    };
    let spec = ParamSpec {
        name: leak(p.name.as_str()),
        label: leak(p.label.as_str()),
        kind,
        default,
        help: leak(p.help.as_str()),
        live: p.live,
    };
    spec.check(&spec.default)
        .map_err(|e| err(format!("parameter `{}`: default {e}", p.name)))?;
    Ok(spec)
}

pub fn descriptor(plugin: &str, b: &w::BlockDescriptor) -> Result<BlockDescriptor, Error> {
    Ok(BlockDescriptor {
        kind: leak(format!("{plugin}:{}", b.kind)),
        label: leak(b.label.as_str()),
        category: leak(b.category.as_str()),
        help: leak(b.help.as_str()),
        inputs: b.inputs.iter().map(port).collect(),
        outputs: b.outputs.iter().map(port).collect(),
        params: b
            .params
            .iter()
            .map(param)
            .collect::<Result<_, _>>()
            .map_err(|e| err(format!("block `{}`: {e}", b.kind)))?,
    })
}

/// A JSON parameter value (already checked against `spec`) for the plugin.
pub fn param_value(spec: &ParamSpec, v: &Value) -> Result<w::ParamValue, String> {
    let bad = || format!("parameter `{}`: invalid value {v}", spec.name);
    Ok(match spec.kind {
        ParamKind::Float { .. } => w::ParamValue::Float(v.as_f64().ok_or_else(bad)?),
        ParamKind::Int { .. } => w::ParamValue::Int(v.as_i64().ok_or_else(bad)?),
        ParamKind::Bool => w::ParamValue::Boolean(v.as_bool().ok_or_else(bad)?),
        _ => w::ParamValue::Text(v.as_str().ok_or_else(bad)?.to_owned()),
    })
}

pub fn meta_to_wit(m: &foxygnal_core::StreamMeta) -> w::StreamMeta {
    w::StreamMeta {
        sample_rate: m.sample_rate,
        center_freq: m.center_freq,
    }
}

pub fn meta_from_wit(m: &w::StreamMeta) -> foxygnal_core::StreamMeta {
    foxygnal_core::StreamMeta {
        sample_rate: m.sample_rate,
        center_freq: m.center_freq,
    }
}

pub fn tag_to_wit(v: &TagValue) -> w::TagValue {
    match v {
        TagValue::Bool(b) => w::TagValue::Boolean(*b),
        TagValue::Int(i) => w::TagValue::Int(*i),
        TagValue::Float(f) => w::TagValue::Float(*f),
        TagValue::Str(s) => w::TagValue::Text(s.to_string()),
    }
}

pub fn tag_from_wit(v: w::TagValue) -> TagValue {
    match v {
        w::TagValue::Boolean(b) => TagValue::Bool(b),
        w::TagValue::Int(i) => TagValue::Int(i),
        w::TagValue::Float(f) => TagValue::Float(f),
        w::TagValue::Text(s) => TagValue::Str(s.into()),
    }
}
