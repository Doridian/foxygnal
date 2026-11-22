//! Shared parameter specs used by many blocks.

use foxygnal_core::{ParamKind, ParamSpec};
use serde_json::json;

pub fn stream_type(default: &str, options: Vec<&'static str>) -> ParamSpec {
    ParamSpec {
        name: "type",
        label: "Type",
        kind: ParamKind::Enum { options },
        default: json!(default),
        help: "Item type: f32 = real, c32 = complex I/Q, i16 / u8 = integers.",
    }
}

pub fn any_type() -> ParamSpec {
    stream_type("c32", vec!["c32", "f32", "i16", "u8"])
}

pub fn float_type() -> ParamSpec {
    stream_type("c32", vec!["c32", "f32"])
}

pub fn sample_rate() -> ParamSpec {
    ParamSpec {
        name: "sample_rate",
        label: "Sample rate",
        kind: ParamKind::Float {
            min: Some(1.0),
            max: None,
            unit: Some("S/s"),
        },
        default: json!(48_000.0),
        help: "Samples per second.",
    }
}

pub fn float(
    name: &'static str,
    label: &'static str,
    default: f64,
    unit: Option<&'static str>,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Float {
            min: None,
            max: None,
            unit,
        },
        default: json!(default),
        help,
    }
}
