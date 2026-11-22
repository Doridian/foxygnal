//! Blocks for driving graphs from Rust code and tests.

use std::sync::{Arc, Mutex};

use foxygnal_core::{Block, Error, Instance, Sample, WorkIo, WorkStatus};

/// Emits a fixed vector, then ends the stream.
pub struct VectorSource<T> {
    data: Vec<T>,
    pos: usize,
}

impl<T: Sample> VectorSource<T> {
    pub fn instance(data: Vec<T>) -> Instance {
        Instance {
            block: Box::new(Self { data, pos: 0 }),
            inputs: vec![],
            outputs: vec![("out".into(), T::TYPE)],
        }
    }
}

impl<T: Sample> Block for VectorSource<T> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let rest = &self.data[self.pos..];
        if rest.is_empty() {
            return Ok(WorkStatus::Done);
        }
        let out = io.outputs[0].slice_mut::<T>();
        let n = rest.len().min(out.len());
        out[..n].copy_from_slice(&rest[..n]);
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
}

impl<T: Sample> VectorSink<T> {
    pub fn instance() -> (Instance, Arc<Mutex<Vec<T>>>) {
        let data = Arc::new(Mutex::new(Vec::new()));
        let inst = Instance {
            block: Box::new(Self { data: data.clone() }),
            inputs: vec![("in".into(), T::TYPE)],
            outputs: vec![],
        };
        (inst, data)
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
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }
}
