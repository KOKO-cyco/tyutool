//! SiWx917 flash plugin — serial ISP via the chip's ROM Kermit bootloader.
//!
//! Driven against real hardware (`siwx917_kermit.sh`, modes `menu` and `menu-ta`): the device
//! exposes a text bootloader menu on a dedicated ISP UART (GPIO_8/RX, GPIO_9/TX @ 115200, per
//! Silicon Labs AN1431) once GPIO_34 (BOOT_MODE) is held low across reset. `Ctrl+|` (0x1C)
//! wakes it, `U` prints the menu. Captured verbatim from "BootLoader Version 1.1":
//!
//! ```text
//! 1 Load Default Wireless Firmware        2 Load Default M4 Firmware
//! A Load Wireless Firmware (Image No : 0-f)   3 Load M4 Firmware (Image No : 1-f)
//! B Burn Wireless Firmware (Image No : 0-f)   4 Burn M4 Firmware (Image No : 1-f)
//! 5 Select Default Wireless Firmware      6 Select Default M4 Firmware
//! K Check Wireless Firmware Integrity     9 Check M4 Firmware Integrity
//! F Select M4 and Wireless Images Pair    7/8 Enable/Disable GPIO Based Bypass Mode
//! Q Update KEY   Z JTAG Selection   o Send OPN   b Change UART Baud Rate
//! ```
//!
//! SiWx917 is dual-core, so there are two firmware images and two burn paths. Both were
//! confirmed to prompt for a slot number afterwards ("Enter Wireless Image No(0-f)"):
//!   * M4 application  → `4`, slot `1` (valid 1-f)
//!   * NWP/TA wireless → `B`, slot `0` (valid 0-f)
//!
//! Which one a file is depends on its RPS header, not on user input — see [`ImageKind::detect`].
//!
//! Only [`crate::job::FlashMode::Flash`] is implemented. The captured menu above has **no**
//! erase or read-back entry at all, so `Erase`/`Read` are rejected rather than faked.
//! `Authorize` never reaches a chip plugin (see `registry::run_job`) but the arm is required.
//!
//! There is no confirmed way to drive the board into ISP mode over DTR/RTS, so — unlike ESP32/
//! Beken — this plugin does not attempt an automatic reset. It prompts the user (mirroring the
//! LN882H "hold BOOT/A9 pin LOW" pattern) and polls for the bootloader to respond.
//!
//! ## The menu is not re-enterable without a reset
//!
//! A freshly reset board answers `0x1C` with `"\r\nEnter 'U'"` and then prints the menu for `U`.
//! Once a command has been run, that same pair means something else entirely: replaying it
//! against a board already sitting at the menu was observed to produce `"Configuration Saved..."`
//! instead. So each run re-enters ISP from a reset rather than assuming a usable menu is still
//! there, and nothing is sent speculatively at a prompt whose state is unknown.
//!
//! ## Both burn paths are hardware-verified
//!
//! * M4 — flashed, booted, and ran its application.
//! * NWP/TA wireless — flashed, then checked with the ROM's own `K Check Wireless Firmware
//!   Integrity` entry (`K` then slot `0`), which answered `"Integrity Passed"`.
//!
//! That integrity entry is a stronger post-burn check than reading the text report in
//! [`confirm_upgrade`], and the ROM does invite one (`"Enter Next Command"`). Wiring it in is
//! left alone deliberately: driving the menu again straight after a burn has not been tried on
//! hardware, and per the note above, guessing at menu state is exactly what misfires.

mod protocol;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use protocol::ReadWrite;
use serialport::SerialPort;

use crate::error::FlashError;
use crate::flash_event::{FlashEvent, FlashMilestone, FlashPhase};
use crate::job::{FlashJob, FlashMode};
use crate::plugin::FlashPlugin;

/// Dedicated ISP UART per AN1431, fixed — `job.baud_rate` is deliberately ignored.
///
/// Silicon Labs' Matter documentation also mentions a 921600-baud JLink-CDC path, but on the
/// boards this was reverse-engineered against that channel is the console/VCOM, not the ISP
/// UART. Raising the rate on the ISP UART itself is possible in principle via the bootloader
/// menu's `b Change UART Baud Rate` entry — that is what Simplicity Commander drives when it
/// tries to negotiate 921600 — but that negotiation fails outright on CH340-class adapters
/// (Commander needs `--fixedspeed` to get past it, and a failed negotiation aborts the whole
/// load). Staying at 115200 trades throughput for a transfer that always completes; the
/// bottleneck is the stop-and-wait turnaround anyway, not the line rate.
const ISP_BAUD: u32 = 115_200;
const ISP_ENTER_ATTEMPTS: u32 = 20;
const ISP_ENTER_RETRY_GAP: Duration = Duration::from_secs(2);

