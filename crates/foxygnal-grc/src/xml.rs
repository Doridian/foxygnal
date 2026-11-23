//! The XML `.grc` format of GNU Radio 3.7 (and 3.6), read into the same
//! [`GrcFile`] the YAML path produces, so mapping and expression
//! translation are shared.
//!
//! ```xml
//! <flow_graph>
//!   <block>
//!     <key>blocks_throttle</key>
//!     <param><key>id</key><value>blocks_throttle_0</value></param>
//!     <param><key>_enabled</key><value>True</value></param>
//!     <param><key>_coordinate</key><value>(272, 180)</value></param>
//!     …
//!   </block>
//!   <connection>
//!     <source_block_id>…</source_block_id><sink_block_id>…</sink_block_id>
//!     <source_key>0</source_key><sink_key>0</sink_key>
//!   </connection>
//! </flow_graph>
//! ```

use std::collections::BTreeMap;

use roxmltree::{Document, Node};

use crate::{Error, GrcBlock, GrcConn, GrcFile, State};

/// The text of the first child element called `name`.
fn child_text<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.children()
        .find(|c| c.has_tag_name(name))
        .map(|c| c.text().unwrap_or("").trim())
}

/// `_enabled`: `True` / `1` enabled, `False` / `0` disabled, `2` bypassed
/// (GRC 3.7.9 and later).
fn state(v: Option<&str>) -> State {
    match v.map(str::trim) {
        Some("False" | "false" | "0") => State::Disabled,
        Some("2") => State::Bypassed,
        _ => State::Enabled,
    }
}

/// `_coordinate`: `(x, y)`.
fn coordinate(v: &str) -> Option<(f64, f64)> {
    let inner = v.trim().strip_prefix('(')?.strip_suffix(')')?;
    let (x, y) = inner.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// GNU Radio 3.6 block ids (still found in 3.7-era files) that became the
/// 3.7 ones the mapping table knows.
fn rename(key: &str) -> &str {
    match key {
        "gr_sig_source_x" => "analog_sig_source_x",
        "gr_noise_source_x" => "analog_noise_source_x",
        "gr_throttle" => "blocks_throttle",
        "gr_head" => "blocks_head",
        "gr_null_sink" => "blocks_null_sink",
        "gr_file_source" => "blocks_file_source",
        "gr_file_sink" => "blocks_file_sink",
        "gr_wavfile_source" => "blocks_wavfile_source",
        "gr_wavfile_sink" => "blocks_wavfile_sink",
        "gr_add_xx" => "blocks_add_xx",
        "gr_multiply_xx" => "blocks_multiply_xx",
        "gr_multiply_const_vxx" => "blocks_multiply_const_vxx",
        "gr_complex_to_mag" => "blocks_complex_to_mag",
        "gr_complex_to_mag_squared" => "blocks_complex_to_mag_squared",
        "gr_complex_to_float" => "blocks_complex_to_float",
        "gr_float_to_complex" => "blocks_float_to_complex",
        "gr_quadrature_demod_cf" => "analog_quadrature_demod_cf",
        "gr_freq_xlating_fir_filter_xxx" => "freq_xlating_fir_filter_xxx",
        "blks2_rational_resampler_xxx" => "rational_resampler_xxx",
        "blks2_wfm_rcv" => "analog_wfm_rcv",
        "blks2_nbfm_rx" => "analog_nbfm_rx",
        other => other,
    }
}

pub(crate) fn parse(text: &str) -> Result<GrcFile, Error> {
    let doc = Document::parse(text).map_err(|e| Error(format!("not valid XML: {e}")))?;
    let root = doc.root_element();
    if !root.has_tag_name("flow_graph") {
        return Err(Error(format!(
            "XML, but not a GNU Radio Companion 3.7 flowgraph: the root element is \
             <{}>, not <flow_graph>",
            root.tag_name().name()
        )));
    }
    let mut title = None;
    let mut generate = None;
    let mut blocks = Vec::new();
    let mut connections = Vec::new();
    for (i, el) in root.children().filter(Node::is_element).enumerate() {
        match el.tag_name().name() {
            "block" => {
                let key = child_text(el, "key")
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| {
                        Error(format!(
                            "<block> #{} (line {}) has no <key> (block type)",
                            i + 1,
                            doc.text_pos_at(el.range().start).row
                        ))
                    })?;
                let mut params = BTreeMap::new();
                for p in el.children().filter(|c| c.has_tag_name("param")) {
                    let Some(k) = child_text(p, "key").filter(|k| !k.is_empty()) else {
                        return Err(Error(format!(
                            "a <param> of block `{key}` (line {}) has no <key>",
                            doc.text_pos_at(p.range().start).row
                        )));
                    };
                    // Values keep their inner whitespace (embedded Python source).
                    let v = p
                        .children()
                        .find(|c| c.has_tag_name("value"))
                        .and_then(|c| c.text())
                        .unwrap_or("");
                    params.insert(k.to_owned(), v.to_owned());
                }
                let name = params
                    .get("id")
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("block_{i}"));
                let st = state(params.get("_enabled").map(String::as_str));
                let coord = params.get("_coordinate").and_then(|c| coordinate(c));
                // GRC's own bookkeeping, not block parameters.
                params.retain(|k, _| !k.starts_with('_') && k != "id");
                if key == "options" {
                    let get = |k: &str| {
                        params
                            .get(k)
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty())
                    };
                    title = get("title");
                    generate = get("generate_options");
                    continue;
                }
                blocks.push(GrcBlock {
                    name,
                    id: rename(key).to_owned(),
                    params,
                    state: st,
                    coord,
                });
            }
            "connection" => {
                let get = |k: &str| {
                    child_text(el, k).map(str::to_owned).ok_or_else(|| {
                        Error(format!(
                            "<connection> on line {} has no <{k}>",
                            doc.text_pos_at(el.range().start).row
                        ))
                    })
                };
                connections.push(GrcConn {
                    src: get("source_block_id")?,
                    src_port: get("source_key")?,
                    dst: get("sink_block_id")?,
                    dst_port: get("sink_key")?,
                });
            }
            // <timestamp>, and anything newer GRC versions added.
            _ => {}
        }
    }
    Ok(GrcFile {
        title,
        generate,
        blocks,
        connections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_and_coordinates() {
        assert_eq!(state(None), State::Enabled);
        assert_eq!(state(Some("True")), State::Enabled);
        assert_eq!(state(Some("1")), State::Enabled);
        assert_eq!(state(Some("False")), State::Disabled);
        assert_eq!(state(Some("0")), State::Disabled);
        assert_eq!(state(Some("2")), State::Bypassed);
        assert_eq!(coordinate("(272, 180)"), Some((272.0, 180.0)));
        assert_eq!(coordinate(" (8,8) "), Some((8.0, 8.0)));
        assert_eq!(coordinate("8, 8"), None);
        assert_eq!(coordinate("(a, 8)"), None);
    }
}
