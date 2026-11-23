//! Stream metadata: what a stream *is* (sample rate, centre frequency), worked
//! out once before the graph starts and passed downstream block by block.

use serde::{Deserialize, Serialize};

/// Facts about a stream that blocks downstream may need.
///
/// Sources state them (a signal generator knows its rate, a SigMF file knows
/// its rate and centre frequency), and every block passes them on, changing
/// what it changes: a decimate-by-4 block divides the rate by four, a
/// frequency translator moves the centre frequency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamMeta {
    /// Samples per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<f64>,
    /// The RF frequency that 0 Hz in this stream corresponds to, in Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_freq: Option<f64>,
}

impl StreamMeta {
    pub fn with_rate(sample_rate: f64) -> Self {
        Self {
            sample_rate: Some(sample_rate),
            center_freq: None,
        }
    }

    /// Each field taken from the first input that has it.
    pub fn merge(inputs: &[StreamMeta]) -> Self {
        Self {
            sample_rate: inputs.iter().find_map(|m| m.sample_rate),
            center_freq: inputs.iter().find_map(|m| m.center_freq),
        }
    }
}

/// What [`crate::Block::configure`] sees: the metadata of every input, and
/// the metadata of every output to fill in. Outputs start as
/// [`StreamMeta::merge`] of the inputs, so most blocks change nothing.
pub struct Configure<'a> {
    pub inputs: &'a [StreamMeta],
    pub outputs: &'a mut [StreamMeta],
}

impl Configure<'_> {
    /// The input sample rate, or an error a user can act on.
    pub fn input_rate(&self, port: usize) -> Result<f64, crate::Error> {
        self.inputs
            .get(port)
            .and_then(|m| m.sample_rate)
            .ok_or_else(|| {
                crate::Error::Other(
                    "needs to know the sample rate, but nothing upstream sets it \
                     (set one on the source)"
                        .into(),
                )
            })
    }

    /// Set every output's sample rate.
    pub fn set_rate(&mut self, rate: f64) {
        for o in self.outputs.iter_mut() {
            o.sample_rate = Some(rate);
        }
    }
}
