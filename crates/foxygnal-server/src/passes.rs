//! `GET /api/passes?block=<id>&hours=<h>`: upcoming passes of the satellites
//! a `sat_tracker` block knows (its TLE file and text), over the position it
//! is set to, for the palette's Passes section.
//!
//! Everything comes from the graph: no element sets are fetched.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use foxygnal_blocks::sat::{self, Observer, Sgp4, Tle};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::ApiError;

/// At most this many satellites are predicted (a whole catalogue would take
/// a while).
const MAX_SATS: usize = 150;
const MAX_HOURS: f64 = 7.0 * 24.0;

#[derive(Deserialize)]
pub struct PassQuery {
    block: String,
    #[serde(default = "default_hours")]
    hours: f64,
}

fn default_hours() -> f64 {
    24.0
}

#[derive(Serialize)]
pub struct SatInfo {
    name: String,
    norad: u32,
    /// Element set epoch (Unix s).
    epoch: f64,
    /// Age of the elements at `start`, days.
    age_days: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
pub struct PassInfo {
    name: String,
    norad: u32,
    aos: f64,
    tca: f64,
    los: f64,
    max_el: f64,
    aos_az: f64,
    tca_az: f64,
    los_az: f64,
    aos_clipped: bool,
    los_clipped: bool,
}

#[derive(Serialize)]
pub struct Passes {
    block: String,
    /// Start of the search (Unix s): now, or the tracker's start time.
    start: f64,
    end: f64,
    /// Whether `start` is the system clock (else the tracker's `time`).
    live: bool,
    lat: f64,
    lon: f64,
    alt: f64,
    min_elevation: f64,
    /// NORAD number of the satellite the tracker follows, if it found one.
    tracked: Option<u32>,
    satellites: Vec<SatInfo>,
    passes: Vec<PassInfo>,
    /// Element sets that didn't parse, files that couldn't be read.
    errors: Vec<String>,
}

pub async fn passes(
    State(s): State<Arc<AppState>>,
    Query(q): Query<PassQuery>,
) -> Result<Json<Passes>, ApiError> {
    if !(q.hours > 0.0 && q.hours <= MAX_HOURS) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("hours must be between 0 and {MAX_HOURS}"),
        ));
    }
    let session = s.session.clone();
    tokio::task::spawn_blocking(move || compute(&session, &q))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))
}

fn compute(session: &crate::session::Session, q: &PassQuery) -> Result<Passes, String> {
    let (kind, p) = session.resolved_params(&q.block)?;
    if kind != "sat_tracker" {
        return Err(format!("`{}` is a {kind}, not a sat_tracker", q.block));
    }
    let num = |k: &str, d: f64| p.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
    let text = |k: &str| {
        p.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    let obs = Observer {
        lat: num("lat", 0.0),
        lon: num("lon", 0.0),
        alt_m: num("alt", 0.0),
    };
    let min_el = num("min_elevation", 0.0);
    let time = text("time");
    let (start, live) = if time.is_empty() {
        (sat::now_unix(), true)
    } else {
        (
            sat::parse_utc(&time)
                .ok_or_else(|| format!("start time `{time}` is not a UTC time"))?,
            false,
        )
    };
    let end = start + q.hours * 3600.0;

    let mut errors = Vec::new();
    let mut sets: Vec<Tle> = Vec::new();
    let tle_text = text("tle");
    if !tle_text.is_empty() {
        let (s, e) = sat::parse_all(&tle_text);
        sets.extend(s);
        errors.extend(e.into_iter().map(|e| format!("TLE text: {e}")));
    }
    let file = text("tle_file");
    if !file.is_empty() {
        match std::fs::read_to_string(&file) {
            Ok(t) => {
                let (s, e) = sat::parse_all(&t);
                sets.extend(s);
                errors.extend(e.into_iter().map(|e| format!("{file}: {e}")));
            }
            Err(e) => errors.push(format!("can't read {file}: {e}")),
        }
    }
    let tracked = sat::find(&sets, &text("satellite")).map(|t| t.norad);
    if sets.len() > MAX_SATS {
        errors.push(format!(
            "{} element sets: only the first {MAX_SATS} are predicted",
            sets.len()
        ));
        sets.truncate(MAX_SATS);
    }

    let mut satellites = Vec::new();
    let mut passes = Vec::new();
    for t in &sets {
        let mut info = SatInfo {
            name: t.label(),
            norad: t.norad,
            epoch: t.epoch_unix(),
            age_days: (start - t.epoch_unix()) / 86_400.0,
            error: None,
        };
        match Sgp4::new(t) {
            Ok(m) => passes.extend(sat::passes(&m, &obs, min_el, start, end).into_iter().map(
                |p| PassInfo {
                    name: t.label(),
                    norad: t.norad,
                    aos: p.aos,
                    tca: p.tca,
                    los: p.los,
                    max_el: p.max_el,
                    aos_az: p.aos_az,
                    tca_az: p.tca_az,
                    los_az: p.los_az,
                    aos_clipped: p.aos_clipped,
                    los_clipped: p.los_clipped,
                },
            )),
            Err(e) => info.error = Some(e.to_string()),
        }
        satellites.push(info);
    }
    passes.sort_by(|a, b| a.aos.total_cmp(&b.aos));
    Ok(Passes {
        block: q.block.clone(),
        start,
        end,
        live,
        lat: obs.lat,
        lon: obs.lon,
        alt: obs.alt_m,
        min_elevation: min_el,
        tracked,
        satellites,
        passes,
        errors,
    })
}
