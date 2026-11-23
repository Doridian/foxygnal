//! The transfer frame layer: AOS transfer frames (VCDUs, CCSDS 732.0-B), the
//! multiplexing protocol data unit (M_PDU) in their data field, and CCSDS
//! space packets (CCSDS 133.0-B) reassembled across frames, per virtual
//! channel.
//!
//! VCDU primary header (6 bytes): version (2 bits, 01), spacecraft id (8),
//! virtual channel id (6); VC frame count (24); replay flag (1), VC frame
//! count usage flag (1), spare (2), VC frame count cycle (4). Then a mission's
//! optional fields (frame header error control, insert zone), which
//! [`Layout::insert_zone`] skips, and the data field. VCID 63 carries fill.
//!
//! M_PDU header (2 bytes): 5 spare bits and the 11-bit first header pointer:
//! where in the packet zone the first packet starts; `0x7FF` = none starts in
//! this frame (the zone continues a packet), `0x7FE` = idle data only.
//!
//! Space packet primary header (6 bytes): version (3), type (1), secondary
//! header flag (1), APID (11); sequence flags (2), sequence count (14); data
//! length − 1 (16). APID `0x7FF` is an idle packet.
//!
//! Reassembly: per virtual channel, packets are cut from the stream of packet
//! zones. A gap in the VC frame count (or a frame that couldn't be
//! corrected, which never reaches here) drops the packet in progress; the
//! next frame's first header pointer finds the packet boundary again, and the
//! first packet after the gap is flagged ([`Packet::after_gap`]). A first
//! header pointer that disagrees with where the packet in progress ends is
//! taken as a gap too.

/// The longest space packet (6 + 65536 bytes).
pub const MAX_PACKET: usize = 6 + 65536;
/// VCDU primary header bytes.
pub const VCDU_HEADER: usize = 6;
/// The fill virtual channel.
pub const VCID_FILL: u8 = 63;
/// The idle APID.
pub const APID_IDLE: u16 = 0x7FF;
/// First header pointer: no packet starts in this frame.
pub const FHP_NONE: u16 = 0x7FF;
/// First header pointer: idle data only.
pub const FHP_IDLE: u16 = 0x7FE;

/// The VCDU primary header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcduHeader {
    pub version: u8,
    pub spacecraft: u8,
    pub vcid: u8,
    /// VC frame count (24 bits).
    pub counter: u32,
    pub replay: bool,
}

impl VcduHeader {
    pub fn parse(frame: &[u8]) -> Option<Self> {
        let h = frame.get(..VCDU_HEADER)?;
        Some(Self {
            version: h[0] >> 6,
            spacecraft: (h[0] << 2) | (h[1] >> 6),
            vcid: h[1] & 0x3F,
            counter: u32::from_be_bytes([0, h[2], h[3], h[4]]),
            replay: h[5] & 0x80 != 0,
        })
    }

    /// The 6 header bytes (signalling field: replay flag only).
    pub fn write(&self, out: &mut [u8]) {
        out[0] = ((self.version & 3) << 6) | (self.spacecraft >> 2);
        out[1] = (self.spacecraft << 6) | (self.vcid & 0x3F);
        let c = self.counter.to_be_bytes();
        out[2..5].copy_from_slice(&c[1..]);
        out[5] = if self.replay { 0x80 } else { 0 };
    }
}

/// A space packet's primary header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub version: u8,
    /// 0 telemetry, 1 telecommand.
    pub kind: u8,
    pub secondary_header: bool,
    pub apid: u16,
    /// 0b01 first, 0b00 continuation, 0b10 last, 0b11 unsegmented.
    pub seq_flags: u8,
    pub seq: u16,
    /// Bytes after the primary header.
    pub data_len: usize,
}

