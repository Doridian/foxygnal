//! The `sat-doppler` template, run headless on the simulated SDR with the
//! tracker's clock set to a moment in a Meteor-M N2-3 pass, checked against
//! Skyfield's look angles for that moment (`data/sat/skyfield-2026-11-23.txt`).

use std::time::{Duration, Instant};

use foxygnal_blocks::sat::Utc;
use foxygnal_blocks::testing::VectorSink;
use foxygnal_core::{Flowgraph, GraphSpec, NoLoader};
use serde_json::json;

#[test]
fn template_tracks_doppler_in_real_time() {
    // Skyfield: METEOR-M2 3 at this time is at el 43.838399°, az 347.116559°,
    // range rate −4.007941280 km/s (approaching).
    let t = 1_795_512_060.0;
    let (want_el, want_az, want_rr) = (43.838_399, 347.116_559, -4.007_941_280);
    let want_offset = 250_000.0 - 137.9e6 * want_rr / 299_792.458;

    let mut spec =
        GraphSpec::from_toml(include_str!("../../../examples/sat-doppler.fox.toml")).unwrap();
    let sdr = spec.blocks.iter_mut().find(|b| b.id == "sdr").unwrap();
    sdr.params.insert("device".into(), json!("sim:"));
    let tr = spec.blocks.iter_mut().find(|b| b.id == "tracker").unwrap();
    tr.params.insert("time".into(), json!(Utc(t).to_string()));
    let reg = foxygnal_blocks::registry();
    let r = foxygnal_core::resolve(&spec, &reg, &NoLoader).unwrap();
    let mut fg = Flowgraph::from_resolved(&r.graph, &reg).unwrap();
    let tracker = fg.block_id("tracker").unwrap();
    let (s, offset) = VectorSink::<f32>::instance();
    let s = fg.add("offsets", s);
    fg.connect(tracker, 0, s, 0).unwrap();
    let (s, az) = VectorSink::<f32>::instance();
    let s = fg.add("az", s);
    fg.connect(tracker, 1, s, 0).unwrap();
    let (s, el) = VectorSink::<f32>::instance();
    let s = fg.add("el", s);
    fg.connect(tracker, 2, s, 0).unwrap();
    let (s, text) = VectorSink::<u8>::instance();
    let s = fg.add("text", s);
    fg.connect(tracker, 3, s, 0).unwrap();
    let edges = fg.configure().unwrap();
    let ctl = edges.iter().find(|e| e.to == "channel.@offset").unwrap();
    assert_eq!(ctl.meta.sample_rate, Some(20.0));

    let running = fg.start().unwrap();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(1200));
    let elapsed = t0.elapsed().as_secs_f64();
    assert!(!running.is_finished());
    running.stop();
    running.wait().unwrap();

    let offset = offset.lock().unwrap();
    let az = az.lock().unwrap();
    let el = el.lock().unwrap();
    // Paced by the clock: one value every 50 ms (and one at the start).
    let n = offset.len() as f64;
    assert!(
        (n - (elapsed * 20.0 + 1.0)).abs() <= 3.0,
        "{n} values in {elapsed} s"
    );
    assert_eq!(az.len(), offset.len());
    assert_eq!(el.len(), offset.len());
    assert!((f64::from(el[0]) - want_el).abs() < 0.005, "el {}", el[0]);
    assert!((f64::from(az[0]) - want_az).abs() < 0.01, "az {}", az[0]);
    assert!(
        (f64::from(offset[0]) - want_offset).abs() < 0.5,
        "offset {} vs {want_offset}",
        offset[0]
    );
    // Approaching fast near the top of the pass: the Doppler falls.
    assert!(offset[offset.len() - 1] < offset[0]);

    let text = String::from_utf8(text.lock().unwrap().clone()).unwrap();
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("time=2026-11-24T"), "{first}");
    assert!(
        first.contains("name=\"METEOR-M2 3\" id=57166 lat="),
        "{first}"
    );
    assert!(first.contains(" el=43.84 "), "{first}");
    assert!(text.lines().count() >= 2, "{text}");
}

#[test]
fn a_bad_satellite_or_tle_is_a_readable_error() {
    let reg = foxygnal_blocks::registry();
    let p = |v: serde_json::Value| v.as_object().unwrap().clone();
    let tle = "METEOR-M2 3\n1 57166U 23091A   26327.52301369 -.00000007  00000+0  15745-4 0  9992\n2 57166  98.5884  19.0681 0004988  73.2184 286.9542 14.24057105177198\n";
    let e = reg
        .build(
            "sat_tracker",
            &p(json!({"tle": tle, "satellite": "NOAA 15"})),
        )
        .err()
        .unwrap();
    assert!(e.to_string().contains("no satellite `NOAA 15`"), "{e}");
    assert!(e.to_string().contains("METEOR-M2 3"), "{e}");
    let e = reg
        .build(
            "sat_tracker",
            &p(json!({"tle": tle.replace("9992", "9993")})),
        )
        .err()
        .unwrap();
    assert!(e.to_string().contains("checksum"), "{e}");
    let e = reg.build("sat_tracker", &p(json!({}))).err().unwrap();
    assert!(e.to_string().contains("no element sets"), "{e}");
    assert!(
        reg.build("sat_tracker", &p(json!({"tle": tle, "satellite": "57166"})))
            .is_ok()
    );
}
