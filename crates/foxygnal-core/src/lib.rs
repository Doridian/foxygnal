//! foxygnal-core: stream types, buffers, the block trait, the block registry
//! and the flowgraph runtime. No blocks live here — see `foxygnal-blocks`.

pub mod block;
pub mod buffer;
pub mod graph;
pub mod registry;
pub mod runtime;
pub mod sample;

pub use block::{Block, InputPort, OutputPort, WorkIo, WorkStatus};
pub use graph::GraphSpec;
pub use registry::{
    BlockDescriptor, Instance, ParamKind, ParamReader, ParamSpec, Params, PortSpec, Registry,
};
pub use runtime::{BlockId, Flowgraph, Running, StopHandle};
pub use sample::{Complex32, Sample, StreamType};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown block kind `{0}`")]
    UnknownBlock(String),
    #[error("block `{block}`, parameter `{param}`: {msg}")]
    Param {
        block: String,
        param: String,
        msg: String,
    },
    #[error("graph error: {0}")]
    Graph(String),
    #[error("block `{block}` failed: {source}")]
    Block {
        block: String,
        #[source]
        source: Box<Error>,
    },
    #[error("a block thread panicked")]
    Panic,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
