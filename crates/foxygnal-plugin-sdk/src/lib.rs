//! Write foxygnal blocks as WebAssembly component plugins.
//!
//! A plugin is a `cdylib` built for `wasm32-unknown-unknown`. It implements
//! [`Plugin`] (what blocks it has, and how to build one) and [`Block`] for
//! each block, then calls [`export_plugin!`]. Writing a block is close to
//! writing a native one: `work` gets typed input slices and output slices,
//! and calls `consume` / `produce`.
//!
//! ```ignore
//! use foxygnal_plugin_sdk::*;
//!
//! struct Gain(f32);
//! impl Block for Gain {
//!     fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
//!         let (inp, out) = io.in_out::<f32, f32>(0, 0);
//!         let n = inp.len().min(out.len());
//!         if n == 0 {
//!             return Ok(WorkStatus::Blocked);
//!         }
//!         for (o, x) in out[..n].iter_mut().zip(&inp[..n]) {
//!             *o = x * self.0;
//!         }
//!         io.consume(0, n);
//!         io.produce(0, n);
//!         Ok(WorkStatus::Ok)
//!     }
//! }
//! ```
//!
//! See docs/PLUGINS.md for building and installing plugins.

use std::cell::RefCell;

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "block-plugin",
        pub_export_macro: true,
        export_macro_name: "__export",
        default_bindings_module: "foxygnal_plugin_sdk::bindings",
    });
}

pub use bindings::foxygnal::block::types::{
    BlockDescriptor, FloatRange, InputTag, IntRange, ParamKind, ParamSpec, ParamValue, PluginInfo,
    PortSpec, StreamMeta, StreamType, TagMode, TagValue, WorkStatus,
};

use bindings::exports::foxygnal::block::plugin as exports;
use bindings::foxygnal::block::host;

/// Write a debug message to the engine's log (rate-limited by the host).
pub fn log(message: &str) {
    host::log(message);
}

/// A complex sample, laid out like the engine's `c32` (re, im).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[repr(C)]
pub struct Complex32 {
    pub re: f32,
    pub im: f32,
}

impl Complex32 {
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
}

impl std::ops::Add for Complex32 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.re + o.re, self.im + o.im)
    }
}

impl std::ops::Sub for Complex32 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.re - o.re, self.im - o.im)
    }
}

impl std::ops::Mul for Complex32 {
    type Output = Self;
    fn mul(self, o: Self) -> Self {
        Self::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
}

impl std::ops::Mul<f32> for Complex32 {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.re * k, self.im * k)
    }
}

mod sealed {
    pub trait Sealed {}
}

/// Item types that match a [`StreamType`].
pub trait Sample: Copy + Default + sealed::Sealed + 'static {
    const TYPE: StreamType;
}

macro_rules! sample {
    ($t:ty, $s:ident) => {
        impl sealed::Sealed for $t {}
        impl Sample for $t {
            const TYPE: StreamType = StreamType::$s;
        }
    };
}
sample!(f32, F32);
sample!(Complex32, C32);
sample!(i16, I16);
sample!(u8, U8);

impl StreamType {
    /// Size of one item in bytes.
    pub const fn item_size(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::C32 => 8,
            Self::I16 => 2,
            Self::U8 => 1,
        }
    }

    /// Parse `f32`, `c32`, `i16` or `u8` (the usual `type` parameter).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "f32" => Self::F32,
            "c32" => Self::C32,
            "i16" => Self::I16,
            "u8" => Self::U8,
            _ => return None,
        })
    }
}

impl StreamMeta {
    pub const fn with_rate(sample_rate: f64) -> Self {
        Self {
            sample_rate: Some(sample_rate),
            center_freq: None,
        }
    }
}

impl PortSpec {
    /// A port of a fixed type.
    pub fn new(name: &str, ty: StreamType) -> Self {
        Self {
            name: name.into(),
            type_: Some(ty),
        }
    }

    /// A port whose type depends on the parameters.
    pub fn any(name: &str) -> Self {
        Self {
            name: name.into(),
            type_: None,
        }
    }
}

