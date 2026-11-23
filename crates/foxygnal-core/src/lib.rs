//! foxygnal-core: stream types, buffers, the block trait, the block registry
//! and the flowgraph runtime. No blocks live here — see `foxygnal-blocks`.

pub mod block;
pub mod buffer;
pub mod expr;
pub mod graph;
pub mod meta;
pub mod raster;
pub mod registry;
pub mod resolve;
pub mod runtime;
pub mod sample;
mod sched;
pub mod tag;
mod vmem;

pub use block::{Block, InputPort, OutputPort, WorkIo, WorkStatus};
pub use buffer::{BufferKind, SPARE_READERS, Tap, TapRead, TapReader};
pub use graph::GraphSpec;
pub use meta::{Configure, StreamMeta};
pub use registry::{
    BlockDescriptor, CatalogEntry, DynFactory, FrequencyResponse, Instance, ParamKind, ParamReader,
    ParamSpec, Params, PortSpec, Registry, ResponseFn,
};
pub use resolve::{DirLoader, GraphLoader, NoLoader, Resolved, resolve};
pub use runtime::{
    BlockCounters, BlockId, BlockWarning, EdgeCounters, EdgeMeta, Flowgraph, GraphCounters,
    Monitor, ParamSender, Running, StopHandle,
};
pub use sample::{Complex32, Sample, StreamType};
pub use tag::{Tag, TagPropagation, TagValue};

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
    /// A connection could not be made. `msg` names both endpoints.
    #[error("{msg}")]
    Edge {
        from: String,
        to: String,
        msg: String,
    },
    #[error("block `{block}`: {source}")]
    Block {
        block: String,
        #[source]
        source: Box<Error>,
    },
    #[error("the block thread panicked")]
    Panic,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// The id of the block this error is about, if it is about one.
    pub fn block(&self) -> Option<&str> {
        match self {
            Self::Param { block, .. } | Self::Block { block, .. } => Some(block),
            _ => None,
        }
    }

    /// The message without the block prefix, for display next to the block.
    pub fn detail(&self) -> String {
        match self {
            Self::Block { source, .. } => source.detail(),
            Self::Param { param, msg, .. } => format!("parameter `{param}`: {msg}"),
            e => e.to_string(),
        }
    }
}