/// RPS container header size — `SLI_RPS_HEADER_SIZE` in the WiseConnect SDK. Cross-checked
/// against 14 sample images: every one is exactly `header.image_size + 64` bytes long.
const RPS_HEADER_SIZE: usize = 64;
/// `magic_no` field of `sl_wifi_firmware_header_t`; identical in all 14 samples.
const RPS_MAGIC: u32 = 0x900d_900d;

/// Which core an RPS image targets. SiWx917 is dual-core and the two images go through
/// different bootloader menu entries, so picking the wrong one silently flashes the wrong core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageKind {
    /// M4 application — the per-app build output (`<project>.rps` / `_isp.bin` / `_QIO_*.bin`).
    M4,
    /// NWP / TA wireless firmware — a prebuilt blob (e.g. `mcu/patch/RS9117_WC_SI.rps`),
    /// normally burned once per board.
    Nwp,
}

impl ImageKind {
    /// Bootloader menu key that starts a burn for this image kind.
    /// Matches `BURN_M4_FW` / `BURN_NWP_FW` in the SDK's `sl_si91x_constants.h`.
    fn menu_key(self) -> u8 {
        match self {
            ImageKind::M4 => b'4',
            ImageKind::Nwp => b'B',
        }
    }

    /// Slot answered to the "Image No" prompt that follows [`Self::menu_key`]
    /// (M4 slots are advertised as 1-f, wireless as 0-f).
    ///
    /// These are not guesses: Simplicity Commander was recorded driving a stand-in bootloader
    /// on a PTY, and it sends exactly `4` + `1` for an M4 image and `B` + `0` for a wireless
    /// one. The M4 pair is additionally confirmed by a successful flash on real hardware.
    ///
    /// Note that the slot number does **not** select between the SDK's SlotA and SlotB wireless
    /// images: Commander sends `0` for both (checked against
    /// `connectivity_firmware/standard/` and `connectivity_firmware/siwx91x_slot_b_images/
    /// standard/`, whose headers carry `flash_location` 0x00011000 and 0x005e0000). Placement
    /// comes from the image header, so one slot value covers every wireless image and there is
    /// nothing here to branch on.
    fn image_slot(self) -> u8 {
        match self {
            ImageKind::M4 => b'1',
            ImageKind::Nwp => b'0',
        }
    }

    fn label(self) -> &'static str {
        match self {
            ImageKind::M4 => "M4 application",
            ImageKind::Nwp => "NWP/TA wireless firmware",
        }
    }

    /// Classify an RPS image from its 64-byte header, also validating that it *is* an RPS
    /// container so a stray `.bin`/`.hex` cannot be pushed at the bootloader.
    ///
    /// Discriminator is bit 0 of `control_flags`. Verified across 14 images of known type
    /// (3 wireless from `connectivity_firmware/` + `mcu/patch/`, 11 M4 from `demos/`,
    /// `embedded-otbr/` and a local app build): wireless is always 0, M4 always 1.
    ///
    /// Deliberately *not* keyed on `flash_location` (0x00011000 wireless vs 0x00201000 M4 in
    /// those same samples): that address moves with the flash partitioning, which is itself a
    /// build option here (`CONFIG_CORE_M4_FLASH_SIZE` offers 2040 KB / 3008 KB), and for
    /// wireless images it additionally distinguishes the SDK's SlotA/SlotB variants — a
    /// distinction the burn path does not need (see [`Self::image_slot`]). It is logged for
    /// diagnostics instead.
    pub(crate) fn detect(data: &[u8]) -> Result<Self, FlashError> {
        if data.len() < RPS_HEADER_SIZE {
            return Err(FlashError::InvalidJob(format!(
                "SIWX917: file is {} bytes — too short to be an RPS image (need at least {RPS_HEADER_SIZE})",
                data.len()
            )));
        }
        let control_flags = u16::from_le_bytes([data[0], data[1]]);
        let magic = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let image_size = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let flash_location = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);

        if magic != RPS_MAGIC {
            return Err(FlashError::InvalidJob(format!(
                "SIWX917: not an RPS image (magic 0x{magic:08x}, expected 0x{RPS_MAGIC:08x}) — \
                 the ROM bootloader only accepts .rps files, not .bin/.hex/.s37"
            )));
        }
        let expected = image_size as usize + RPS_HEADER_SIZE;
        if data.len() != expected {
            return Err(FlashError::InvalidJob(format!(
                "SIWX917: RPS image is truncated or padded — header says {image_size} bytes \
                 (+{RPS_HEADER_SIZE} header = {expected}), file is {}",
                data.len()
            )));
        }

        let kind = if control_flags & 0x0001 != 0 {
            ImageKind::M4
        } else {
            ImageKind::Nwp
        };
        log::info!(
            "SIWX917: RPS control_flags=0x{control_flags:04x} flash_location=0x{flash_location:08x} \
             image_size={image_size} -> {}",
            kind.label()
        );
        Ok(kind)
    }
}