impl ParamSpec {
    fn new(name: &str, label: &str, kind: ParamKind, default: ParamValue, help: &str) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            kind,
            default,
            help: help.into(),
            live: false,
        }
    }

    /// A float parameter (no range, no unit; see [`Self::range`], [`Self::unit`]).
    pub fn float(name: &str, label: &str, default: f64, help: &str) -> Self {
        let kind = ParamKind::Float(FloatRange {
            min: None,
            max: None,
            unit: None,
        });
        Self::new(name, label, kind, ParamValue::Float(default), help)
    }

    /// An integer parameter (see [`Self::range`]).
    pub fn int(name: &str, label: &str, default: i64, help: &str) -> Self {
        let kind = ParamKind::Int(IntRange {
            min: None,
            max: None,
        });
        Self::new(name, label, kind, ParamValue::Int(default), help)
    }

    pub fn boolean(name: &str, label: &str, default: bool, help: &str) -> Self {
        Self::new(
            name,
            label,
            ParamKind::Boolean,
            ParamValue::Boolean(default),
            help,
        )
    }

    /// One of `options`.
    pub fn choice(name: &str, label: &str, options: &[&str], default: &str, help: &str) -> Self {
        let kind = ParamKind::Choice(options.iter().map(|s| s.to_string()).collect());
        Self::new(name, label, kind, ParamValue::Text(default.into()), help)
    }

    pub fn text(name: &str, label: &str, default: &str, help: &str) -> Self {
        Self::new(
            name,
            label,
            ParamKind::Text,
            ParamValue::Text(default.into()),
            help,
        )
    }

    /// The usual stream type choice: a `type` parameter with these options.
    pub fn stream_type(options: &[StreamType], default: StreamType) -> Self {
        let names: Vec<&str> = options.iter().map(|t| type_name(*t)).collect();
        Self::choice(
            "type",
            "Type",
            &names,
            type_name(default),
            "Item type of the stream.",
        )
    }

    /// Bounds for a float or int parameter (inclusive).
    pub fn range(mut self, min: Option<f64>, max: Option<f64>) -> Self {
        match &mut self.kind {
            ParamKind::Float(r) => {
                r.min = min;
                r.max = max;
            }
            ParamKind::Int(r) => {
                r.min = min.map(|v| v as i64);
                r.max = max.map(|v| v as i64);
            }
            _ => {}
        }
        self
    }

    /// Display unit of a float parameter.
    pub fn unit(mut self, unit: &str) -> Self {
        if let ParamKind::Float(r) = &mut self.kind {
            r.unit = Some(unit.into());
        }
        self
    }

    /// Changeable while running ([`Block::set_param`]).
    pub fn live(mut self) -> Self {
        self.live = true;
        self
    }
}

fn type_name(t: StreamType) -> &'static str {
    match t {
        StreamType::F32 => "f32",
        StreamType::C32 => "c32",
        StreamType::I16 => "i16",
        StreamType::U8 => "u8",
    }
}

impl ParamValue {
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Float(v) => Some(v),
            Self::Int(v) => Some(v as f64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Int(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Boolean(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(v) => Some(v),
            _ => None,
        }
    }
}

/// The parameters a block is built with: every parameter of its
/// descriptor, defaults filled in, values checked by the engine.
pub struct Params(Vec<(String, ParamValue)>);

impl Params {
    pub fn get(&self, name: &str) -> Option<&ParamValue> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    fn need<'a, T>(
        &'a self,
        name: &str,
        f: impl Fn(&'a ParamValue) -> Option<T>,
    ) -> Result<T, String> {
        self.get(name)
            .and_then(f)
            .ok_or_else(|| format!("parameter `{name}` is missing or has the wrong type"))
    }

    pub fn f64(&self, name: &str) -> Result<f64, String> {
        self.need(name, ParamValue::as_f64)
    }

    pub fn i64(&self, name: &str) -> Result<i64, String> {
        self.need(name, ParamValue::as_i64)
    }

    pub fn bool(&self, name: &str) -> Result<bool, String> {
        self.need(name, ParamValue::as_bool)
    }

    pub fn str(&self, name: &str) -> Result<&str, String> {
        self.need(name, |v| v.as_str())
    }

    /// The `type` parameter as a stream type.
    pub fn stream_type(&self, name: &str) -> Result<StreamType, String> {
        StreamType::parse(self.str(name)?)
            .ok_or_else(|| format!("parameter `{name}`: expected f32, c32, i16 or u8"))
    }
}

/// A processing block inside a plugin.
pub trait Block: 'static {
    /// Process as much as is sensible right now. Never loop forever: the
    /// engine interrupts a call that takes too long and the block fails.
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String>;

    /// Given each input's metadata, return each output's. The default (an
    /// empty list) keeps the engine's default: every field from the first
    /// input that has it.
    fn configure(&mut self, inputs: &[StreamMeta]) -> Result<Vec<StreamMeta>, String> {
        let _ = inputs;
        Ok(Vec::new())
    }

    /// Apply a parameter flagged `live`, between `work` calls.
    fn set_param(&mut self, name: &str, value: &ParamValue) -> Result<(), String> {
        let _ = value;
        Err(format!("parameter `{name}` cannot change while running"))
    }
}

