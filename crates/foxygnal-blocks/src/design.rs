//! Parameter specs and helpers shared by the filter, rate-change, demodulator
//! and level blocks: FIR window choice, Hz formatting for error messages and
//! frequency-response previews.

use foxygnal_core::{Error, FrequencyResponse, ParamKind, ParamReader, ParamSpec};
use foxygnal_dsp::{DesignError, DesignWindow, fir};
use serde_json::json;

/// Points in a frequency-response preview.
pub const RESPONSE_POINTS: usize = 512;

/// A float with a lower bound.
pub fn float_min(
    name: &'static str,
    label: &'static str,
    default: f64,
    min: f64,
    unit: Option<&'static str>,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Float {
            min: Some(min),
            max: None,
            unit,
        },
        default: json!(default),
        help,
        live: false,
    }
}

pub fn int(
    name: &'static str,
    label: &'static str,
    default: i64,
    min: i64,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Int {
            min: Some(min),
            max: None,
        },
        default: json!(default),
        help,
        live: false,
    }
}

/// An int within `min..=max`.
pub fn int_range(
    name: &'static str,
    label: &'static str,
    default: i64,
    min: i64,
    max: i64,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Int {
            min: Some(min),
            max: Some(max),
        },
        default: json!(default),
        help,
        live: false,
    }
}

pub fn choice(
    name: &'static str,
    label: &'static str,
    options: Vec<&'static str>,
    default: &'static str,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Enum { options },
        default: json!(default),
        help,
        live: false,
    }
}

/// The window of a windowed-sinc FIR design, and the Kaiser attenuation.
pub fn fir_window() -> [ParamSpec; 2] {
    [
        choice(
            "window",
            "Window",
            vec!["hamming", "blackman", "kaiser"],
            "hamming",
            "Hamming: ~53 dB stopband, fewest taps. Blackman: ~74 dB. Kaiser: the \
             attenuation you ask for (more dB = more taps).",
        ),
        float_min(
            "attenuation",
            "Attenuation",
            60.0,
            20.0,
            Some("dB"),
            "Stopband attenuation for the Kaiser window (ignored otherwise).",
        ),
    ]
}

/// Read the parameters made by [`fir_window`].
pub fn read_fir_window(p: &ParamReader<'_>) -> Result<DesignWindow, Error> {
    Ok(match p.str("window")? {
        "blackman" => DesignWindow::Blackman,
        "kaiser" => DesignWindow::Kaiser {
            attenuation: p.f64("attenuation")?,
        },
        _ => DesignWindow::Hamming,
    })
}

/// A rate change's transition band, as a fraction of the slower side's
/// Nyquist frequency.
pub fn rate_transition() -> ParamSpec {
    ParamSpec {
        name: "transition",
        label: "Transition",
        kind: ParamKind::Float {
            min: Some(0.01),
            max: Some(1.0),
            unit: None,
        },
        default: json!(0.2),
        help: "Width of the anti-alias filter's transition band, as a fraction of the \
               output's Nyquist frequency. Smaller = sharper but more taps. 0.2 passes \
               90% of the new band.",
        live: false,
    }
}

/// Format a frequency for messages: `12.5 kHz`.
pub fn fmt_hz(f: f64) -> String {
    si(f, "Hz")
}

/// Format a rate for messages: `2.4 MS/s`.
pub fn fmt_rate(f: f64) -> String {
    si(f, "S/s")
}

fn si(v: f64, unit: &str) -> String {
    let (scale, prefix) = match v.abs() {
        a if a >= 1e9 => (1e9, "G"),
        a if a >= 1e6 => (1e6, "M"),
        a if a >= 1e3 => (1e3, "k"),
        _ => (1.0, ""),
    };
    let s = format!("{:.4}", v / scale);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s} {prefix}{unit}")
}

/// Turn a rejected FIR design into a message in Hz.
pub fn design_error(e: DesignError, rate: f64) -> Error {
    Error::Other(match e {
        DesignError::EdgeOutOfRange(f) => format!(
            "cutoff {} must be above 0 Hz and below {} (the Nyquist frequency at {})",
            fmt_hz(f * rate),
            fmt_hz(rate / 2.0),
            fmt_rate(rate),
        ),
        DesignError::EdgesOrder => "the low cutoff must be below the high cutoff".into(),
        DesignError::Transition => "the transition width must be more than 0 Hz".into(),
        DesignError::TooManyTaps(n) => format!(
            "the transition band is too narrow for {}: it would need {n} taps (at most {}). \
             Widen it, or lower the sample rate first",
            fmt_rate(rate),
            fir::MAX_TAPS,
        ),
    })
}

/// Response of real taps running at `rate`, from 0 Hz to Nyquist.
pub fn real_response(taps: &[f32], rate: f64) -> FrequencyResponse {
    let (freqs, db) = fir::response_db(taps, RESPONSE_POINTS);
    FrequencyResponse {
        freqs: freqs.into_iter().map(|f| f * rate).collect(),
        db,
        taps: taps.len(),
    }
}

/// Response of real (low-pass) taps shifted up by `shift` Hz, as seen by a
/// complex stream at `rate`: from −Nyquist to +Nyquist.
pub fn shifted_response(taps: &[f32], rate: f64, shift: f64) -> FrequencyResponse {
    let n = RESPONSE_POINTS;
    let freqs: Vec<f64> = (0..n)
        .map(|i| rate * (-0.5 + i as f64 / (n - 1) as f64))
        .collect();
    let db = freqs
        .iter()
        .map(|&f| fir::to_db(fir::magnitude_at(taps, (f - shift) / rate)))
        .collect();
    FrequencyResponse {
        freqs,
        db,
        taps: taps.len(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn si_formatting() {
        assert_eq!(super::fmt_hz(12_500.0), "12.5 kHz");
        assert_eq!(super::fmt_rate(2.4e6), "2.4 MS/s");
        assert_eq!(super::fmt_hz(300.0), "300 Hz");
        assert_eq!(super::fmt_hz(-75_000.0), "-75 kHz");
    }
}
