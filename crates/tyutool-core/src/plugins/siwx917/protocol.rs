//! Minimal Kermit sender for the SiWx917 ROM ISP bootloader.
//!
//! The device's ROM Kermit is a minimal, non-negotiating implementation: real-hardware
//! probing (see `siwx917_kermit.sh` in the TuyaOpen tooling repo) recorded a fixed reply of
//! `MAXL=94, CAPAS=2 (no long packets / no sliding window), CHKT=1` regardless of what the
//! sender proposes. Rather than parse those fields back out of the device's Send-Init ACK
//! (the exact byte layout of optional fields like PADC has historically drifted between
//! Kermit implementations and could not be cross-checked against real hardware in this
//! session), this module hardcodes the already-confirmed values and only uses the ACK to
//! confirm the handshake succeeded. `NEGOTIATED_MAXL`/`NEGOTIATED_CHKT` are the values to
//! revisit if a future device capture shows different numbers.
//!
//! Packet framing (SOH/LEN/SEQ/TYPE/DATA/CHECK/EOL), the 6-bit Type-1 checksum, and control-
//! character quoting are the stable, standardized part of the classic Kermit protocol and are
//! unit-tested below. The stop-and-wait state machine (window size 1, 20 retries) mirrors the
//! `.ksc` script parameters already validated against the device.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::FlashError;
use crate::flash_event::FlashEvent;

/// Minimal transport bound: real use passes `Box<dyn serialport::SerialPort>` (whose
/// `SerialPort` supertraits already give `Read + Write`); tests pass an in-memory double.
/// `?Sized` so an unsized `dyn SerialPort` satisfies it — trait objects cannot be coerced
/// into each other, so callers pass `&mut *boxed_port` and the bound is resolved generically.
pub trait ReadWrite: io::Read + io::Write {}
impl<T: io::Read + io::Write + ?Sized> ReadWrite for T {}

const SOH: u8 = 0x01;
const EOL: u8 = b'\r';
const QCTL: u8 = b'#';

/// Values confirmed by capturing a real Send-Init exchange against the device (see module
/// docs) — not re-derived from the live ACK.
const NEGOTIATED_MAXL: u8 = 94;
const NEGOTIATED_CHKT: u8 = b'1';

const DEFAULT_RETRIES: u8 = 20;
const DEFAULT_PACKET_TIMEOUT: Duration = Duration::from_secs(5);

fn tochar(n: u8) -> u8 {
    n + 32
}

fn unchar(c: u8) -> u8 {
    c.wrapping_sub(32)
}

fn needs_quote(b: u8) -> bool {
    b < 0x20 || b == 0x7f || b == QCTL
}

/// Control-quote a raw byte string for the Kermit DATA field.
///
/// **Do not "correct" this to emit `QCTL QCTL` for a literal QCTL byte.** Classic Kermit (and
/// C-Kermit) escape the prefix character by doubling it, and only apply the `^ 0x40` transform
/// when the result is a real control character. The SiWx917 ROM implementation is more
/// primitive: it applies `^ 0x40` to whatever follows QCTL, unconditionally. The two rules are
/// mutually incompatible for this one byte, and the device is the one we have to satisfy, so a
/// literal `#` (0x23) goes out as `# c` (0x23 0x63) here.
///
/// Evidence: the 804,656-byte M4 image that flashed and booted successfully on real hardware
/// contains 7,179 `0x23` bytes. Under the doubling rule the device would have mis-decoded every
/// one of them, scattered through the whole image, and the firmware could not have run.
/// Feeding the same stream to C-Kermit as a receiver shows the mirror image of this: it accepts
/// every packet (framing, LEN, checksum and sequencing all agree) and reproduces the file at
/// exactly the right length, with only those QCTL bytes differing.
fn quote(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        if needs_quote(b) {
            out.push(QCTL);
            out.push(b ^ 0x40);
        } else {
            out.push(b);
        }
    }
    out
}