pub struct Siwx917Plugin;

impl FlashPlugin for Siwx917Plugin {
    fn id(&self) -> &'static str {
        "SIWX917"
    }

    fn run(
        &self,
        job: &FlashJob,
        cancel: &AtomicBool,
        progress: &dyn Fn(FlashEvent),
    ) -> Result<(), FlashError> {
        match job.mode {
            FlashMode::Flash => run_flash(job, cancel, progress),
            FlashMode::Erase => Err(FlashError::Plugin(
                "SIWX917: erase is not exposed by the ROM ISP menu — not supported".into(),
            )),
            FlashMode::Read => Err(FlashError::Plugin(
                "SIWX917: flash read-back is not exposed by the ROM ISP menu — not supported"
                    .into(),
            )),
            FlashMode::Authorize => Err(FlashError::Plugin(
                "SIWX917: authorize mode not supported by this plugin".into(),
            )),
        }
    }
}

fn open_port(port_name: &str) -> Result<Box<dyn SerialPort>, FlashError> {
    serialport::new(port_name, ISP_BAUD)
        .timeout(Duration::from_millis(300))
        .open()
        .map_err(FlashError::Serial)
}

fn run_flash(
    job: &FlashJob,
    cancel: &AtomicBool,
    progress: &dyn Fn(FlashEvent),
) -> Result<(), FlashError> {
    let firmware_path = resolve_single_image(job)?;

    let data = std::fs::read(&firmware_path)
        .map_err(|e| FlashError::Plugin(format!("cannot read firmware '{firmware_path}': {e}")))?;
    if data.is_empty() {
        return Err(FlashError::InvalidJob(format!(
            "firmware file '{firmware_path}' is empty"
        )));
    }
    // Classify before touching the port: a wrong-core burn is worse than a failed one.
    let kind = ImageKind::detect(&data)?;
    let file_name = std::path::Path::new(&firmware_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("firmware.rps");

    let mut port = open_port(&job.port)?;

    progress(FlashEvent::Phase {
        phase: FlashPhase::Handshake,
    });
    enter_isp_menu(&mut *port, cancel, progress, kind)?;
    progress(FlashEvent::Milestone {
        milestone: FlashMilestone::HandshakeComplete,
    });

    progress(FlashEvent::Phase {
        phase: FlashPhase::Write,
    });
    protocol::send_file(&mut *port, cancel, progress, file_name, &data)?;
    progress(FlashEvent::Milestone {
        milestone: FlashMilestone::WriteComplete,
    });

    progress(FlashEvent::Phase {
        phase: FlashPhase::Verify,
    });
    confirm_upgrade(&mut *port, progress)?;

    Ok(())
}

/// Read the ROM's own verdict on the burn, which arrives as plain text after the final Kermit
/// Break packet is acknowledged.
///
/// Captured from a real wireless burn driven by Simplicity Commander (strace on its serial fd):
///
/// ```text
/// Safe Upgrade in Progress ...
/// Upgradation Successful
///
/// Enter Next Command
/// ```
///
/// Commander issues **no** further menu command after the Break — its "Verifying file upload..."
/// step is precisely this read. That also settles a question the menu invites: there is no
/// "select default image" step to perform afterwards.
///
/// Only the wireless wording has been captured, so an unrecognised report is surfaced as a
/// warning rather than failing a flash that may well have succeeded. An explicit failure word
/// is treated as an error, because reporting success there would be worse than a false alarm.
fn confirm_upgrade<T: ReadWrite + ?Sized>(
    port: &mut T,
    progress: &dyn Fn(FlashEvent),
) -> Result<(), FlashError> {
    // Each Kermit packet ends in a CR that `read_packet` does not consume, so the Break ACK
    // leaves one behind. Without dropping it first, that single stray byte counts as "the
    // device is talking" and starts the idle countdown before the ROM has even begun writing.
    drain(port, Duration::from_millis(100));

    // Generous windows on purpose. The ROM prints "Safe Upgrade in Progress ...", then does
    // real flash work, and only then prints its verdict — so the gap between those two lines
    // is long, and a short idle window would give up inside it. The success text is the real
    // exit condition; these bounds only decide how long to wait for it.
    let report = read_until(
        port,
        Duration::from_secs(60),
        Duration::from_secs(5),
        |buf| classify_report(buf).is_some(),
    );
    let text = String::from_utf8_lossy(&report);
    log::info!("SIWX917: post-burn report: {text:?}");

    match classify_report(&report) {
        Some(true) => {
            progress(FlashEvent::Milestone {
                milestone: FlashMilestone::VerifyPassed,
            });
            return Ok(());
        }
        Some(false) => {
            return Err(FlashError::Plugin(format!(
                "SIWX917: device reported a failed upgrade: {}",
                text.trim()
            )));
        }
        None => {}
    }
    progress(FlashEvent::Warning {
        message: format!(
            "Image sent and acknowledged, but the device's completion report was not \
             recognised — verify the board boots. Report: {:?}",
            text.trim()
        ),
    });
    Ok(())
}

/// `Some(true)` = the ROM said the upgrade worked, `Some(false)` = it said it failed,
/// `None` = nothing conclusive yet. Doubling as the read loop's exit condition means a stated
/// failure ends the wait immediately instead of sitting out the idle window.
fn classify_report(buf: &[u8]) -> Option<bool> {
    if contains_ignore_case(buf, "Successful") {
        return Some(true);
    }
    if ["fail", "error", "corrupt", "invalid"]
        .iter()
        .any(|w| contains_ignore_case(buf, w))
    {
        return Some(false);
    }
    None
}

fn contains_ignore_case(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Pick the one image to burn, honouring the shared `segments` list that the GUI always sends
/// (`buildFlashJob` in `stores/flash.ts`) and that every other plugin reads first.
///
/// One image per run, by design: whether the ROM bootloader returns to its menu after a
/// completed Kermit transfer has never been observed — `siwx917_kermit.sh` always closes the
/// port afterwards. Chaining a second burn on that assumption could push a whole image into a
/// bootloader that is not listening, so extra segments are rejected loudly instead of silently
/// dropped. Burning both cores means running twice (the NWP image is a once-per-board job).
///
/// The per-segment addresses are ignored: this bootloader takes a menu slot, not an address.
fn resolve_single_image(job: &FlashJob) -> Result<String, FlashError> {
    let from_segments: Vec<String> = job
        .segments
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|s| s.firmware_path.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();

    if from_segments.len() > 1 {
        return Err(FlashError::InvalidJob(format!(
            "SIWX917: got {} firmware images, but this bootloader burns one image per run — \
             flash the M4 application and the NWP/TA wireless firmware in separate runs",
            from_segments.len()
        )));
    }
    if let Some(path) = from_segments.into_iter().next() {
        return Ok(path);
    }
    job.firmware_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .ok_or_else(|| FlashError::InvalidJob("missing firmware_path".into()))
}

/// Collect bytes until any of: `accept` is satisfied, the stream goes quiet for `idle` after
/// having produced something, or `overall` elapses.
///
/// The two early exits matter for how long a user stands there holding a reset button. The
/// bootloader emits its menu as one burst and then says nothing, so a plain "read for N
/// seconds" pays the full N every single attempt — with a 20-attempt poll that added up to
/// minutes of pure waiting.
fn read_until<T: ReadWrite + ?Sized>(
    port: &mut T,
    overall: Duration,
    idle: Duration,
    accept: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = Instant::now() + overall;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];
    let mut last_data: Option<Instant> = None;

    while Instant::now() < deadline {
        match port.read(&mut tmp) {
            Ok(n) if n > 0 => {
                buf.extend_from_slice(&tmp[..n]);
                last_data = Some(Instant::now());
                if accept(&buf) {
                    return buf;
                }
            }
            // No data: either a read timeout (the normal case on a serial port) or a
            // zero-length read. Sleep briefly so a port that returns instantly cannot spin.
            _ => {
                if last_data.is_some_and(|t| t.elapsed() >= idle) {
                    return buf;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    buf
}

/// Read and discard whatever is already buffered, with no early accept.
fn drain<T: ReadWrite + ?Sized>(port: &mut T, dur: Duration) {
    let _ = read_until(port, dur, dur, |_| false);
}

/// Recognise the bootloader menu in a reply to `U`. Anchored on the banner and on the two burn
/// entries, all captured verbatim from a real board (see module docs). Matching real content —
/// rather than "any non-empty reply" — keeps application log output on a mis-wired console port
/// from being mistaken for a bootloader that is ready to accept a firmware image.
fn looks_like_isp_menu(buf: &[u8]) -> bool {
    contains_ignore_case(buf, "BootLoader")
        && contains_ignore_case(buf, "Burn M4 Firmware")
        && contains_ignore_case(buf, "Burn Wireless Firmware")
}

/// Handshake + menu navigation: `Ctrl+|` wake, `U` for the menu, then the burn entry for this
/// image kind followed by its slot number. Both burn entries prompt for a slot — confirmed on
/// hardware for `B` ("Enter Wireless Image No(0-f)") and for `4` by the pre-existing flow.
///
/// Polls because there is no confirmed automatic way to assert GPIO_34/BOOT_MODE low — the user
/// must do it physically, on their own schedule.
fn enter_isp_menu<T: ReadWrite + ?Sized>(
    port: &mut T,
    cancel: &AtomicBool,
    progress: &dyn Fn(FlashEvent),
    kind: ImageKind,
) -> Result<(), FlashError> {
    for attempt in 0..ISP_ENTER_ATTEMPTS {
        if cancel.load(Ordering::Relaxed) {
            return Err(FlashError::Cancelled);
        }
        if attempt == 0 {
            progress(FlashEvent::Warning {
                message: format!(
                    "About to burn the {} — put the board into ISP mode now: hold the ISP \
                     button, tap Reset, then release ISP (no ISP button: pull GPIO_34/BOOT_MODE \
                     low during reset instead).",
                    kind.label()
                ),
            });
        }

        drain(port, Duration::from_millis(50)); // stale bytes from a previous attempt

        if port.write_all(&[0x1c]).is_ok() {
            // Wake ack, if any. Settles as soon as the device stops talking.
            let _ = read_until(
                port,
                Duration::from_secs(2),
                Duration::from_millis(200),
                |_| false,
            );
            if port.write_all(b"U").is_ok() {
                // Return the moment the menu is recognisable rather than waiting out the
                // window — this is the step that used to cost a fixed 4 s per attempt.
                let menu = read_until(
                    port,
                    Duration::from_secs(4),
                    Duration::from_millis(400),
                    looks_like_isp_menu,
                );
                if looks_like_isp_menu(&menu) {
                    log::info!(
                        "SIWX917: bootloader menu up; selecting '{}' + slot '{}' for {}",
                        kind.menu_key() as char,
                        kind.image_slot() as char,
                        kind.label()
                    );
                    port.write_all(&[kind.menu_key()]).map_err(FlashError::Io)?;
                    // "Enter <M4|Wireless> Image No(...)" — settle on the prompt.
                    let prompt = read_until(
                        port,
                        Duration::from_secs(2),
                        Duration::from_millis(300),
                        |_| false,
                    );
                    log::debug!(
                        "SIWX917: slot prompt: {:?}",
                        String::from_utf8_lossy(&prompt)
                    );
                    port.write_all(&[kind.image_slot()])
                        .map_err(FlashError::Io)?;
                    // The device answers with an init status before it starts listening for a
                    // Kermit stream; we do not parse it, but it must be consumed and it must
                    // have arrived before the first Send-Init goes out.
                    let ack = read_until(
                        port,
                        Duration::from_secs(2),
                        Duration::from_millis(300),
                        |_| false,
                    );
                    log::debug!(
                        "SIWX917: post-slot status: {:?}",
                        String::from_utf8_lossy(&ack)
                    );
                    return Ok(());
                }
                log::debug!(
                    "SIWX917: reply to 'U' was not the bootloader menu: {:?}",
                    String::from_utf8_lossy(&menu)
                );
            }
        }

        std::thread::sleep(ISP_ENTER_RETRY_GAP);
    }

    Err(FlashError::Plugin(
        "SIWX917: device never entered ISP mode — hold ISP/BOOT_MODE low during reset and retry \
         (note: on BRD2605A the JLink VCOM port is the console, not the ISP UART)"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(mode: FlashMode) -> FlashJob {
        FlashJob {
            mode,
            chip_id: "SIWX917".to_string(),
            port: String::new(),
            baud_rate: 115200,
            segments: None,
            flash_start_hex: None,
            flash_end_hex: None,
            erase_start_hex: None,
            erase_end_hex: None,
            read_start_hex: None,
            read_end_hex: None,
            read_file_path: None,
            firmware_path: None,
            authorize_uuid: None,
            authorize_key: None,
            authorize_storage: None,
            confirm_overwrite: None,
        }
    }

    #[test]
    fn plugin_id_is_siwx917() {
        assert_eq!(Siwx917Plugin.id(), "SIWX917");
    }

    #[test]
    fn erase_is_rejected_without_opening_a_port() {
        let cancel = AtomicBool::new(false);
        let res = Siwx917Plugin.run(&job(FlashMode::Erase), &cancel, &|_| {});
        assert!(matches!(res, Err(FlashError::Plugin(ref msg)) if msg.contains("erase")));
    }

    #[test]
    fn read_is_rejected_without_opening_a_port() {
        let cancel = AtomicBool::new(false);
        let res = Siwx917Plugin.run(&job(FlashMode::Read), &cancel, &|_| {});
        assert!(matches!(res, Err(FlashError::Plugin(ref msg)) if msg.contains("read-back")));
    }

    #[test]
    fn authorize_is_rejected_without_opening_a_port() {
        let cancel = AtomicBool::new(false);
        let res = Siwx917Plugin.run(&job(FlashMode::Authorize), &cancel, &|_| {});
        assert!(matches!(res, Err(FlashError::Plugin(ref msg)) if msg.contains("authorize")));
    }

    #[test]
    fn flash_rejects_missing_firmware_path() {
        let cancel = AtomicBool::new(false);
        let res = Siwx917Plugin.run(&job(FlashMode::Flash), &cancel, &|_| {});
        assert!(matches!(res, Err(FlashError::InvalidJob(_))));
    }

    // ── ISP menu recognition ────────────────────────────────────────

    /// Captured verbatim from a real board (BootLoader Version 1.1) via
    /// `siwx917_kermit.sh menu-ta`.
    const REAL_MENU: &str = "U\r\nWELCOME TO SILICON LABS\r\nBootLoader Version 1.1\r\n\r\n\
1 Load Default Wireless Firmware\r\nA Load Wireless Firmware (Image No : 0-f)\r\n\
B Burn Wireless Firmware (Image No : 0-f)\r\n5 Select Default Wireless Firmware (Image No : 0-f)\r\n\
K Check Wireless Firmware Integrity (Image No : 0-f)\r\n2 Load Default M4 Firmware\r\n\
3 Load M4 Firmware (Image No : 1-f)\r\n4 Burn M4 Firmware (Image No : 1-f)\r\n\
6 Select Default M4 Firmware (Image No : 1-f)\r\n9 Check M4 Firmware Integrity (Image No : 1-f)\r\n\
F Select M4 and Wireless Images Pair \r\n7 Enable GPIO Based Bypass Mode\r\n\
8 Disable GPIO Based Bypass Mode\r\nQ Update KEY\r\nZ JTAG Selection\r\n\
l Lock/Unlock Debug Interfaces\r\ns Continue Debug Interfaces change\r\no Send OPN\r\n\
b Change UART Baud Rate\r\n";

    #[test]
    fn looks_like_isp_menu_accepts_the_real_captured_menu() {
        assert!(looks_like_isp_menu(REAL_MENU.as_bytes()));
    }

    // ── read_until early exits ──────────────────────────────────────

    /// Hands out chunks as they "arrive", then reports "no data" forever.
    ///
    /// Each chunk carries the delay after which it becomes readable. Modelling arrival time
    /// matters: code that first drains stale bytes and *then* listens looks correct against a
    /// mock that offers everything instantly, yet still misses a report that only shows up a
    /// few hundred milliseconds later — which is precisely the bug real hardware exposed.
    struct Feeder {
        chunks: std::collections::VecDeque<(Duration, Vec<u8>)>,
        start: Instant,
    }
    impl std::io::Read for Feeder {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let elapsed = self.start.elapsed();
            let ready = self
                .chunks
                .front()
                .is_some_and(|(after, _)| elapsed >= *after);
            if !ready {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "nothing available yet",
                ));
            }
            // A chunk may be larger than the caller's buffer (the captured menu is 700+
            // bytes against a 256-byte read buffer), so hand out only what fits and keep
            // the remainder for the next call — like a real serial port.
            let (_, c) = self.chunks.front_mut().expect("checked above");
            let n = c.len().min(buf.len());
            buf[..n].copy_from_slice(&c[..n]);
            c.drain(..n);
            if c.is_empty() {
                self.chunks.pop_front();
            }
            Ok(n)
        }
    }
    impl std::io::Write for Feeder {
        fn write(&mut self, d: &[u8]) -> std::io::Result<usize> {
            Ok(d.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Everything readable immediately.
    fn feeder(chunks: Vec<Vec<u8>>) -> Feeder {
        Feeder {
            chunks: chunks.into_iter().map(|c| (Duration::ZERO, c)).collect(),
            start: Instant::now(),
        }
    }

    /// Chunks that only become readable after the given delay.
    fn feeder_timed(chunks: Vec<(Duration, Vec<u8>)>) -> Feeder {
        Feeder {
            chunks: chunks.into(),
            start: Instant::now(),
        }
    }

    /// A stray CR available at once (the Break ACK's leftover EOL), then the device's real
    /// report only after the drain window has closed.
    fn feeder_post_burn(report: &str) -> Feeder {
        feeder_timed(vec![
            (Duration::ZERO, b"\r".to_vec()),
            (Duration::from_millis(300), report.as_bytes().to_vec()),
        ])
    }

    #[test]
    fn read_until_returns_as_soon_as_accept_matches() {
        // A 60 s window that must not actually be waited out.
        let mut f = feeder(vec![REAL_MENU.as_bytes().to_vec()]);
        let started = Instant::now();
        let got = read_until(
            &mut f,
            Duration::from_secs(60),
            Duration::from_secs(30),
            looks_like_isp_menu,
        );
        assert!(looks_like_isp_menu(&got));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "should return on the accept predicate, not the deadline"
        );
    }

    #[test]
    fn read_until_settles_after_an_idle_gap_once_data_arrived() {
        let mut f = feeder(vec![b"partial".to_vec()]);
        let started = Instant::now();
        let got = read_until(
            &mut f,
            Duration::from_secs(60),
            Duration::from_millis(50),
            |_| false, // never accepts: the idle rule is what must end this
        );
        assert_eq!(got, b"partial");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "should settle on the idle gap, not the deadline"
        );
    }

    #[test]
    fn read_until_honours_the_deadline_when_nothing_ever_arrives() {
        let mut f = feeder(vec![]);
        let started = Instant::now();
        let got = read_until(
            &mut f,
            Duration::from_millis(120),
            Duration::from_millis(10),
            |_| false,
        );
        assert!(got.is_empty());
        // The idle rule must not fire before any data has been seen, so this waits it out.
        assert!(started.elapsed() >= Duration::from_millis(120));
    }

    // ── post-burn report ────────────────────────────────────────────

    /// Captured verbatim from a real wireless burn (Commander, strace on its serial fd).
    const REAL_SUCCESS_REPORT: &str =
        "\r\nSafe Upgrade in Progress ...\r\n\r\nUpgradation Successful\r\n\r\nEnter Next Command\r\n";

    /// Regression: on real hardware the Break ACK's trailing CR was the first thing read, which
    /// armed the idle timer and made the report window expire before the ROM had said anything.
    /// The leading stray byte must not shorten the wait.
    #[test]
    fn confirm_upgrade_survives_the_stray_cr_left_by_the_break_ack() {
        let mut f = feeder_post_burn(REAL_SUCCESS_REPORT);
        let events = std::cell::RefCell::new(Vec::new());
        let res = confirm_upgrade(&mut f, &|e| events.borrow_mut().push(e));
        assert!(res.is_ok());
        assert!(
            events.borrow().iter().any(|e| matches!(
                e,
                FlashEvent::Milestone {
                    milestone: FlashMilestone::VerifyPassed
                }
            )),
            "a leading CR must not cause the real report to be missed"
        );
    }

    #[test]
    fn confirm_upgrade_accepts_the_real_success_report() {
        let mut f = feeder_timed(vec![(
            Duration::from_millis(300),
            REAL_SUCCESS_REPORT.as_bytes().to_vec(),
        )]);
        let events = std::cell::RefCell::new(Vec::new());
        let res = confirm_upgrade(&mut f, &|e| events.borrow_mut().push(e));
        assert!(res.is_ok());
        assert!(events.borrow().iter().any(|e| matches!(
            e,
            FlashEvent::Milestone {
                milestone: FlashMilestone::VerifyPassed
            }
        )));
    }

    #[test]
    fn confirm_upgrade_errors_when_the_device_reports_failure() {
        let mut f = feeder_post_burn("\r\nUpgradation Failed\r\n");
        let res = confirm_upgrade(&mut f, &|_| {});
        assert!(matches!(res, Err(FlashError::Plugin(ref m)) if m.contains("failed upgrade")));
    }

    /// The M4 wording was never captured, so anything unrecognised must warn, not fail —
    /// turning a working flash into a reported error would be the worse mistake.
    #[test]
    fn confirm_upgrade_warns_but_succeeds_on_an_unrecognised_report() {
        let mut f = feeder_post_burn("\r\nsomething we have never seen\r\n");
        let events = std::cell::RefCell::new(Vec::new());
        let res = confirm_upgrade(&mut f, &|e| events.borrow_mut().push(e));
        assert!(res.is_ok());
        assert!(events
            .borrow()
            .iter()
            .any(|e| matches!(e, FlashEvent::Warning { .. })));
    }

    #[test]
    fn read_until_accumulates_across_multiple_reads() {
        let mut f = feeder(vec![b"BootLoader ".to_vec(), b"Burn M4 Firmware ".to_vec()]);
        let got = read_until(
            &mut f,
            Duration::from_secs(5),
            Duration::from_millis(30),
            |_| false,
        );
        assert_eq!(got, b"BootLoader Burn M4 Firmware ");
    }

    #[test]
    fn looks_like_isp_menu_rejects_noise_and_app_log_output() {
        assert!(!looks_like_isp_menu(&[]));
        assert!(!looks_like_isp_menu(b"anything"));
        // Console/VCOM port carrying TuyaOpen application logs must not look like a bootloader.
        assert!(!looks_like_isp_menu(
            b"[01-01 00:00:00 ty D][tkl_gpio.c:120] gpio init\r\n"
        ));
    }

    // ── RPS header classification ───────────────────────────────────

    /// Build a synthetic RPS container with the given control flags and payload length.
    fn rps(control_flags: u16, payload_len: usize) -> Vec<u8> {
        let mut v = vec![0u8; RPS_HEADER_SIZE + payload_len];
        v[0..2].copy_from_slice(&control_flags.to_le_bytes());
        v[4..8].copy_from_slice(&RPS_MAGIC.to_le_bytes());
        v[8..12].copy_from_slice(&(payload_len as u32).to_le_bytes());
        v
    }

    #[test]
    fn detect_classifies_by_control_flags_bit0() {
        // Real observed values: wireless 0x0000; M4 0x0001 (SDK demos) and 0x0041 (local build).
        assert_eq!(ImageKind::detect(&rps(0x0000, 32)).unwrap(), ImageKind::Nwp);
        assert_eq!(ImageKind::detect(&rps(0x0001, 32)).unwrap(), ImageKind::M4);
        assert_eq!(ImageKind::detect(&rps(0x0041, 32)).unwrap(), ImageKind::M4);
        // Bit 6 alone must not read as M4 — that was an early wrong hypothesis from 2 samples.
        assert_eq!(ImageKind::detect(&rps(0x0040, 32)).unwrap(), ImageKind::Nwp);
    }

    #[test]
    fn detect_rejects_non_rps_files() {
        let mut not_rps = rps(0x0001, 32);
        not_rps[4..8].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert!(
            matches!(ImageKind::detect(&not_rps), Err(FlashError::InvalidJob(m)) if m.contains("not an RPS image"))
        );
    }

    #[test]
    fn detect_rejects_truncated_or_padded_images() {
        let mut short = rps(0x0001, 32);
        short.truncate(RPS_HEADER_SIZE + 16); // header still claims 32 payload bytes
        assert!(
            matches!(ImageKind::detect(&short), Err(FlashError::InvalidJob(m)) if m.contains("truncated or padded"))
        );

        let mut padded = rps(0x0001, 32);
        padded.extend_from_slice(&[0u8; 8]);
        assert!(matches!(
            ImageKind::detect(&padded),
            Err(FlashError::InvalidJob(_))
        ));
    }

    #[test]
    fn detect_rejects_file_shorter_than_header() {
        assert!(
            matches!(ImageKind::detect(&[0u8; 16]), Err(FlashError::InvalidJob(m)) if m.contains("too short"))
        );
    }

    #[test]
    fn menu_key_and_slot_match_the_captured_menu_ranges() {
        // M4 slots are advertised as 1-f, wireless as 0-f; we take the lowest valid of each.
        assert_eq!(ImageKind::M4.menu_key(), b'4');
        assert_eq!(ImageKind::M4.image_slot(), b'1');
        assert_eq!(ImageKind::Nwp.menu_key(), b'B');
        assert_eq!(ImageKind::Nwp.image_slot(), b'0');
    }

    // ── segment resolution ──────────────────────────────────────────

    fn seg(path: &str) -> crate::job::FlashSegment {
        crate::job::FlashSegment {
            firmware_path: path.into(),
            start_addr: "0x00000000".into(),
            end_addr: "0x00000000".into(),
        }
    }

    #[test]
    fn resolve_prefers_segments_over_legacy_firmware_path() {
        let mut j = job(FlashMode::Flash);
        j.firmware_path = Some("legacy.rps".into());
        j.segments = Some(vec![seg("from_segments.rps")]);
        assert_eq!(resolve_single_image(&j).unwrap(), "from_segments.rps");
    }

    #[test]
    fn resolve_falls_back_to_firmware_path_when_segments_absent_or_blank() {
        let mut j = job(FlashMode::Flash);
        j.firmware_path = Some("only.rps".into());
        assert_eq!(resolve_single_image(&j).unwrap(), "only.rps");

        // The GUI always sends a segments array; a fresh row has an empty path.
        j.segments = Some(vec![seg("   ")]);
        assert_eq!(resolve_single_image(&j).unwrap(), "only.rps");
    }

    #[test]
    fn resolve_rejects_multiple_images_instead_of_dropping_them() {
        let mut j = job(FlashMode::Flash);
        j.segments = Some(vec![seg("m4.rps"), seg("wireless.rps")]);
        assert!(
            matches!(resolve_single_image(&j), Err(FlashError::InvalidJob(m)) if m.contains("one image per run"))
        );
    }

    #[test]
    fn resolve_rejects_when_nothing_is_provided() {
        let j = job(FlashMode::Flash);
        assert!(matches!(
            resolve_single_image(&j),
            Err(FlashError::InvalidJob(_))
        ));
    }
}
