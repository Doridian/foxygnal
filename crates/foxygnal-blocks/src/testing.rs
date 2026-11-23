//! Blocks for driving graphs from Rust code and tests.

use std::sync::{Arc, Mutex};

use foxygnal_core::{
    Block, Configure, Error, Instance, Sample, StreamMeta, Tag, WorkIo, WorkStatus,
};

/// Emits a fixed vector, then ends the stream.
pub struct VectorSource<T> {
    data: Vec<T>,
    pos: usize,
    meta: StreamMeta,
    /// Tags to attach, sorted by offset (an index into `data`).
    tags: Vec<Tag>,
}

impl<T: Sample> VectorSource<T> {
    pub fn instance(data: Vec<T>) -> Instance {
        Self::build(data, StreamMeta::default(), vec![])
    }

    /// A source that tells downstream its sample rate (and centre frequency).
    pub fn instance_with_meta(data: Vec<T>, meta: StreamMeta) -> Instance {
        Self::build(data, meta, vec![])
    }

    /// Like [`Self::instance`], also tagging items (`Tag::offset` indexes `data`).
    pub fn with_tags(data: Vec<T>, tags: Vec<Tag>) -> Instance {
        Self::build(data, StreamMeta::default(), tags)
    }

    fn build(data: Vec<T>, meta: StreamMeta, mut tags: Vec<Tag>) -> Instance {
        tags.sort_by_key(|t| t.offset);
        Instance {
            block: Box::new(Self {
                data,
                pos: 0,
                meta,
                tags,
            }),
            inputs: vec![],
            outputs: vec![("out".into(), T::TYPE)],
        }
    }
}

impl<T: Sample> Block for VectorSource<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for o in cx.outputs.iter_mut() {
            *o = self.meta;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let rest = &self.data[self.pos..];
        if rest.is_empty() {
            return Ok(WorkStatus::Done);
        }
        let out = io.outputs[0].slice_mut::<T>();
        let n = rest.len().min(out.len());
        out[..n].copy_from_slice(&rest[..n]);
        let end = (self.pos + n) as u64;
        for t in self
            .tags
            .iter()
            .filter(|t| t.offset >= self.pos as u64 && t.offset < end)
        {
            io.outputs[0].forward_tag(t.offset as usize - self.pos, t);
        }
        io.produce(0, n);
        self.pos += n;
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }
}

/// Collects everything it receives into a shared `Vec`.
pub struct VectorSink<T> {
    data: Arc<Mutex<Vec<T>>>,
    tags: Arc<Mutex<Vec<Tag>>>,
}

/// Items or tags collected by a [`VectorSink`].
pub type Collected<T> = Arc<Mutex<Vec<T>>>;

impl<T: Sample> VectorSink<T> {
    pub fn instance() -> (Instance, Collected<T>) {
        let (inst, data, _) = Self::with_tags();
        (inst, data)
    }

    /// Like [`Self::instance`], also collecting the tags it receives.
    pub fn with_tags() -> (Instance, Collected<T>, Collected<Tag>) {
        let data = Arc::new(Mutex::new(Vec::new()));
        let tags = Arc::new(Mutex::new(Vec::new()));
        let inst = Instance {
            block: Box::new(Self {
                data: data.clone(),
                tags: tags.clone(),
            }),
            inputs: vec![("in".into(), T::TYPE)],
            outputs: vec![],
        };
        (inst, data, tags)
    }
}

impl<T: Sample> Block for VectorSink<T> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].slice::<T>();
        let n = inp.len();
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        self.data.lock().expect("poisoned").extend_from_slice(inp);
        let tags = io.inputs[0].tags();
        if !tags.is_empty() {
            self.tags.lock().expect("poisoned").extend_from_slice(tags);
        }
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }
}