impl PacketHeader {
    pub fn parse(h: &[u8]) -> Option<Self> {
        let h = h.get(..6)?;
        Some(Self {
            version: h[0] >> 5,
            kind: (h[0] >> 4) & 1,
            secondary_header: h[0] & 0x08 != 0,
            apid: u16::from_be_bytes([h[0] & 7, h[1]]),
            seq_flags: h[2] >> 6,
            seq: u16::from_be_bytes([h[2] & 0x3F, h[3]]),
            data_len: usize::from(u16::from_be_bytes([h[4], h[5]])) + 1,
        })
    }

    /// Whole packet length.
    pub fn len(&self) -> usize {
        6 + self.data_len
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// The 6 header bytes. `data_len` must be 1 to 65536.
    pub fn write(&self, out: &mut [u8]) {
        let id = (u16::from(self.version & 7) << 13)
            | (u16::from(self.kind & 1) << 12)
            | (u16::from(self.secondary_header) << 11)
            | (self.apid & 0x7FF);
        out[..2].copy_from_slice(&id.to_be_bytes());
        let seq = (u16::from(self.seq_flags & 3) << 14) | (self.seq & 0x3FFF);
        out[2..4].copy_from_slice(&seq.to_be_bytes());
        out[4..6].copy_from_slice(&((self.data_len - 1) as u16).to_be_bytes());
    }
}

/// Where things are in a transfer frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Transfer frame bytes (header to trailer; RS parity not included).
    pub frame_len: usize,
    /// Bytes between the primary header and the M_PDU header (frame header
    /// error control, insert zone).
    pub insert_zone: usize,
    /// Bytes at the end of the frame that aren't packet zone (operational
    /// control field, frame error control).
    pub trailer: usize,
}

impl Layout {
    /// The packet zone's start and end in a transfer frame.
    pub fn zone(&self) -> (usize, usize) {
        (
            VCDU_HEADER + self.insert_zone + 2,
            self.frame_len - self.trailer,
        )
    }

    pub fn check(&self) -> Result<(), String> {
        let (a, b) = self.zone();
        if b <= a {
            return Err(format!(
                "a {}-byte transfer frame has no room for packets after a {}-byte insert zone \
                 and a {}-byte trailer",
                self.frame_len, self.insert_zone, self.trailer
            ));
        }
        if b - a > 2046 {
            return Err(format!(
                "the packet zone ({} bytes) is longer than a first header pointer can address \
                 (2046)",
                b - a
            ));
        }
        Ok(())
    }
}

/// A reassembled packet.
#[derive(Debug)]
pub struct Packet<'a> {
    pub spacecraft: u8,
    pub vcid: u8,
    pub header: PacketHeader,
    /// Data was lost on this virtual channel since the previous packet.
    pub after_gap: bool,
    /// The whole packet, primary header included.
    pub bytes: &'a [u8],
}

#[derive(Default)]
struct Channel {
    seen: bool,
    counter: u32,
    /// The packet in progress.
    buf: Vec<u8>,
    /// Inside the packet stream (know where packets start).
    synced: bool,
    gap: bool,
}

/// Counters for the UI.
#[derive(Debug, Clone, Copy, Default)]
pub struct AssemblerStats {
    pub frames: u64,
    pub fill_frames: u64,
    pub packets: u64,
    pub idle_packets: u64,
    /// Frames missing according to the VC frame counts.
    pub frames_lost: u64,
    /// Partial packets dropped (gaps, inconsistent first header pointers).
    pub packets_dropped: u64,
}

/// Cuts packets out of transfer frames.
pub struct PacketAssembler {
    layout: Layout,
    channels: Vec<Channel>,
    stats: AssemblerStats,
    /// Pass idle packets on too.
    pub keep_idle: bool,
}

