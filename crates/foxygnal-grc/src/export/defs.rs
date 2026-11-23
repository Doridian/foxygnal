//! The GNU Radio 3.10 blocks the exporter writes, with their parameter
//! names, checked against the block definitions (`*.block.yml`, maint-3.10;
//! gr-osmosdr for `osmosdr_*`). The comment above each entry names the file
//! and its stream ports. Tests check that every exported block and
//! parameter is listed here. `epy_block` and `options` are GRC built-ins.

/// (block id, parameter names).
pub(crate) const BLOCKS: &[(&str, &[&str])] = &[
    // analog_sig_source_x.block.yml: in [] out ['${ type }']
    (
        "analog_sig_source_x",
        &[
            "type",
            "samp_rate",
            "waveform",
            "freq",
            "amp",
            "offset",
            "phase",
            "showports",
        ],
    ),
    // analog_const_source_x.block.yml: in [] out ['${ type }']
    ("analog_const_source_x", &["type", "const"]),
    // analog_noise_source_x.block.yml: in [] out ['${ type }']
    (
        "analog_noise_source_x",
        &["type", "noise_type", "amp", "seed"],
    ),
    // blocks_file_source.block.yml: in [] out ['${ type }']
    (
        "blocks_file_source",
        &[
            "file",
            "type",
            "repeat",
            "vlen",
            "begin_tag",
            "offset",
            "length",
        ],
    ),
    // blocks_file_sink.block.yml: in ['${ type }'] out []
    (
        "blocks_file_sink",
        &["file", "type", "vlen", "unbuffered", "append"],
    ),
    // blocks_wavfile_source.block.yml: in [] out ['float*${ nchan }']
    ("blocks_wavfile_source", &["file", "repeat", "nchan"]),
    // blocks_wavfile_sink.block.yml: in ['float*${ nchan }'] out []
    (
        "blocks_wavfile_sink",
        &[
            "file",
            "nchan",
            "samp_rate",
            "format",
            "bits_per_sample1",
            "bits_per_sample2",
            "bits_per_sample3",
            "bits_per_sample4",
            "append",
        ],
    ),
    // blocks_null_sink.block.yml: in ['${ type }*${ num_inputs }'] out []
    (
        "blocks_null_sink",
        &["type", "vlen", "num_inputs", "bus_structure_sink"],
    ),
    // blocks_throttle2.block.yml: in ['${ type }'] out ['${ type }?']
    (
        "blocks_throttle2",
        &[
            "type",
            "samples_per_second",
            "vlen",
            "ignoretag",
            "limit",
            "maximum",
        ],
    ),
    // blocks_head.block.yml: in ['${ type }'] out ['${ type }?']
    ("blocks_head", &["type", "num_items", "vlen"]),
    // blocks_skiphead.block.yml: in ['${ type }'] out ['${ type }']
    ("blocks_skiphead", &["type", "num_items", "vlen"]),
    // blocks_delay.block.yml: in ['${ type }*${ num_ports }'] out ['${ type }*${ num_ports }']
    (
        "blocks_delay",
        &["type", "delay", "num_ports", "vlen", "showports"],
    ),
    // blocks_copy.block.yml: in ['${ type }'] out ['${ type }']
    ("blocks_copy", &["type", "enabled", "vlen", "showports"]),
    // blocks_selector.block.yml: in ['${ type }*${ num_inputs }'] out ['${ type }*${ num_outputs }']
    (
        "blocks_selector",
        &[
            "type",
            "enabled",
            "num_inputs",
            "num_outputs",
            "input_index",
            "output_index",
            "vlen",
            "showports",
        ],
    ),
    // blocks_add_xx.block.yml: in ['${ type }*${ num_inputs }'] out ['${ type }']
    ("blocks_add_xx", &["type", "num_inputs", "vlen"]),
    // blocks_sub_xx.block.yml: in ['${ type }*${ num_inputs }'] out ['${ type }']
    ("blocks_sub_xx", &["type", "vlen", "num_inputs"]),
    // blocks_multiply_xx.block.yml: in ['${ type }*${ num_inputs }'] out ['${ type }']
    ("blocks_multiply_xx", &["type", "num_inputs", "vlen"]),
    // blocks_divide_XX.block.yml: in ['${ type }*${ num_inputs }'] out ['${ type }']
    ("blocks_divide_xx", &["type", "vlen", "num_inputs"]),
    // blocks_multiply_const_xx.block.yml: in ['${ type }'] out ['${ type }']
    ("blocks_multiply_const_xx", &["type", "const", "vlen"]),
    // blocks_add_const_vxx.block.yml: in ['${ type }'] out ['${ type }']
    ("blocks_add_const_vxx", &["type", "const", "vlen"]),
    // blocks_conjugate_cc.block.yml: in ['complex'] out ['complex']
    ("blocks_conjugate_cc", &[]),
    // blocks_abs_xx.block.yml: in ['${ type }'] out ['${ type }']
    ("blocks_abs_xx", &["vlen", "type"]),
    // blocks_nlog10_ff.block.yml: in ['float'] out ['float']
    ("blocks_nlog10_ff", &["n", "k", "vlen"]),
    // blocks_complex_to_mag.block.yml: in ['complex'] out ['float']
    ("blocks_complex_to_mag", &["vlen"]),
    // blocks_complex_to_mag_squared.block.yml: in ['complex'] out ['float']
    ("blocks_complex_to_mag_squared", &["vlen"]),
    // blocks_complex_to_real.block.yml: in ['complex'] out ['float']
    ("blocks_complex_to_real", &["vlen"]),
    // blocks_complex_to_imag.block.yml: in ['complex'] out ['float']
    ("blocks_complex_to_imag", &["vlen"]),
    // blocks_complex_to_arg.block.yml: in ['complex'] out ['float']
    ("blocks_complex_to_arg", &["vlen"]),
    // blocks_complex_to_float.block.yml: in ['complex'] out ['float', 'float?']
    ("blocks_complex_to_float", &["vlen"]),
    // blocks_float_to_complex.block.yml: in ['float', 'float?'] out ['complex']
    ("blocks_float_to_complex", &["vlen"]),
    // blocks_short_to_float.block.yml: in ['short'] out ['float']
    ("blocks_short_to_float", &["vlen", "scale"]),
    // blocks_float_to_short.block.yml: in ['float'] out ['short']
    ("blocks_float_to_short", &["vlen", "scale"]),
    // blocks_uchar_to_float.block.yml: in ['byte'] out ['float']
    ("blocks_uchar_to_float", &[]),
    // blocks_interleaved_short_to_complex.block.yml: in ['short'] out ['complex']
    (
        "blocks_interleaved_short_to_complex",
        &["scale_factor", "vector_input", "swap"],
    ),
    // blocks_deinterleave.block.yml: in ['${ type }'] out ['${ type }*${ num_streams }']
    (
        "blocks_deinterleave",
        &["type", "num_streams", "blocksize", "vlen"],
    ),
    // filter_low_pass_filter.block.yml: in ['${ type.input }'] out ['${ type.output }']
    (
        "low_pass_filter",
        &[
            "type",
            "decim",
            "interp",
            "gain",
            "samp_rate",
            "cutoff_freq",
            "width",
            "win",
            "beta",
        ],
    ),
    // filter_high_pass_filter.block.yml: in ['${ type.input }'] out ['${ type.output }']
    (
        "high_pass_filter",
        &[
            "type",
            "decim",
            "interp",
            "gain",
            "samp_rate",
            "cutoff_freq",
            "width",
            "win",
            "beta",
        ],
    ),
    // filter_band_pass_filter.block.yml: in ['${ type.input }'] out ['${ type.output }']
    (
        "band_pass_filter",
        &[
            "type",
            "decim",
            "interp",
            "gain",
            "samp_rate",
            "low_cutoff_freq",
            "high_cutoff_freq",
            "width",
            "win",
            "beta",
        ],
    ),
    // filter_band_reject_filter.block.yml: in ['${ type.input }'] out ['${ type.output }']
    (
        "band_reject_filter",
        &[
            "type",
            "decim",
            "interp",
            "gain",
            "samp_rate",
            "low_cutoff_freq",
            "high_cutoff_freq",
            "width",
            "win",
            "beta",
        ],
    ),
    // filter_freq_xlating_fir_filter_xxx.block.yml: in ['${ type.input }'] out ['${ type.output }']
    (
        "freq_xlating_fir_filter_xxx",
        &["type", "decim", "taps", "center_freq", "samp_rate"],
    ),
    // filter_rational_resampler_xxx.block.yml: in ['${ type.input }'] out ['${ type.output }']
    (
        "rational_resampler_xxx",
        &["type", "interp", "decim", "taps", "fbw"],
    ),
    // filter_root_raised_cosine_filter.block.yml: in ['${ type.input }'] out ['${ type.output }']
    (
        "root_raised_cosine_filter",
        &[
            "type",
            "decim",
            "interp",
            "gain",
            "samp_rate",
            "sym_rate",
            "alpha",
            "ntaps",
        ],
    ),
    // filter_iir_filter_xxx.block.yml: in ['${ type.input }'] out ['${ type.output }']
    ("iir_filter_xxx", &["type", "fftaps", "fbtaps", "oldstyle"]),
    // filter_hilbert_fc.block.yml: in ['float'] out ['complex']
    ("hilbert_fc", &["num_taps", "win", "beta"]),
    // analog_quadrature_demod_cf.block.yml: in ['complex'] out ['float']
    ("analog_quadrature_demod_cf", &["gain"]),
    // analog_fm_deemph.block.yml: in ['float'] out ['float']
    ("analog_fm_deemph", &["samp_rate", "tau"]),
    // analog_am_demod_cf.block.yml: in ['complex'] out ['float']
    (
        "analog_am_demod_cf",
        &["chan_rate", "audio_decim", "audio_pass", "audio_stop"],
    ),
    // analog_agc2_xx.block.yml: in ['${ type }'] out ['${ type }']
    (
        "analog_agc2_xx",
        &[
            "type",
            "attack_rate",
            "decay_rate",
            "reference",
            "gain",
            "max_gain",
        ],
    ),
    // analog_pwr_squelch_xx.block.yml: in ['${ type }'] out ['${ type }']
    (
        "analog_pwr_squelch_xx",
        &["type", "threshold", "alpha", "ramp", "gate"],
    ),
    // audio_sink.block.yml: in ['float*${ num_inputs }'] out []
    (
        "audio_sink",
        &["samp_rate", "device_name", "ok_to_block", "num_inputs"],
    ),
    // audio_source.block.yml: in [] out ['float*${ num_outputs }']
    (
        "audio_source",
        &["samp_rate", "device_name", "ok_to_block", "num_outputs"],
    ),
    // soapy_plutosdr_source.block.yml: in [] out ['${ type.type }*1']
    (
        "soapy_plutosdr_source",
        &[
            "type",
            "dev_args",
            "samp_rate",
            "bandwidth",
            "center_freq",
            "agc",
            "gain",
        ],
    ),
    // soapy_custom_source.block.yml: in [] out ['${ type.type }*${ nchan }']
    (
        "soapy_custom_source",
        &[
            "driver",
            "type",
            "nchan",
            "dev_args",
            "stream_args",
            "samp_rate",
            "tune_args0",
            "settings0",
            "bandwidth0",
            "antenna0",
            "center_freq0",
            "freq_correction0",
            "agc0",
            "gain0",
            "dc_removal0",
            "dc_offset0",
            "iq_balance0",
            "tune_args1",
            "settings1",
            "bandwidth1",
            "antenna1",
            "center_freq1",
            "freq_correction1",
            "agc1",
            "gain1",
            "dc_removal1",
            "dc_offset1",
            "iq_balance1",
        ],
    ),
    // soapy_custom_sink.block.yml: in ['${ type.type }*${ nchan }'] out []
    (
        "soapy_custom_sink",
        &[
            "driver",
            "type",
            "nchan",
            "dev_args",
            "stream_args",
            "samp_rate",
            "tune_args0",
            "settings0",
            "bandwidth0",
            "antenna0",
            "center_freq0",
            "freq_correction0",
            "gain0",
            "dc_offset0",
            "iq_balance0",
            "tune_args1",
            "settings1",
            "bandwidth1",
            "antenna1",
            "center_freq1",
            "freq_correction1",
            "gain1",
            "dc_offset1",
            "iq_balance1",
        ],
    ),
    // network_udp_source.block.yml: in [] out ['${ type }']
    (
        "network_udp_source",
        &[
            "type",
            "port",
            "header",
            "payloadsize",
            "notify_missed",
            "src_zeros",
            "ipv6",
            "vlen",
        ],
    ),
    // network_udp_sink.block.yml: in ['${ type }'] out []
    (
        "network_udp_sink",
        &[
            "type",
            "addr",
            "port",
            "header",
            "payloadsize",
            "send_eof",
            "vlen",
        ],
    ),
    // network_tcp_source.block.yml: in [] out ['${ type }']
    (
        "network_tcp_source",
        &["type", "addr", "port", "server", "vlen"],
    ),
    // network_tcp_sink.block.yml: in ['${ type }'] out []
    (
        "network_tcp_sink",
        &["type", "mode", "addr", "port", "vlen"],
    ),
    // digital_costas_loop_cc.block.yml: in ['complex'] out ['complex', 'float?', 'float?', 'float?']
    ("digital_costas_loop_cc", &["w", "order", "use_snr"]),
    // digital_symbol_sync_xx.block.yml: in ['${ type.input }'] out ['${ type.output }', 'float?', 'float?', 'float?']
    (
        "digital_symbol_sync_xx",
        &[
            "type",
            "ted_type",
            "constellation",
            "sps",
            "ted_gain",
            "loop_bw",
            "damping",
            "max_dev",
            "osps",
            "resamp_type",
            "nfilters",
            "pfb_mf_taps",
        ],
    ),
    // digital_binary_slicer_fb.block.yml: in ['float'] out ['byte']
    ("digital_binary_slicer_fb", &[]),
    // digital_correlate_access_code_tag_xx.block.yml: in ['${ type }'] out ['${ type }']
    (
        "digital_correlate_access_code_tag_xx",
        &["type", "access_code", "threshold", "tagname"],
    ),
    // variable.block.yml: in [] out []
    ("variable", &["value"]),
    // import.block.yml: in [] out []
    ("import_", &["imports"]),
    // note.block.yml: in [] out []
    ("note", &["note"]),
    // filter_dc_blocker_xx.block.yml: in ['${ type.input }'] out ['${ type.output }']
    ("dc_blocker_xx", &["type", "length", "long_form"]),
    // osmosdr_source.block.yml: in [] out ['${type.type}*${nchan}']
    (
        "osmosdr_source",
        &[
            "type",
            "args",
            "sync",
            "num_mboards",
            "clock_source0",
            "time_source0",
            "nchan",
            "sample_rate",
            "freq0",
            "corr0",
            "dc_offset_mode0",
            "iq_balance_mode0",
            "gain_mode0",
            "gain0",
            "if_gain0",
            "bb_gain0",
            "ant0",
            "bw0",
            "bw31",
        ],
    ),
    // osmosdr_sink.block.yml: in ['${type.type}*${nchan}'] out []
    (
        "osmosdr_sink",
        &[
            "type",
            "args",
            "sync",
            "num_mboards",
            "clock_source0",
            "time_source0",
            "nchan",
            "sample_rate",
            "freq0",
            "corr0",
            "gain0",
            "if_gain0",
            "bb_gain0",
            "ant0",
            "bw0",
            "bw31",
        ],
    ),
    // grc/core/blocks/embedded_python.py (built in): ports from the source code
    ("epy_block", &["_source_code", "comment"]),
];

/// Parameters every block has (grc/core/blocks/_flags / block.py).
pub(crate) const COMMON: &[&str] = &["alias", "comment", "affinity", "minoutbuf", "maxoutbuf"];

/// The parameter names of a block, if it is one the exporter may write.
pub(crate) fn params(id: &str) -> Option<&'static [&'static str]> {
    BLOCKS.iter().find(|(b, _)| *b == id).map(|(_, p)| *p)
}

/// What isn't in the table: the block id, or a parameter.
pub(crate) fn unknown<'a>(id: &str, keys: impl IntoIterator<Item = &'a String>) -> Option<String> {
    let Some(known) = params(id) else {
        return Some(format!("block `{id}`"));
    };
    keys.into_iter()
        .find(|k| !known.contains(&k.as_str()) && !COMMON.contains(&k.as_str()))
        .map(|k| format!("parameter `{k}` of `{id}`"))
}