/// Reverse of [`quote`]. Used by tests to round-trip; the sender never needs to decode.
fn unquote(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == QCTL && i + 1 < data.len() {
            out.push(data[i + 1] ^ 0x40);
            i += 2;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Classic Kermit Type-1 checksum, over the LEN, SEQ, TYPE and DATA fields — the run from
/// just after the SOH mark up to (not including) the check byte. **LEN is part of the sum**;
/// omitting it produces packets a real receiver NAKs forever, and a sender/receiver pair that
/// both omit it still agree with each other, so only a wire-format vector catches the mistake
/// (see `ack_packet_matches_hand_derived_wire_bytes`).
///
/// Folding is the standard `(s + ((s & 0xC0) >> 6)) & 0x3F`; bits 6-7 live in the low byte, so
/// summing with `u8` wraparound gives the same answer as a wider accumulator.
fn checksum1(bytes: &[u8]) -> u8 {
    let sum8: u8 = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    let folded = (sum8 as u32 + ((sum8 as u32) >> 6)) & 0x3f;
    tochar(folded as u8)
}

/// Build a framed packet: `SOH LEN SEQ TYPE DATA CHECK EOL`. `data` must already be quoted.
fn build_packet(seq: u8, ptype: u8, data: &[u8]) -> Vec<u8> {
    // LEN counts everything after itself up to and including CHECK: SEQ + TYPE + DATA + CHECK.
    let len = 3 + data.len();

    let mut checked = Vec::with_capacity(3 + data.len());
    checked.push(tochar(len as u8));
    checked.push(tochar(seq));
    checked.push(ptype);
    checked.extend_from_slice(data);
    let chk = checksum1(&checked);

    let mut pkt = Vec::with_capacity(checked.len() + 3);
    pkt.push(SOH);
    pkt.extend_from_slice(&checked);
    pkt.push(chk);
    pkt.push(EOL);
    pkt
}

fn read_byte<T: ReadWrite + ?Sized>(io: &mut T, deadline: Instant) -> Result<u8, FlashError> {
    let mut buf = [0u8; 1];
    loop {
        if Instant::now() > deadline {
            return Err(FlashError::Plugin("SIWX917: Kermit read timeout".into()));
        }
        match io.read(&mut buf) {
            Ok(1) => return Ok(buf[0]),
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(FlashError::Io(e)),
        }
    }
}

/// Read one framed packet, resyncing on `SOH` and dropping anything before it.
fn read_packet<T: ReadWrite + ?Sized>(
    io: &mut T,
    deadline: Instant,
) -> Result<(u8, u8, Vec<u8>), FlashError> {
    loop {
        if read_byte(io, deadline)? == SOH {
            break;
        }
    }
    let len_char = read_byte(io, deadline)?;
    let len = unchar(len_char) as usize;
    if len < 3 {
        return Err(FlashError::Plugin(
            "SIWX917: Kermit packet shorter than SEQ+TYPE+CHECK".into(),
        ));
    }
    let mut body = Vec::with_capacity(len);
    for _ in 0..len {
        body.push(read_byte(io, deadline)?);
    }
    let (payload, chk) = body.split_at(body.len() - 1);
    // The check covers LEN as well as SEQ/TYPE/DATA — see [`checksum1`].
    let mut checked = Vec::with_capacity(1 + payload.len());
    checked.push(len_char);
    checked.extend_from_slice(payload);
    if checksum1(&checked) != chk[0] {
        return Err(FlashError::Plugin(
            "SIWX917: Kermit checksum mismatch".into(),
        ));
    }
    let seq = unchar(payload[0]);
    let ptype = payload[1];
    Ok((seq, ptype, payload[2..].to_vec()))
}

/// Send a packet, retrying on timeout/NAK until it is ACKed (type `Y`) for the same `seq`.
/// Returns the (unquoted) ACK payload.
fn send_and_wait_ack<T: ReadWrite + ?Sized>(
    io: &mut T,
    cancel: &AtomicBool,
    seq: u8,
    ptype: u8,
    data: &[u8],
) -> Result<Vec<u8>, FlashError> {
    let pkt = build_packet(seq, ptype, data);
    for attempt in 0..DEFAULT_RETRIES {
        if cancel.load(Ordering::Relaxed) {
            return Err(FlashError::Cancelled);
        }
        io.write_all(&pkt)?;
        let deadline = Instant::now() + DEFAULT_PACKET_TIMEOUT;
        // Why this attempt failed. Worth logging: a run of instant NAKs means the device
        // rejects the packet itself (bad framing/checksum) and resending cannot help, while
        // repeated timeouts mean it is not listening at all — two very different faults that
        // look identical without this.
        let reason;
        loop {
            match read_packet(io, deadline) {
                Ok((rseq, rtype, rdata)) if rseq == seq && rtype == b'Y' => {
                    return Ok(unquote(&rdata));
                }
                Ok((rseq, rtype, _)) if rseq == seq && rtype == b'N' => {
                    reason = "device NAKed the packet".to_string();
                    break;
                }
                // Stale/mismatched packet — keep listening until the deadline.
                Ok((rseq, rtype, _)) => {
                    log::debug!(
                        "SIWX917: ignoring packet seq={rseq} type='{}' (waiting for seq={seq} 'Y')",
                        rtype as char
                    );
                    continue;
                }
                Err(e) => {
                    reason = e.to_string();
                    break;
                }
            }
        }
        log::warn!(
            "SIWX917: Kermit retry {attempt}/{DEFAULT_RETRIES} for seq={seq} type='{}': {reason}",
            ptype as char
        );
    }
    Err(FlashError::Plugin(format!(
        "SIWX917: no ACK for Kermit packet type '{}' after {DEFAULT_RETRIES} retries",
        ptype as char
    )))
}

/// Our declared Send-Init fields. The device's ROM Kermit is minimal/non-negotiating (see
/// module docs), so exact values mostly need to be well-formed, not authoritative.
fn send_init_fields() -> Vec<u8> {
    vec![
        tochar(NEGOTIATED_MAXL), // MAXL we can receive (irrelevant: we never receive data)
        tochar(10),              // TIME: 10s
        tochar(0),               // NPAD: no padding
        0x40,                    // PADC: NUL ctl-quoted; irrelevant since NPAD=0
        tochar(13),              // EOL: CR
        QCTL,                    // QCTL literal
        b'N',                    // QBIN: no 8th-bit prefixing needed
        NEGOTIATED_CHKT,         // CHKT
        b' ',                    // REPT: no run-length encoding
        tochar(0),               // CAPAS: no extended capabilities requested
    ]
}

/// Split off a chunk of `data` whose quoted encoding fits in `max_encoded` bytes.
/// Returns (raw bytes consumed, quoted chunk). Always consumes at least 1 byte.
fn take_chunk(data: &[u8], max_encoded: usize) -> (usize, Vec<u8>) {
    let mut encoded = Vec::with_capacity(max_encoded);
    let mut raw = 0usize;
    for &b in data {
        let add = if needs_quote(b) { 2 } else { 1 };
        if raw > 0 && encoded.len() + add > max_encoded {
            break;
        }
        if needs_quote(b) {
            encoded.push(QCTL);
            encoded.push(b ^ 0x40);
        } else {
            encoded.push(b);
        }
        raw += 1;
    }
    (raw, encoded)
}

/// Send a whole file over an already-menu-selected Kermit receiver: Send-Init, File-Header,
/// Data packets, End-of-File, Break. Stop-and-wait (window size 1), matching the device's
/// fixed `CAPAS=2` (no sliding window / no long packets).
pub fn send_file<T: ReadWrite + ?Sized>(
    io: &mut T,
    cancel: &AtomicBool,
    progress: &dyn Fn(FlashEvent),
    file_name: &str,
    data: &[u8],
) -> Result<(), FlashError> {
    let mut seq: u8 = 0;

    let ack = send_and_wait_ack(io, cancel, seq, b'S', &send_init_fields())?;
    log::debug!("SIWX917: Send-Init ACK raw={ack:?}");

    seq = (seq + 1) % 64;
    send_and_wait_ack(io, cancel, seq, b'F', &quote(file_name.as_bytes()))?;

    // MAXL bounds (SEQ+TYPE+DATA+CHECK); SEQ+TYPE+CHECK cost 3 bytes of that budget.
    let max_encoded = (NEGOTIATED_MAXL as usize).saturating_sub(3).max(2);
    let total = data.len().max(1);
    let mut idx = 0usize;
    while idx < data.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err(FlashError::Cancelled);
        }
        let (raw, encoded) = take_chunk(&data[idx..], max_encoded);
        seq = (seq + 1) % 64;
        send_and_wait_ack(io, cancel, seq, b'D', &encoded)?;
        idx += raw;
        progress(FlashEvent::Percent {
            value: (idx as u64 * 100 / total as u64) as u8,
        });
    }

    seq = (seq + 1) % 64;
    send_and_wait_ack(io, cancel, seq, b'Z', &[])?;

    seq = (seq + 1) % 64;
    send_and_wait_ack(io, cancel, seq, b'B', &[])?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn tochar_unchar_roundtrip_full_range() {
        for n in 0..=94u8 {
            assert_eq!(unchar(tochar(n)), n);
        }
    }

    #[test]
    fn quote_escapes_controls_del_and_qctl_itself() {
        let raw = [0x00, b'A', 0x1f, 0x7f, QCTL, 0x80];
        let q = quote(&raw);
        assert_eq!(unquote(&q), raw);
        // A plain printable byte is never expanded.
        assert_eq!(quote(b"A"), b"A");
        // QCTL itself must be escaped (else the receiver can't tell data from a prefix).
        assert_eq!(quote(&[QCTL]), vec![QCTL, QCTL ^ 0x40]);
    }

    #[test]
    fn checksum1_matches_known_vector() {
        // "abc" (0x61+0x62+0x63=0x126 -> 8-bit wrap 0x26; fold: 0x26+(0x26>>6)=0x26 -> tochar).
        assert_eq!(checksum1(b"abc"), tochar(0x26));
    }

    /// Pins the exact bytes on the wire, derived by hand from the Kermit framing rules.
    ///
    /// An ACK for packet 0 with no data:
    ///   LEN  = SEQ + TYPE + CHECK = 3      -> tochar(3)  = 35 = '#'
    ///   SEQ  = 0                           -> tochar(0)  = 32 = ' '
    ///   TYPE = 'Y'                         =              89
    ///   sum over LEN+SEQ+TYPE = 35+32+89   = 156
    ///   fold: (156 + ((156 & 0xC0) >> 6)) & 0x3F = (156 + 2) & 63 = 30
    ///   CHECK = tochar(30) = 62 = '>'
    ///
    /// This is the test that would have caught LEN being left out of the checksum: a
    /// sender and receiver that both omit it still agree, so `build_and_read_packet_roundtrip`
    /// passes either way. Only a fixed external vector pins the format down.
    #[test]
    fn ack_packet_matches_hand_derived_wire_bytes() {
        assert_eq!(
            build_packet(0, b'Y', &[]),
            vec![SOH, b'#', b' ', b'Y', b'>', EOL]
        );
    }

    #[test]
    fn checksum_covers_the_len_field() {
        // Same SEQ/TYPE/DATA at two different lengths must not collide on the check byte;
        // they would if LEN were excluded from the sum.
        let short = build_packet(0, b'D', b"A");
        let long = build_packet(0, b'D', b"AB");
        let chk_short = short[short.len() - 2];
        let chk_long = long[long.len() - 2];
        // 'B' (66) alone would shift the sum by 66; LEN also grows by 1, so the two effects
        // must both be present. Recompute independently to be sure.
        assert_eq!(chk_short, checksum1(&[tochar(4), tochar(0), b'D', b'A']));
        assert_eq!(
            chk_long,
            checksum1(&[tochar(5), tochar(0), b'D', b'A', b'B'])
        );
    }

    #[test]
    fn build_and_read_packet_roundtrip() {
        let pkt = build_packet(5, b'D', &quote(b"hello\x01"));
        struct Loop(VecDeque<u8>);
        impl io::Read for Loop {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "empty"));
                }
                let mut n = 0;
                while n < buf.len() {
                    match self.0.pop_front() {
                        Some(b) => {
                            buf[n] = b;
                            n += 1;
                        }
                        None => break,
                    }
                }
                Ok(n)
            }
        }
        impl io::Write for Loop {
            fn write(&mut self, d: &[u8]) -> io::Result<usize> {
                Ok(d.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut lp = Loop(pkt.into_iter().collect());
        let (seq, ptype, data) =
            read_packet(&mut lp, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(seq, 5);
        assert_eq!(ptype, b'D');
        assert_eq!(unquote(&data), b"hello\x01");
    }

    #[test]
    fn read_packet_rejects_bad_checksum() {
        let mut pkt = build_packet(0, b'Y', &[]);
        // Corrupt the checksum byte (second-to-last, before EOL).
        let n = pkt.len();
        pkt[n - 2] ^= 0xff;
        struct Once(VecDeque<u8>);
        impl io::Read for Once {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "empty"));
                }
                buf[0] = self.0.pop_front().unwrap();
                Ok(1)
            }
        }
        impl io::Write for Once {
            fn write(&mut self, d: &[u8]) -> io::Result<usize> {
                Ok(d.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut once = Once(pkt.into_iter().collect());
        let res = read_packet(&mut once, Instant::now() + Duration::from_secs(1));
        assert!(matches!(res, Err(FlashError::Plugin(msg)) if msg.contains("checksum")));
    }

    #[test]
    fn take_chunk_always_makes_progress_even_with_tiny_budget() {
        let (raw, encoded) = take_chunk(&[0x00, b'A', b'B'], 2);
        assert_eq!(raw, 1); // 0x00 needs quoting -> 2 encoded bytes, exactly fills budget
        assert_eq!(encoded, vec![QCTL, 0x40]); // 0x00 ctl-quoted is 0x00 ^ 0x40
    }

    #[test]
    fn take_chunk_packs_multiple_plain_bytes_per_packet() {
        let data = vec![b'A'; 200];
        let (raw, encoded) = take_chunk(&data, 91);
        assert_eq!(raw, 91);
        assert_eq!(encoded.len(), 91);
    }

    struct MockPort {
        inbound: VecDeque<u8>,
        outbound: Vec<u8>,
    }
    impl io::Read for MockPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.inbound.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no data"));
            }
            let mut n = 0;
            while n < buf.len() {
                match self.inbound.pop_front() {
                    Some(b) => {
                        buf[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            Ok(n)
        }
    }
    impl io::Write for MockPort {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.outbound.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn send_and_wait_ack_accepts_matching_ack_and_writes_framed_packet() {
        let ack_pkt = build_packet(0, b'Y', &quote(b"ack-data"));
        let mut port = MockPort {
            inbound: ack_pkt.into_iter().collect(),
            outbound: Vec::new(),
        };
        let cancel = AtomicBool::new(false);
        let data = send_and_wait_ack(&mut port, &cancel, 0, b'S', &send_init_fields()).unwrap();
        assert_eq!(data, b"ack-data");
        assert_eq!(port.outbound[0], SOH);
        assert_eq!(port.outbound[3], b'S'); // SOH, LEN, SEQ, TYPE
    }

    #[test]
    fn send_and_wait_ack_retransmits_on_nak_then_succeeds() {
        let nak = build_packet(0, b'N', &[]);
        let ack = build_packet(0, b'Y', &[]);
        let mut inbound = VecDeque::new();
        inbound.extend(nak);
        inbound.extend(ack);
        let mut port = MockPort {
            inbound,
            outbound: Vec::new(),
        };
        let cancel = AtomicBool::new(false);
        let res = send_and_wait_ack(&mut port, &cancel, 0, b'F', &[]);
        assert!(res.is_ok());
        // Packet was written twice: once before the NAK, once after.
        let pkt = build_packet(0, b'F', &[]);
        assert_eq!(port.outbound.len(), pkt.len() * 2);
    }

    #[test]
    fn send_and_wait_ack_respects_cancellation() {
        let mut port = MockPort {
            inbound: VecDeque::new(),
            outbound: Vec::new(),
        };
        let cancel = AtomicBool::new(true);
        let res = send_and_wait_ack(&mut port, &cancel, 0, b'S', &[]);
        assert!(matches!(res, Err(FlashError::Cancelled)));
    }

    #[test]
    fn send_file_drives_full_state_machine_over_mock_transport() {
        // Program replies for S, F, D(one chunk fits in one packet for a short payload), Z, B.
        let mut inbound = VecDeque::new();
        for seq in 0..5u8 {
            inbound.extend(build_packet(seq, b'Y', &[]));
        }
        let mut port = MockPort {
            inbound,
            outbound: Vec::new(),
        };
        let cancel = AtomicBool::new(false);
        let events = std::cell::RefCell::new(Vec::new());
        let progress = |e: FlashEvent| events.borrow_mut().push(e);
        let res = send_file(&mut port, &cancel, &progress, "fw.rps", b"hello world");
        assert!(res.is_ok(), "{res:?}");
        assert!(!events.borrow().is_empty());
    }
}
