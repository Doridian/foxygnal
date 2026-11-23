//! Browser audio: a sink whose input the UI plays through the speakers of
//! the tab that started the run (WebAudio), so listening needs no audio
//! libraries on the engine's machine and works over a remote connection.
//!
//! The block itself only consumes its input, paced at the stream's rate
//! unless something upstream (an SDR) already sets the pace. The server
//! streams the samples to the browser through an `audio` probe on the wire
//! feeding it.
//!
//! With `type = "c32"` it is a stereo sink: each complex sample is one
//! frame, the real part left and the imaginary part right, so the channels
//! travel together and can't drift apart.

use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

/// WebAudio can play buffers at these rates.
const RATES: std::ops::RangeInclusive<f64> = 3_000.0..=768_000.0;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "browser_audio",
            label: "Browser Audio",
            category: "Sinks",
            help: "Plays its input in the browser tab that pressed Run. Needs a known sample \
                   rate (48 kS/s is a safe choice) and samples in −1…1. For stereo set the \
                   channels to c32 and feed left as the real part, right as the imaginary \
                   part (e.g. through Re + Im → Complex).",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![],
            params: vec![
                ParamSpec {
                    name: "type",
                    label: "Channels",
                    kind: ParamKind::Enum {
                        options: vec!["f32", "c32"],
                    },
                    default: json!("f32"),
                    help: "f32: mono. c32: stereo, real part = left, imaginary part = right.",
                    live: false,
                },
                ParamSpec {
                    name: "pace",
                    label: "Pace",
                    kind: ParamKind::Bool,
                    default: json!(true),
                    help: "Consume at the stream's rate, so files and generators play in real \
                           time. Turn off when hardware (an SDR) already sets the pace.",
                    live: false,
                },
            ],
        },
        build,
    );
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    // Older graphs have no `type`: the default, mono.
    let ty = match p.stream_type("type")? {
        StreamType::C32 => StreamType::C32,
        _ => StreamType::F32,
    };
    Ok(Instance {
        block: Box::new(BrowserAudio {
            pace: p.bool("pace")?,
            rate: 0.0,
            start: None,
            consumed: 0,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![],
    })
}

pub struct BrowserAudio {
    pace: bool,
    rate: f64,
    start: Option<Instant>,
    consumed: u64,
}

impl Block for BrowserAudio {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if !RATES.contains(&rate) {
            return Err(Error::Other(format!(
                "browsers play audio at 3–768 kS/s, not {rate} S/s: resample it first \
                 (48 kS/s is a safe choice)"
            )));
        }
        self.rate = rate;
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        // One item is one frame, mono (f32) or stereo (c32).
        let input = &io.inputs[0];
        let avail = input.bytes().len() / input.stream_type().item_size();
        let mut n = avail;
        if self.pace {
            let start = *self.start.get_or_insert_with(Instant::now);
            // A little ahead of real time, so the browser's buffer stays fed.
            let due = ((start.elapsed().as_secs_f64() + 0.05) * self.rate) as u64;
            let allowed = due.saturating_sub(self.consumed) as usize;
            if allowed == 0 {
                return Ok(WorkStatus::Wait(Duration::from_millis(5)));
            }
            n = n.min(allowed);
        }
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, n);
        self.consumed += n as u64;
        Ok(WorkStatus::Ok)
    }
}