/// A built block and its concrete port types (in descriptor order).
pub struct Created {
    pub block: Box<dyn Block>,
    pub inputs: Vec<StreamType>,
    pub outputs: Vec<StreamType>,
}

/// What a plugin provides.
pub trait Plugin {
    /// Name, version and block descriptors. Called once at load.
    fn info() -> PluginInfo;
    /// Build a block of `kind`.
    fn create(kind: &str, params: &Params) -> Result<Created, String>;
}

/// An input port's readable items: borrowed from what the host passed, or
/// copied into aligned scratch space if that wasn't aligned for the type.
struct Input<'a> {
    bytes: &'a [u8],
    ty: StreamType,
    consumed: usize,
}

struct Output<'a> {
    buf: &'a mut [u8],
    ty: StreamType,
    produced: usize,
}

/// Everything a block can touch during `work`.
pub struct WorkIo<'a> {
    inputs: Vec<Input<'a>>,
    outputs: Vec<Output<'a>>,
    tags: &'a [InputTag],
}

fn cast<T: Sample>(b: &[u8]) -> &[T] {
    let size = std::mem::size_of::<T>();
    assert_eq!(b.as_ptr() as usize % std::mem::align_of::<T>(), 0);
    // SAFETY: aligned (checked), every bit pattern is a valid T (plain
    // numbers), and the length is rounded down to whole items.
    unsafe { std::slice::from_raw_parts(b.as_ptr().cast(), b.len() / size) }
}

fn cast_mut<T: Sample>(b: &mut [u8]) -> &mut [T] {
    let size = std::mem::size_of::<T>();
    assert_eq!(b.as_ptr() as usize % std::mem::align_of::<T>(), 0);
    // SAFETY: as in `cast`, and the borrow is exclusive.
    unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr().cast(), b.len() / size) }
}

impl WorkIo<'_> {
    /// Number of input ports.
    pub fn num_inputs(&self) -> usize {
        self.inputs.len()
    }

    /// Number of output ports.
    pub fn num_outputs(&self) -> usize {
        self.outputs.len()
    }

    /// Readable items of input `port`. Panics if `T` doesn't match its type.
    pub fn input<T: Sample>(&self, port: usize) -> &[T] {
        let i = &self.inputs[port];
        assert!(i.ty == T::TYPE, "input {port} type mismatch");
        cast(i.bytes)
    }

    /// Room on output `port`. Panics if `T` doesn't match its type. The
    /// contents are not zeroed: write before you produce.
    pub fn output<T: Sample>(&mut self, port: usize) -> &mut [T] {
        let o = &mut self.outputs[port];
        assert!(o.ty == T::TYPE, "output {port} type mismatch");
        cast_mut(o.buf)
    }

    /// The common 1-in/1-out case: input slice and output slice.
    pub fn in_out<I: Sample, O: Sample>(&mut self, i: usize, o: usize) -> (&[I], &mut [O]) {
        let inp = &self.inputs[i];
        assert!(inp.ty == I::TYPE, "input {i} type mismatch");
        let out = &mut self.outputs[o];
        assert!(out.ty == O::TYPE, "output {o} type mismatch");
        (cast(inp.bytes), cast_mut(out.buf))
    }

    /// Mark the first `items` readable items of `port` consumed.
    pub fn consume(&mut self, port: usize, items: usize) {
        self.inputs[port].consumed = items;
    }

    /// Mark the first `items` items of `port`'s output slice produced.
    pub fn produce(&mut self, port: usize, items: usize) {
        self.outputs[port].produced = items;
    }

    /// Tags on the readable items (manual tag mode only; empty otherwise).
    /// `index` counts items from the start of the input slice.
    pub fn tags(&self) -> &[InputTag] {
        self.tags
    }

    /// Tag item `index` of output `port`'s slice (manual tag mode only).
    pub fn add_tag(&mut self, port: usize, index: usize, key: &str, value: TagValue) {
        host::add_tag(port as u32, index as u32, key, &value);
    }
}

/// Scratch space a block reuses across `work` calls (no allocation once
/// warmed up). `u64` storage keeps every item type aligned.
#[derive(Default)]
struct Scratch {
    inputs: Vec<Vec<u64>>,
    outputs: Vec<Vec<u64>>,
}

fn words(bytes: usize) -> usize {
    bytes.div_ceil(8)
}

fn as_bytes_mut(v: &mut [u64]) -> &mut [u8] {
    // SAFETY: u8 has no alignment or validity requirements.
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr().cast(), v.len() * 8) }
}

