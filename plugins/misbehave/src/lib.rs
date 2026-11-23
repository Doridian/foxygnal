//! Test plugin for the engine's sandbox: blocks that misbehave on purpose
//! (trap, spin forever, eat memory), plus a manual tagger and a decimator
//! that sets its output rate. Used by crates/foxygnal-plugin/tests.

use foxygnal_plugin_sdk::*;

struct Misbehave;

fn f32_block(kind: &str, label: &str, help: &str, params: Vec<ParamSpec>) -> BlockDescriptor {
    BlockDescriptor {
        kind: kind.into(),
        label: label.into(),
        category: "Test".into(),
        help: help.into(),
        inputs: vec![PortSpec::new("in", StreamType::F32)],
        outputs: vec![PortSpec::new("out", StreamType::F32)],
        params,
        tags: TagMode::AllToAll,
    }
}

impl Plugin for Misbehave {
    fn info() -> PluginInfo {
        PluginInfo {
            name: "misbehave".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            blocks: vec![
                f32_block(
                    "trap",
                    "Trap",
                    "Passes items through, then panics after `after` items.",
                    vec![ParamSpec::int(
                        "after",
                        "After",
                        1000,
                        "Items before the panic.",
                    )],
                ),
                f32_block("spin", "Spin", "Loops forever in work.", vec![]),
                f32_block(
                    "hog",
                    "Hog",
                    "Allocates memory in work until it runs out.",
                    vec![],
                ),
                BlockDescriptor {
                    tags: TagMode::Manual,
                    ..f32_block(
                        "tagger",
                        "Tagger",
                        "Passes items and tags through (manually) and tags the first item \
                         `seen`.",
                        vec![],
                    )
                },
                f32_block(
                    "half",
                    "Half",
                    "Keeps every other item and halves the sample rate.",
                    vec![],
                ),
                f32_block("fail_create", "Fail create", "Refuses to be built.", vec![]),
            ],
        }
    }

    fn create(kind: &str, params: &Params) -> Result<Created, String> {
        let block: Box<dyn Block> = match kind {
            "trap" => Box::new(Trap {
                left: params.i64("after")?.max(0) as usize,
            }),
            "spin" => Box::new(Spin),
            "hog" => Box::new(Hog(Vec::new())),
            "tagger" => Box::new(Tagger { first: true }),
            "half" => Box::new(Half { phase: 0 }),
            "fail_create" => return Err("this block never builds".into()),
            _ => return Err(format!("unknown kind `{kind}`")),
        };
        Ok(Created {
            block,
            inputs: vec![StreamType::F32],
            outputs: vec![StreamType::F32],
        })
    }
}

export_plugin!(Misbehave);

fn pass(io: &mut WorkIo<'_>, max: usize) -> usize {
    let (inp, out) = io.in_out::<f32, f32>(0, 0);
    let n = inp.len().min(out.len()).min(max);
    out[..n].copy_from_slice(&inp[..n]);
    io.consume(0, n);
    io.produce(0, n);
    n
}

struct Trap {
    left: usize,
}

impl Block for Trap {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        if self.left == 0 && !io.input::<f32>(0).is_empty() {
            panic!("boom");
        }
        let n = pass(io, self.left);
        self.left -= n;
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }
}

struct Spin;

impl Block for Spin {
    fn work(&mut self, _io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        let mut x = 0u64;
        loop {
            x = std::hint::black_box(x.wrapping_add(1));
        }
    }
}

struct Hog(Vec<Vec<u8>>);

impl Block for Hog {
    fn work(&mut self, _io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        loop {
            self.0.push(std::hint::black_box(vec![1u8; 1 << 20]));
        }
    }
}

struct Tagger {
    first: bool,
}

impl Block for Tagger {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        let avail = io.input::<f32>(0).len();
        let room = io.output::<f32>(0).len();
        let n = avail.min(room);
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let tags: Vec<_> = io
            .tags()
            .iter()
            .filter(|t| (t.index as usize) < n)
            .cloned()
            .collect();
        for t in tags {
            io.add_tag(0, t.index as usize, &t.key, t.value);
        }
        if self.first {
            self.first = false;
            io.add_tag(0, 0, "seen", TagValue::Boolean(true));
        }
        pass(io, n);
        Ok(WorkStatus::Ok)
    }
}

struct Half {
    phase: usize,
}

impl Block for Half {
    fn configure(&mut self, inputs: &[StreamMeta]) -> Result<Vec<StreamMeta>, String> {
        let rate = inputs
            .first()
            .and_then(|m| m.sample_rate)
            .ok_or("needs a sample rate")?;
        Ok(vec![StreamMeta {
            sample_rate: Some(rate / 2.0),
            center_freq: inputs[0].center_freq,
        }])
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        let (inp, out) = io.in_out::<f32, f32>(0, 0);
        let mut i = 0;
        let mut o = 0;
        while i < inp.len() && (o < out.len() || (self.phase + i) % 2 == 1) {
            if (self.phase + i).is_multiple_of(2) {
                out[o] = inp[i];
                o += 1;
            }
            i += 1;
        }
        if i == 0 {
            return Ok(WorkStatus::Blocked);
        }
        self.phase = (self.phase + i) % 2;
        io.consume(0, i);
        io.produce(0, o);
        Ok(WorkStatus::Ok)
    }
}