impl PacketAssembler {
    pub fn new(layout: Layout) -> Result<Self, String> {
        layout.check()?;
        Ok(Self {
            layout,
            channels: (0..64).map(|_| Channel::default()).collect(),
            stats: AssemblerStats::default(),
            keep_idle: false,
        })
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn stats(&self) -> &AssemblerStats {
        &self.stats
    }

    /// Forget everything in progress (a new stream).
    pub fn reset(&mut self) {
        for c in &mut self.channels {
            c.seen = false;
            c.synced = false;
            c.gap = false;
            c.buf.clear();
        }
    }

    /// One transfer frame (at least `frame_len` bytes; more, e.g. RS parity,
    /// is ignored) that passed its checks. Complete packets go to `out`.
    pub fn push_frame(&mut self, frame: &[u8], mut out: impl FnMut(Packet<'_>)) {
        let Some(hdr) = VcduHeader::parse(frame) else {
            return;
        };
        if frame.len() < self.layout.frame_len {
            return;
        }
        self.stats.frames += 1;
        if hdr.vcid == VCID_FILL {
            self.stats.fill_frames += 1;
            return;
        }
        let (zs, ze) = self.layout.zone();
        let zone = &frame[zs..ze];
        let fhp = u16::from_be_bytes([frame[zs - 2], frame[zs - 1]]) & 0x7FF;
        let keep_idle = self.keep_idle;
        let stats = &mut self.stats;
        let ch = &mut self.channels[usize::from(hdr.vcid)];

        if ch.seen {
            let expect = (ch.counter + 1) & 0xFF_FFFF;
            if hdr.counter != expect {
                let lost = hdr.counter.wrapping_sub(expect) & 0xFF_FFFF;
                stats.frames_lost += u64::from(lost);
                Self::drop_partial(ch, stats);
            }
        }
        ch.seen = true;
        ch.counter = hdr.counter;
        if fhp == FHP_IDLE {
            return;
        }
        let mut emit = |ch: &mut Channel, stats: &mut AssemblerStats| {
            let Some(h) = PacketHeader::parse(&ch.buf) else {
                return;
            };
            if h.apid == APID_IDLE {
                stats.idle_packets += 1;
                if !keep_idle {
                    return;
                }
            } else {
                stats.packets += 1;
            }
            out(Packet {
                spacecraft: hdr.spacecraft,
                vcid: hdr.vcid,
                header: h,
                after_gap: std::mem::take(&mut ch.gap),
                bytes: &ch.buf,
            });
        };

        let start = if fhp == FHP_NONE {
            zone.len()
        } else {
            usize::from(fhp)
        };
        if start > zone.len() {
            // A pointer past the zone: nothing here can be trusted.
            Self::drop_partial(ch, stats);
            return;
        }
        if ch.synced {
            // The packet in progress continues up to `start`.
            Self::feed(ch, stats, &zone[..start], &mut emit);
            if start < zone.len() && !ch.buf.is_empty() {
                // It should have ended exactly where the next one starts.
                Self::drop_partial(ch, stats);
            }
        }
        if start < zone.len() {
            ch.synced = true;
            Self::feed(ch, stats, &zone[start..], &mut emit);
        }
    }

    fn drop_partial(ch: &mut Channel, stats: &mut AssemblerStats) {
        stats.packets_dropped += u64::from(!ch.buf.is_empty());
        ch.buf.clear();
        ch.synced = false;
        ch.gap = true;
    }

    /// Append packet-stream bytes, emitting each packet as it completes.
    fn feed(
        ch: &mut Channel,
        stats: &mut AssemblerStats,
        mut bytes: &[u8],
        emit: &mut impl FnMut(&mut Channel, &mut AssemblerStats),
    ) {
        while !bytes.is_empty() {
            let need = if ch.buf.len() < 6 {
                6 - ch.buf.len()
            } else {
                let len = 7 + usize::from(u16::from_be_bytes([ch.buf[4], ch.buf[5]]));
                len - ch.buf.len()
            };
            let n = need.min(bytes.len());
            if ch.buf.capacity() == 0 {
                ch.buf.reserve(8192);
            }
            ch.buf.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if ch.buf.len() >= 6 {
                let len = 7 + usize::from(u16::from_be_bytes([ch.buf[4], ch.buf[5]]));
                if ch.buf.len() == len {
                    emit(ch, stats);
                    ch.buf.clear();
                }
            }
        }
    }
}

/// Builds transfer frames from packets: the transmit side of
/// [`PacketAssembler`], for one virtual channel.
pub struct FrameBuilder {
    layout: Layout,
    header: VcduHeader,
    /// The frame being filled.
    frame: Vec<u8>,
    /// Bytes of the packet zone filled.
    fill: usize,
    /// No packet has started in the frame being filled yet.
    no_start: bool,
}

impl FrameBuilder {
    pub fn new(layout: Layout, spacecraft: u8, vcid: u8) -> Result<Self, String> {
        layout.check()?;
        let mut b = Self {
            layout,
            header: VcduHeader {
                version: 1,
                spacecraft,
                vcid,
                counter: 0,
                replay: false,
            },
            frame: vec![0; layout.frame_len],
            fill: 0,
            no_start: true,
        };
        b.begin();
        Ok(b)
    }

    fn zone_len(&self) -> usize {
        let (a, b) = self.layout.zone();
        b - a
    }

    fn begin(&mut self) {
        self.frame.fill(0);
        self.header.write(&mut self.frame);
        self.fill = 0;
        self.no_start = true;
        self.set_fhp(FHP_NONE);
    }

    fn set_fhp(&mut self, fhp: u16) {
        let (zs, _) = self.layout.zone();
        self.frame[zs - 2..zs].copy_from_slice(&(fhp & 0x7FF).to_be_bytes());
    }

    /// Add a whole packet (primary header included). Completed frames (the
    /// transfer frame, `frame_len` bytes) go to `out`.
    pub fn push_packet(&mut self, packet: &[u8], out: &mut impl FnMut(&[u8])) {
        let (zs, _) = self.layout.zone();
        let mut rest = packet;
        let mut first = true;
        while !rest.is_empty() {
            if first && self.no_start {
                self.set_fhp(self.fill as u16);
                self.no_start = false;
            }
            first = false;
            let n = (self.zone_len() - self.fill).min(rest.len());
            let at = zs + self.fill;
            self.frame[at..at + n].copy_from_slice(&rest[..n]);
            self.fill += n;
            rest = &rest[n..];
            if self.fill == self.zone_len() {
                out(&self.frame);
                self.header.counter = (self.header.counter + 1) & 0xFF_FFFF;
                self.begin();
            }
        }
    }

    /// Fill the frame in progress (if any) with an idle packet and send it.
    pub fn flush(&mut self, out: &mut impl FnMut(&[u8])) {
        if self.fill == 0 {
            return;
        }
        let room = self.zone_len() - self.fill;
        // An idle packet has at least 7 bytes; if fewer are left it runs into
        // the next frame, which it then fills.
        let len = if room >= 7 {
            room
        } else {
            room + self.zone_len()
        };
        let mut idle = [0x55u8; 2048 + 7];
        PacketHeader {
            version: 0,
            kind: 0,
            secondary_header: false,
            apid: APID_IDLE,
            seq_flags: 3,
            seq: 0,
            data_len: len - 6,
        }
        .write(&mut idle);
        self.push_packet(&idle[..len], out);
    }

    /// Send a frame of fill (VCID 63), e.g. to keep a link busy.
    pub fn fill_frame(&mut self, out: &mut impl FnMut(&[u8])) {
        let mut f = vec![0u8; self.layout.frame_len];
        VcduHeader {
            vcid: VCID_FILL,
            ..self.header
        }
        .write(&mut f);
        out(&f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(apid: u16, seq: u16, len: usize) -> Vec<u8> {
        let mut p = vec![0u8; 6 + len];
        PacketHeader {
            version: 0,
            kind: 0,
            secondary_header: false,
            apid,
            seq_flags: 3,
            seq,
            data_len: len,
        }
        .write(&mut p);
        for (i, b) in p[6..].iter_mut().enumerate() {
            *b = (i as u16 ^ seq) as u8;
        }
        p
    }

    const LAYOUT: Layout = Layout {
        frame_len: 892,
        insert_zone: 0,
        trailer: 0,
    };

    #[test]
    fn headers_round_trip() {
        let h = VcduHeader {
            version: 1,
            spacecraft: 0xAB,
            vcid: 42,
            counter: 0x12_3456,
            replay: true,
        };
        let mut b = [0u8; 6];
        h.write(&mut b);
        assert_eq!(VcduHeader::parse(&b), Some(h));
        let p = packet(0x123, 0x2ABC, 300);
        let ph = PacketHeader::parse(&p).unwrap();
        assert_eq!((ph.apid, ph.seq, ph.len()), (0x123, 0x2ABC, 306));
    }

    #[test]
    fn packets_of_every_size_survive_framing() {
        let mut fb = FrameBuilder::new(LAYOUT, 7, 5).unwrap();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let sizes = [1usize, 5, 100, 878, 879, 880, 881, 2000, 8190, 1, 3];
        let pkts: Vec<Vec<u8>> = sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| packet(100 + i as u16, i as u16, n))
            .collect();
        for p in &pkts {
            fb.push_packet(p, &mut |f: &[u8]| frames.push(f.to_vec()));
        }
        fb.flush(&mut |f: &[u8]| frames.push(f.to_vec()));
        fb.fill_frame(&mut |f: &[u8]| frames.push(f.to_vec()));
        let mut asm = PacketAssembler::new(LAYOUT).unwrap();
        let mut got = Vec::new();
        for f in &frames {
            asm.push_frame(f, |p| {
                assert_eq!((p.spacecraft, p.vcid), (7, 5));
                got.push((p.after_gap, p.bytes.to_vec()));
            });
        }
        assert_eq!(got.len(), pkts.len());
        for ((gap, g), want) in got.iter().zip(&pkts) {
            assert!(!gap);
            assert_eq!(g, want);
        }
        assert_eq!(asm.stats().fill_frames, 1);
        assert_eq!(asm.stats().idle_packets, 1);
    }

    #[test]
    fn a_lost_frame_drops_only_the_packets_it_touched() {
        let mut fb = FrameBuilder::new(LAYOUT, 1, 2).unwrap();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let pkts: Vec<Vec<u8>> = (0..40)
            .map(|i| packet(200, i, 150 + 37 * i as usize))
            .collect();
        for p in &pkts {
            fb.push_packet(p, &mut |f: &[u8]| frames.push(f.to_vec()));
        }
        let lost = 4;
        let mut asm = PacketAssembler::new(LAYOUT).unwrap();
        let mut got = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            if i == lost {
                continue;
            }
            asm.push_frame(f, |p| {
                got.push((p.header.seq, p.after_gap, p.bytes.to_vec()))
            });
        }
        assert_eq!(asm.stats().frames_lost, 1);
        // Every packet received is intact, exactly one is flagged, and only a
        // few around the lost frame are missing.
        for (seq, _, bytes) in &got {
            assert_eq!(bytes, &pkts[*seq as usize]);
        }
        assert_eq!(got.iter().filter(|g| g.1).count(), 1);
        let missing = pkts.len() - got.len();
        assert!((1..=4).contains(&missing), "{missing}");
        let first_after = got.iter().position(|g| g.1).unwrap();
        assert!(got[first_after].0 > got[first_after - 1].0 + 1);
    }

    #[test]
    fn layout_checks() {
        assert!(
            Layout {
                frame_len: 10,
                insert_zone: 2,
                trailer: 2
            }
            .check()
            .is_err()
        );
        assert!(
            Layout {
                frame_len: 892,
                insert_zone: 2,
                trailer: 0
            }
            .check()
            .is_ok()
        );
    }
}