/// The resource the host holds for each block.
#[doc(hidden)]
pub struct Instance {
    block: RefCell<Box<dyn Block>>,
    inputs: Vec<StreamType>,
    outputs: Vec<StreamType>,
    scratch: RefCell<Scratch>,
}

impl exports::GuestBlock for Instance {
    fn configure(&self, inputs: Vec<StreamMeta>) -> Result<Vec<StreamMeta>, String> {
        self.block.borrow_mut().configure(&inputs)
    }

    fn set_param(&self, name: String, value: ParamValue) -> Result<(), String> {
        self.block.borrow_mut().set_param(&name, &value)
    }

    fn work(
        &self,
        inputs: Vec<Vec<u8>>,
        space: Vec<u32>,
        tags: Vec<InputTag>,
    ) -> Result<WorkStatus, String> {
        if inputs.len() != self.inputs.len() || space.len() != self.outputs.len() {
            return Err("host passed the wrong number of ports".into());
        }
        let mut scratch = self.scratch.borrow_mut();
        let Scratch {
            inputs: in_scratch,
            outputs: out_scratch,
        } = &mut *scratch;
        in_scratch.resize_with(inputs.len(), Vec::new);
        out_scratch.resize_with(space.len(), Vec::new);

        let mut io = WorkIo {
            inputs: Vec::with_capacity(inputs.len()),
            outputs: Vec::with_capacity(space.len()),
            tags: &tags,
        };
        for ((bytes, ty), s) in inputs.iter().zip(&self.inputs).zip(in_scratch.iter_mut()) {
            let bytes: &[u8] = if (bytes.as_ptr() as usize).is_multiple_of(8) {
                bytes
            } else {
                // Rare: the host's list wasn't aligned for the item type.
                s.clear();
                s.resize(words(bytes.len()), 0);
                let b = &mut as_bytes_mut(s)[..bytes.len()];
                b.copy_from_slice(bytes);
                b
            };
            io.inputs.push(Input {
                bytes,
                ty: *ty,
                consumed: 0,
            });
        }
        for ((n, ty), s) in space.iter().zip(&self.outputs).zip(out_scratch.iter_mut()) {
            let len = *n as usize * ty.item_size();
            if s.len() < words(len) {
                s.resize(words(len), 0);
            }
            io.outputs.push(Output {
                buf: &mut as_bytes_mut(s)[..len],
                ty: *ty,
                produced: 0,
            });
        }

        let status = self.block.borrow_mut().work(&mut io)?;

        for (port, o) in io.outputs.iter().enumerate() {
            let len = o.produced * o.ty.item_size();
            if len > o.buf.len() {
                return Err(format!(
                    "produced {} items on output {port}, but there was room for {}",
                    o.produced,
                    o.buf.len() / o.ty.item_size()
                ));
            }
            if len > 0 {
                host::emit(port as u32, &o.buf[..len]);
            }
        }
        for (port, i) in io.inputs.iter().enumerate() {
            if i.consumed > 0 {
                host::consume(port as u32, i.consumed as u32);
            }
        }
        Ok(status)
    }
}

#[doc(hidden)]
pub fn __create<P: Plugin>(
    kind: &str,
    params: Vec<(String, ParamValue)>,
) -> Result<exports::Created, String> {
    let c = P::create(kind, &Params(params))?;
    let (inputs, outputs) = (c.inputs.clone(), c.outputs.clone());
    Ok(exports::Created {
        block: exports::Block::new(Instance {
            block: RefCell::new(c.block),
            inputs: c.inputs,
            outputs: c.outputs,
            scratch: RefCell::default(),
        }),
        inputs,
        outputs,
    })
}

/// Export a [`Plugin`] implementation as the component's entry points.
/// Use it once, at the top level of the plugin crate.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        const _: () = {
            struct __FoxygnalPlugin;
            impl $crate::bindings::exports::foxygnal::block::plugin::Guest for __FoxygnalPlugin {
                type Block = $crate::Instance;
                fn info() -> $crate::PluginInfo {
                    <$plugin as $crate::Plugin>::info()
                }
                fn create(
                    kind: ::std::string::String,
                    params: ::std::vec::Vec<(::std::string::String, $crate::ParamValue)>,
                ) -> ::std::result::Result<
                    $crate::bindings::exports::foxygnal::block::plugin::Created,
                    ::std::string::String,
                > {
                    $crate::__create::<$plugin>(&kind, params)
                }
            }
    $crate::bindings::__export!(__FoxygnalPlugin with_types_in $crate::bindings);
        };
    };
}
