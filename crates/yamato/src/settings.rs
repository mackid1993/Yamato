// SPDX-License-Identifier: MIT
//
// Yamato - fan control software for ThinkPads
// Copyright (c) 2026 David Brustein

//! The settings window and the curve canvas.
//!
//! Direct2D rather than GDI, because the curve is the point of this program
//! and a fan curve drawn with aliased lines looks like something from 2003.
//! Both Direct2D and DirectWrite ship with Windows, so this costs a device
//! context, not a dependency.
//!
//! Interaction and geometry live in [`crate::curve_editor`]; this module only
//! draws and routes input.

use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::core::{w, Interface, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_COLOR_F, D2D1_GRADIENT_STOP, D2D_POINT_2F, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Graphics::Gdi::{InvalidateRect, ValidateRect};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::curve_editor::{axis_for_level, Editor, Rect as ERect};
use crate::theme;

/// Rounded corners, Windows 11 and later. Ignored gracefully on 10.
const DWMWCP_ROUND: u32 = 2;

const CANVAS_TOP: f32 = 88.0;
/// Width of the live temperature column down the right-hand side.
const READOUT_WIDTH: f32 = 248.0;
/// Width of the settings column beyond it.
///
/// A column of its own, not a section added to the readout: that panel is
/// already full at the default window size, and stealing height from the
/// sensor list would hide readings to show settings.
const TUNING_WIDTH: f32 = 248.0;
const CANVAS_BOTTOM_GAP: f32 = 96.0;

/// The size the window is born at, in DIPs.
///
/// DIPs, and named, because these numbers used to go straight to
/// CreateWindowExW, which counts physical pixels. On the 150% panel a ThinkPad
/// actually has, that made the window two thirds of the size the layout was
/// written for: the graph came out a sliver and the headings ran into one
/// another. At 200% it was half.
const DEFAULT_WIDTH: f32 = 1244.0;
const DEFAULT_HEIGHT: f32 = 720.0;

/// Smallest the window may be dragged to, in DIPs.
///
/// The size at which the layout still holds. Below it the canvas, which is
/// whatever is left after the two fixed columns, runs out; the sensor list no
/// longer fits its panel; and the heading meets the profile picker. Getting
/// below it is meant to be impossible, not merely discouraged.
///
/// The height has gone up by a row's worth each time the settings column
/// gained a row, the eighth and then the ninth: at the old minimum the
/// column's closing caption ran out of the bottom of its panel, and a minimum
/// the layout overflows at is not a minimum.
const MIN_WIDTH: f32 = 900.0;
const MIN_HEIGHT: f32 = 648.0;

/// Room kept for the heading, so the picker beside it never takes the width
/// the words need.
const TITLE_WIDTH: f32 = 110.0;

/// The Save button, which the footer text has to leave room for.
const SAVE_WIDTH: f32 = 132.0;
const SAVE_HEIGHT: f32 = 34.0;

/// How far the surface panel extends past the plot on every side. The plot
/// itself needs breathing room inside its panel or points at the extremes
/// look like they are falling off the edge.
const PANEL_BLEED: f32 = theme::SPACE_MD;

/// Menu ids for the profile list. Above anything else the window uses.
const PROFILE_MENU_BASE: usize = 2000;

/// The management items in the same menu, below the list's base so that one
/// comparison tells the two apart.
const PROFILE_NEW: usize = 1900;
const PROFILE_DUPLICATE: usize = 1901;
const PROFILE_RENAME: usize = 1902;
const PROFILE_DELETE: usize = 1903;
const PROFILE_IMPORT: usize = 1904;

/// The profile picker above the graph. The width is what it would like; a
/// narrow window gives it less rather than letting it reach the heading.
const PICKER_WIDTH: f32 = 216.0;
const PICKER_MIN_WIDTH: f32 = 120.0;
const PICKER_HEIGHT: f32 = 32.0;
const PICKER_TOP: f32 = 16.0;

pub struct Settings {
    window: HWND,
    /// The tray's window, which owns the refresh timer that feeds this one.
    ///
    /// Held in windows-sys form because that is what the tray is written
    /// against, and the only thing done with it is handing it back there.
    tray_window: windows_sys::Win32::Foundation::HWND,
    /// Where the Profile row was last drawn, so it can be clicked.
    ///
    /// Captured during the draw, not computed twice, because a second copy of
    /// the layout arithmetic is a second thing to get out of step.
    profile_row: std::cell::Cell<(f32, f32, f32, f32)>,
    mode_row: std::cell::Cell<(f32, f32, f32, f32)>,
    /// Whether the pointer is over each of those, so they light up the way
    /// the picker does. Without it the accent text is the only thing saying
    /// these rows are live, and a color alone reads as decoration.
    profile_hot: std::cell::Cell<bool>,
    mode_hot: std::cell::Cell<bool>,
    /// The Level row, which exists only while a level is being held. Its
    /// rectangle is cleared in every other mode, so a click cannot land on a
    /// row that is no longer drawn.
    level_row: std::cell::Cell<(f32, f32, f32, f32)>,
    level_hot: std::cell::Cell<bool>,
    /// A mode command that has been sent and not yet come back.
    ///
    /// Without this the rows are unusable. The engine publishes on its poll
    /// interval, five seconds by default, and picks a command up on the same
    /// pass, so for up to that long every sample still describes the mode
    /// being left. Each one overwrote the row, which snapped back, and the
    /// next click stepped from the old value and repeated the step just made.
    /// That is the sticking: not a lost click, but a click undone.
    ///
    /// The level has the same problem twice over, because in Smart mode the
    /// curve moves the control register between samples, so the number a step
    /// counts from moves on its own even when nothing was clicked.
    pending: Option<Pending>,
    /// Where the Save button was last drawn, for the same reason.
    save_button: std::cell::Cell<(f32, f32, f32, f32)>,
    /// Where Discard was last drawn, and empty while there is nothing to
    /// throw away.
    discard_button: std::cell::Cell<(f32, f32, f32, f32)>,
    /// Where the profile picker was last drawn.
    picker_box: std::cell::Cell<(f32, f32, f32, f32)>,
    /// Whether the pointer is over that picker.
    ///
    /// Kept, not asked for at paint time, so an ordinary redraw does not go to
    /// the window manager for a cursor position. Re-checked with every sample,
    /// because a pointer can leave the window without the window being told.
    picker_hot: std::cell::Cell<bool>,
    /// Where each settings row was last drawn, in the order of [`KNOBS`].
    tuning_rows: std::cell::Cell<[(f32, f32, f32, f32); KNOBS.len()]>,
    /// Which of those the pointer is over, if any.
    ///
    /// A row shows a name and a number and no reason for either. Rather than
    /// grow a tooltip window, the paragraph already under these rows says what
    /// the one being pointed at is for, and goes back to its own words when
    /// the pointer leaves.
    hot_knob: std::cell::Cell<Option<usize>>,
    /// And the two rows for the chosen curve point, in the order of
    /// [`POINT_KNOBS`].
    point_rows: std::cell::Cell<[(f32, f32, f32, f32); POINT_KNOBS.len()]>,
    /// The sensor list as left, top, right and the height of one row. Stored
    /// this way because the rows are identical and evenly spaced, so which one
    /// the pointer is over is arithmetic, not a search.
    sensor_rows: std::cell::Cell<(f32, f32, f32, f32)>,
    factory: ID2D1Factory,
    target: Option<ID2D1HwndRenderTarget>,
    dwrite: IDWriteFactory,
    // The type ramp, one format per role. Alignment lives on the format, not
    // in rect arithmetic, so a column of values stays a column when a font
    // metric shifts under us.
    title: IDWriteTextFormat,
    big: IDWriteTextFormat,
    body: IDWriteTextFormat,
    value: IDWriteTextFormat,
    caption: IDWriteTextFormat,
    axis: IDWriteTextFormat,
    axis_level: IDWriteTextFormat,
    section: IDWriteTextFormat,
    chip: IDWriteTextFormat,
    /// Centered both ways, which is what a label inside a button wants and
    /// nothing else here does.
    button: IDWriteTextFormat,
    /// One line, trimmed with an ellipsis when there is not room for it. The
    /// hint under the heading and the name in the picker both share their row
    /// with something that must not be run under.
    hint: IDWriteTextFormat,
    picker: IDWriteTextFormat,
    /// Round caps and joins for the curve, so step corners meet cleanly
    /// instead of leaving little square notches.
    round_stroke: ID2D1StrokeStyle,
    /// Short dashes for reference lines: the live marker and the BIOS level.
    dash_stroke: ID2D1StrokeStyle,
    editor: Editor,
    /// Latest sample from the engine, shown as numbers beside the graph. The
    /// curve alone tells you what *would* happen; this tells you what is.
    readout: Readout,
    /// The profile the curve on screen came from, and the one it will be
    /// written back to.
    ///
    /// Held explicitly, not looked up when saving. The tray can switch profiles
    /// while this window is open, so taking the file's answer at save time
    /// meant a curve dragged into shape under one profile was saved into
    /// another, and deleting a profile from the tray could land its curve on
    /// top of a neighbor's.
    editing: String,
    /// The settings file as it stands, for everything on screen that is a
    /// setting rather than a reading: the unit, the ignored sensors, the
    /// tuning rows.
    ///
    /// Re-read with every sample, not remembered from when the window opened,
    /// because the tray writes to the same file: a window that only looked once
    /// would go on contradicting the tooltip until it was closed and reopened.
    ///
    /// The unit is display only, and only text. The curve is stored, evaluated
    /// and plotted in Celsius whatever it says, because that is the unit the
    /// controller speaks and converting on the way in and out would round
    /// twice.
    config: yamato_core::Config,
}

/// The settings as they stand on disk, or the defaults when the file cannot be
/// read, which is also what an unwritten setting means.
///
/// The file is the only place the tray and this window can agree about any of
/// this, since the two live in the same process but neither owns the other's
/// copy.
fn settings_on_disk() -> yamato_core::Config {
    yamato_core::Config::load(&yamato_core::Config::default_path()).unwrap_or_default()
}

/// What was asked for, and how long to keep showing it while waiting.
#[derive(Clone, Copy)]
struct Pending {
    mode: u8,
    level: u8,
    deadline: Instant,
}

/// What the engine last reported. Everything here is displayed.
#[derive(Debug, Clone, Default)]
pub struct Readout {
    pub sensors: [Option<i8>; yamato_ec::SENSOR_COUNT],
    pub hottest: Option<(usize, i8)>,
    pub fan_rpm: [u16; 2],
    pub mode: &'static str,
    /// The same thing as one of the `ipc::MODE_` values. The string above is
    /// for reading; this is for deciding, and clicking the Mode row needs to
    /// know what it is switching away from without matching on prose.
    pub mode_raw: u8,
    /// The fan control register as the engine last saw it: a level in 0..=7,
    /// or `FAN_BIOS`, or the disengaged value this program refuses to set.
    /// The Level row reads a level out of it, and entering a held level
    /// starts from whatever the fan was already doing.
    pub fan_ctrl: u8,
    pub profile: String,
    pub fault: bool,
    /// Which kind of trouble, as one of the `ipc::STATUS_` values. Zero when
    /// there is none, which is also what an engine too old to publish it
    /// leaves behind.
    pub status: u8,
    /// Set by the tray when the pattern of failures suggests a single-fan
    /// machine with the Single fan setting still off. The tray owns the
    /// judgment, because it holds the session memory that judgment needs:
    /// whether the second fan has ever once reported a speed. This window
    /// only shows the conclusion.
    pub single_fan_hint: bool,
    /// The controller mode worth suggesting, as one of the
    /// `ipc::LAYOUT_HINT_` values, or zero for nothing. The engine owns this
    /// judgment: it knows which mode it is driving, why, and how long the
    /// controller has been out of reach. This window only shows the
    /// sentence.
    pub layout_hint: u8,
}

/// Sensor names, in the order the controller reports them. The same layout
/// every ThinkPad tool uses, because it is the hardware's, not ours.
pub const SENSOR_NAMES: [&str; yamato_ec::SENSOR_COUNT] = [
    "CPU", "GPU", "Board", "PCI", "Fan", "Battery", "Slot", "Bus", "Board2", "Battery2", "Board3",
    "Board4",
];

/// One setting that can be changed from the window, drawn as a row.
///
/// These were reachable only by hand-editing the settings file, which is the
/// one thing this program set out not to make anyone do. Each row cycles
/// through a short list of sensible values on a click: a hand-drawn window can
/// have a row that answers the mouse far more cheaply than a text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    Poll,
    Watchdog,
    Escape,
    StandbyPoll,
    StartIn,
    Logging,
    ShowWindow,
    SingleFan,
    EcLayout,
}

/// The rows, in the order they are drawn.
///
/// Controller mode is last on purpose: it is the advanced override for which
/// ports reach the embedded controller, set once by the first boot's probe
/// and then never an everyday setting, and it should not sit among the rows
/// people actually visit.
pub const KNOBS: [Knob; 9] = [
    Knob::Poll,
    Knob::Watchdog,
    Knob::Escape,
    Knob::StandbyPoll,
    Knob::StartIn,
    Knob::Logging,
    Knob::ShowWindow,
    Knob::SingleFan,
    Knob::EcLayout,
];

/// Poll intervals worth offering. Fine grained where it matters, because the
/// difference between one second and five is something people feel, and coarse
/// above that, where it is only battery.
const POLL_PRESETS: [u32; 8] = [1, 2, 3, 5, 10, 15, 30, 60];
const STANDBY_POLL_PRESETS: [u32; 6] = [5, 10, 15, 30, 60, 120];
/// Watchdogs long enough that a slow pass is not mistaken for a stall. The
/// floor computed from the poll intervals may raise whichever is chosen, so
/// the first few are unreachable on a slow poll; that is the honest outcome,
/// and the row shows what is actually in force.
const WATCHDOG_PRESETS: [u32; 5] = [30, 60, 90, 120, 180];

/// Where a held manual level is abandoned. The reference implementation calls
/// this ManModeExit and 78 is a common answer, so it is on the list.
const ESCAPE_PRESETS: [u32; 8] = [60, 65, 70, 75, 78, 80, 85, 90];

/// The next preset above `current`, wrapping round to the first.
///
/// Not "the one after whichever matches": a value from a hand-edited file
/// matches nothing in the list, and a click should still move it somewhere
/// sensible instead of snapping to the start.
fn next_preset(presets: &[u32], current: u32) -> u32 {
    presets.iter().copied().find(|p| *p > current).unwrap_or(presets[0])
}

impl Knob {
    fn label(self) -> &'static str {
        match self {
            Knob::Poll => "Poll",
            Knob::Watchdog => "Watchdog",
            Knob::Escape => "Manual escape",
            Knob::StandbyPoll => "Standby poll",
            Knob::StartIn => "Start in",
            Knob::Logging => "Logging",
            Knob::ShowWindow => "Open at start",
            Knob::SingleFan => "Fans",
            Knob::EcLayout => "Controller mode",
        }
    }

    /// One sentence on what the row is for, shown while the pointer is on it.
    ///
    /// Every one of these has to fit the paragraph under the rows, which is
    /// two lines at caption size and holds about eighty characters. A test
    /// keeps them inside that: an overlong description is clipped at the
    /// bottom of its box rather than wrapped somewhere sensible, and the
    /// sentence that explains a setting is a poor place to lose the end of.
    fn describe(self) -> &'static str {
        match self {
            Knob::Poll => "How often the sensors are read and the fan level decided again.",
            Knob::Watchdog => {
                "If no decision happens for this long, the fan goes back to the firmware."
            }
            Knob::Escape => {
                "A fixed level is held whatever happens. This is the temperature that ends it."
            }
            Knob::StandbyPoll => {
                "The poll interval once the machine is asleep. Slower, to save its battery."
            }
            Knob::StartIn => {
                "Which mode the engine begins in when it starts. Nothing here moves the fan."
            }
            Knob::Logging => {
                "Writes readings and decisions to a CSV in ProgramData, rotated so it cannot grow."
            }
            Knob::ShowWindow => {
                "Whether this window opens when Yamato starts, or it goes straight to the tray."
            }
            Knob::SingleFan => {
                "Whether this machine has a second fan. Set Single if fan 2 never reports a speed."
            }
            Knob::EcLayout => {
                "Advanced. Applies at the next start. The wrong mode stops fan control."
            }
        }
    }

    /// What the row reads right now.
    fn value(self, config: &yamato_core::Config) -> String {
        match self {
            Knob::Poll => format!("{} s", config.poll_secs),
            Knob::Watchdog => format!("{} s", config.watchdog_secs),
            // In whichever unit the rest of the window is showing, since it is
            // a temperature like any other on screen.
            Knob::Escape => format!(
                "{}{}",
                yamato_core::display_temp(config.manual_escape_c, config.fahrenheit),
                yamato_core::unit_suffix(config.fahrenheit)
            ),
            Knob::StandbyPoll => format!("{} s", config.standby_poll_secs),
            Knob::StartIn => match config.startup_mode {
                yamato_core::StartupMode::Bios => "BIOS".to_string(),
                yamato_core::StartupMode::Smart => "Smart".to_string(),
            },
            Knob::Logging => if config.log_enabled { "On" } else { "Off" }.to_string(),
            Knob::ShowWindow => if config.show_window_on_start { "Window" } else { "Tray only" }.to_string(),
            Knob::SingleFan => if config.single_fan { "Single" } else { "Dual" }.to_string(),
            // A file the probe has not decided yet shows Standard, which is
            // where almost every machine lives and what the engine falls
            // back to; the service writes the real answer at its first
            // start, normally long before this window first opens.
            Knob::EcLayout => match config.ec_layout {
                Some(yamato_core::EcLayout::Compat) => "Compatibility",
                _ => "Standard",
            }
            .to_string(),
        }
    }

    /// Moves the setting on one step.
    ///
    /// Everything this can produce is already inside the range the loader
    /// enforces, so a row can never show one value while the engine runs
    /// another. The watchdog is re-floored after any poll interval change for
    /// the same reason: a slower poll raises the shortest watchdog that will
    /// not fire during ordinary running, and the loader would otherwise raise
    /// it silently after the fact.
    fn cycle(self, config: &mut yamato_core::Config) {
        match self {
            Knob::Poll => {
                config.poll_secs = next_preset(&POLL_PRESETS, config.poll_secs)
                    .clamp(yamato_core::POLL_SECS_MIN, yamato_core::POLL_SECS_MAX);
            }
            Knob::Watchdog => {
                config.watchdog_secs = next_preset(&WATCHDOG_PRESETS, config.watchdog_secs);
            }
            Knob::Escape => {
                // Always in Celsius underneath, whatever the row is showing:
                // the engine compares against the controller's own unit.
                let next = next_preset(&ESCAPE_PRESETS, config.manual_escape_c.max(0) as u32);

                config.manual_escape_c = (next as i8)
                    .clamp(yamato_core::MANUAL_ESCAPE_MIN, yamato_core::MANUAL_ESCAPE_MAX);
            }
            Knob::StandbyPoll => {
                config.standby_poll_secs =
                    next_preset(&STANDBY_POLL_PRESETS, config.standby_poll_secs).clamp(
                        yamato_core::STANDBY_POLL_SECS_MIN,
                        yamato_core::STANDBY_POLL_SECS_MAX,
                    );
            }
            Knob::StartIn => {
                // Which mode the engine begins in, not which mode it is in now.
                // Nothing here touches the running fan.
                config.startup_mode = match config.startup_mode {
                    yamato_core::StartupMode::Bios => yamato_core::StartupMode::Smart,
                    yamato_core::StartupMode::Smart => yamato_core::StartupMode::Bios,
                };
            }
            Knob::Logging => config.log_enabled = !config.log_enabled,
            Knob::ShowWindow => {
                config.show_window_on_start = !config.show_window_on_start
            }
            // One fan or two. Off, meaning both fans fully verified, is the
            // honest default; the config field explains why this is asked of
            // a person instead of guessed from the hardware.
            Knob::SingleFan => config.single_fan = !config.single_fan,
            // Between the two concrete modes, always: this row is an
            // override, not a reset, and it writes an answer the engine
            // drives at its next start without validating it away. A file
            // the probe has not decided yet reads as Standard, so the first
            // click shows Compatibility, matching what the row displayed.
            Knob::EcLayout => {
                config.ec_layout = Some(match config.ec_layout {
                    Some(yamato_core::EcLayout::Compat) => yamato_core::EcLayout::Standard,
                    _ => yamato_core::EcLayout::Compat,
                });
            }
        }

        config.watchdog_secs = config
            .watchdog_secs
            .max(yamato_core::watchdog_floor(config.poll_secs, config.standby_poll_secs));
    }
}

/// The two things that can be changed about the chosen curve point.
///
/// Named for what they do, not for what they are called. "Hysteresis" explains
/// nothing to anybody who did not already know it, and nobody tuning their fan
/// should have to learn the word to find out why it keeps changing speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointKnob {
    SlowDown,
    SpeedUp,
}

pub const POINT_KNOBS: [PointKnob; 2] = [PointKnob::SlowDown, PointKnob::SpeedUp];

impl PointKnob {
    fn label(self) -> &'static str {
        match self {
            PointKnob::SlowDown => "Slows down",
            PointKnob::SpeedUp => "Speeds up",
        }
    }

    /// The value as a phrase, so the row reads as a sentence about the fan
    /// instead of a number needing a manual.
    fn value(self, point: &yamato_core::CurvePoint) -> String {
        let degrees = match self {
            PointKnob::SlowDown => point.hyst_down,
            PointKnob::SpeedUp => point.hyst_up,
        };

        if degrees == 0 {
            return "right away".to_string();
        }

        match self {
            PointKnob::SlowDown => format!("{degrees}\u{00b0} cooler"),
            PointKnob::SpeedUp => format!("{degrees}\u{00b0} hotter"),
        }
    }
}

/// Which row of an evenly spaced list a point falls in.
///
/// Free-standing so it can be tested without a window, which is the only way
/// anything in this file gets tested. A height of zero means no list was drawn,
/// which is what the fault panel leaves behind: nothing visible, so nothing
/// clickable.
fn row_at(top: f32, height: f32, y: f32, count: usize) -> Option<usize> {
    if height <= 0.0 || y < top {
        return None;
    }

    let index = ((y - top) / height) as usize;

    (index < count).then_some(index)
}

/// The scale of the display the window is about to open on.
///
/// [`Settings::scale`] asks the window, which is the right answer once there is
/// one; this runs before that. The monitor under the pointer is where a new
/// window lands in practice, and on a desk with two displays at different
/// scalings it beats the system's idea of a single DPI.
fn scale_for_a_new_window() -> f32 {
    let mut point = POINT::default();

    unsafe {
        if GetCursorPos(&mut point).is_err() {
            return 1.0;
        }

        let monitor = windows::Win32::Graphics::Gdi::MonitorFromPoint(
            point,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );

        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() {
            return 1.0;
        }

        if dpi_x == 0 {
            1.0
        } else {
            dpi_x as f32 / 96.0
        }
    }
}

/// A text format with its alignment baked in, because half of typographic
/// tidiness is refusing to position text by nudging rectangles.
unsafe fn text_format(
    dwrite: &IDWriteFactory,
    family: PCWSTR,
    weight: DWRITE_FONT_WEIGHT,
    size: f32,
    align: DWRITE_TEXT_ALIGNMENT,
    para: DWRITE_PARAGRAPH_ALIGNMENT,
) -> Result<IDWriteTextFormat> {
    let format = dwrite.CreateTextFormat(
        family,
        None,
        weight,
        DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_STRETCH_NORMAL,
        size,
        w!("en-us"),
    )?;
    format.SetTextAlignment(align)?;
    format.SetParagraphAlignment(para)?;

    Ok(format)
}

/// Makes a format lose its tail instead of running under its neighbor.
///
/// For strings that are long, expendable and share a row with something else:
/// a hint, or a profile name somebody gave sixty characters to. Never for a
/// temperature or a fan speed, where the tail is the number.
unsafe fn elide(dwrite: &IDWriteFactory, format: &IDWriteTextFormat) -> Result<()> {
    // Wrapping and trimming are alternatives: text that is allowed to wrap
    // never gets long enough on one line to be trimmed, it just grows
    // downwards into whatever is under it.
    format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;

    let sign = dwrite.CreateEllipsisTrimmingSign(format)?;
    format.SetTrimming(
        &DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        },
        &sign,
    )?;

    Ok(())
}

impl Settings {
    /// `editing` names the profile the editor was built from, so that saving
    /// cannot land somewhere else.
    pub fn new(
        editor: Editor,
        editing: String,
        tray_window: windows_sys::Win32::Foundation::HWND,
    ) -> Result<Box<Self>> {
        unsafe {
            let instance = GetModuleHandleW(None)?;

            let class = w!("YamatoSettings");
            let wc = WNDCLASSW {
                // CS_DBLCLKS or there are no double clicks: Windows only
                // synthesizes WM_LBUTTONDBLCLK for a class that asked for it,
                // so adding a curve point by double-clicking the graph, which
                // the window tells you to do in two places, had never worked.
                style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                // The mark compiled into the executable, the same one the tray
                // and File Explorer show. Left unset, the title bar and
                // Alt-Tab fall back to a blank sheet of paper.
                hIcon: LoadIconW(
                    windows::Win32::Foundation::HINSTANCE(instance.0),
                    PCWSTR(1 as *const u16),
                )
                .unwrap_or_default(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);

            // Scaled here, because CreateWindowExW counts physical pixels and
            // every number in the layout is a DIP.
            let scale = scale_for_a_new_window();

            let title = format!("Yamato {}", env!("CARGO_PKG_VERSION"));
            let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

            let window = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                PCWSTR(title_wide.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                (DEFAULT_WIDTH * scale) as i32,
                (DEFAULT_HEIGHT * scale) as i32,
                None,
                None,
                instance,
                None,
            )?;

            // Dark titlebar, so the frame does not sit in bright chrome above
            // a near-black window.
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(
                window,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const _ as *const _,
                size_of::<i32>() as u32,
            );
            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                window,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const _,
                size_of::<u32>() as u32,
            );

            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            // Segoe UI Variable where it exists, Segoe UI elsewhere. Both are
            // present on any Windows this program runs on. Display for the
            // large sizes, Text for the small: the Display face is drawn for
            // headlines and looks starved at caption sizes.
            let display = w!("Segoe UI Variable Display");
            let text_face = w!("Segoe UI Variable Text");

            let title = text_format(
                &dwrite, display, DWRITE_FONT_WEIGHT_SEMI_BOLD, 20.0,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
            )?;
            // The hottest temperature is the one number people open this
            // window for, so it gets a properly large size, not "title but
            // slightly bigger".
            let big = text_format(
                &dwrite, display, DWRITE_FONT_WEIGHT_SEMI_BOLD, 34.0,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
            )?;
            let body = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_NORMAL, 12.0,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            )?;
            // Values are right-aligned and a touch heavier than their labels,
            // which is all a key-value table needs to read as one.
            let value = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_SEMI_BOLD, 12.0,
                DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            )?;
            let caption = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_NORMAL, 11.0,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
            )?;
            // Axis labels center on their gridline; level labels right-align
            // toward the plot they describe.
            let axis = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_NORMAL, 11.0,
                DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
            )?;
            let axis_level = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_NORMAL, 11.0,
                DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            )?;
            let section = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_SEMI_BOLD, 10.0,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            )?;
            let chip = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_SEMI_BOLD, 11.0,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            )?;
            let button = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_SEMI_BOLD, 12.0,
                DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            )?;
            // The two that share a row with something they must not reach.
            let hint = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_NORMAL, 11.0,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
            )?;
            elide(&dwrite, &hint)?;
            let picker = text_format(
                &dwrite, text_face, DWRITE_FONT_WEIGHT_SEMI_BOLD, 12.0,
                DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            )?;
            elide(&dwrite, &picker)?;

            let round_stroke = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 10.0,
                    dashStyle: D2D1_DASH_STYLE_SOLID,
                    dashOffset: 0.0,
                },
                None,
            )?;
            let dash_stroke = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_FLAT,
                    endCap: D2D1_CAP_STYLE_FLAT,
                    dashCap: D2D1_CAP_STYLE_FLAT,
                    lineJoin: D2D1_LINE_JOIN_MITER,
                    miterLimit: 10.0,
                    dashStyle: D2D1_DASH_STYLE_CUSTOM,
                    dashOffset: 0.0,
                },
                Some(&[3.0, 3.0]),
            )?;

            let mut this = Box::new(Settings {
                window,
                tray_window,
                profile_row: std::cell::Cell::new((0.0, 0.0, 0.0, 0.0)),
                mode_row: std::cell::Cell::new((0.0, 0.0, 0.0, 0.0)),
                profile_hot: std::cell::Cell::new(false),
                mode_hot: std::cell::Cell::new(false),
                level_row: std::cell::Cell::new((0.0, 0.0, 0.0, 0.0)),
                level_hot: std::cell::Cell::new(false),
                pending: None,
                save_button: std::cell::Cell::new((0.0, 0.0, 0.0, 0.0)),
                discard_button: std::cell::Cell::new((0.0, 0.0, 0.0, 0.0)),
                picker_box: std::cell::Cell::new((0.0, 0.0, 0.0, 0.0)),
                picker_hot: std::cell::Cell::new(false),
                tuning_rows: std::cell::Cell::new([(0.0, 0.0, 0.0, 0.0); KNOBS.len()]),
                hot_knob: std::cell::Cell::new(None),
                point_rows: std::cell::Cell::new([(0.0, 0.0, 0.0, 0.0); POINT_KNOBS.len()]),
                sensor_rows: std::cell::Cell::new((0.0, 0.0, 0.0, 0.0)),
                factory,
                target: None,
                dwrite,
                title,
                big,
                body,
                value,
                caption,
                axis,
                axis_level,
                section,
                chip,
                button,
                hint,
                picker,
                round_stroke,
                dash_stroke,
                editor,
                editing,
                readout: Readout::default(),
                config: settings_on_disk(),
            });

            // Only once the box has settled is the address stable enough to
            // hand to the window procedure.
            SetWindowLongPtrW(window, GWLP_USERDATA, this.as_mut() as *mut Settings as isize);

            // The minimum this window enforces for itself arrives as
            // WM_GETMINMAXINFO, which Windows sends *during* CreateWindowExW,
            // before the line above, so with nothing behind the pointer to
            // answer it. The clamp could not apply to the window's own birth,
            // only to a later drag. This is where a window born too small is
            // put right.
            this.enforce_minimum();

            let _ = ShowWindow(window, SW_SHOW);

            Ok(this)
        }
    }

    /// The window itself, for the tray, which owns when this is on screen.
    ///
    /// Handed back in windows-sys form because that is the crate the tray is
    /// written against. Both crates describe an HWND as the same pointer, so
    /// this changes the spelling, not the value.
    pub fn hwnd(&self) -> windows_sys::Win32::Foundation::HWND {
        self.window.0
    }

    /// Pops the profile list under the Profile row.
    ///
    /// Switching profiles here saves the curve on screen first. Without that,
    /// edits made since the window opened would be silently discarded by the
    /// switch, which is a surprising way to lose work.
    /// Clicking the Mode row moves the fan between the firmware's control and
    /// the curve.
    ///
    /// Two stops, not three. A fixed level is the one mode that switches the
    /// firmware's thermal management off and holds it off, and nothing in this
    /// program arrives at that by cycling: the tray asks for a level from a
    /// submenu and no hotkey sets one at all, on purpose. So a click from
    /// Manual lands on the curve, which is the direction that gives
    /// management back rather than the one that takes it away.
    ///
    /// A command rather than a config write, because the mode is the engine's
    /// to hold: the file says what to start in, not what is running now.
    fn cycle_mode(&mut self) {
        let next = match self.readout.mode_raw {
            crate::ipc::MODE_BIOS => crate::ipc::MODE_SMART,
            crate::ipc::MODE_SMART => crate::ipc::MODE_MANUAL,
            _ => crate::ipc::MODE_BIOS,
        };

        // Entering a held level starts from the one the fan is already
        // running at, which in Smart mode is whatever the curve just asked
        // for. Landing on the current speed makes this a change of who is
        // deciding rather than a change of speed, and it means the step never
        // arrives as a sudden quiet or a sudden roar.
        let level = if next == crate::ipc::MODE_MANUAL { self.held_level() } else { 0 };

        self.post_mode(next, level);
    }

    /// Steps the held level, 1 through 7 and back to 1.
    ///
    /// Zero is not in the ring. It is a level the controller accepts and this
    /// program refuses to hold, because it means the fan stops with the
    /// firmware's own thermal management switched off, and the way out of it
    /// is a temperature limit rather than a fan.
    fn cycle_level(&mut self) {
        let next = self.held_level() % yamato_ec::FAN_LEVEL_MAX + 1;

        self.post_mode(crate::ipc::MODE_MANUAL, next);
    }

    /// The level to show, and the one a step counts from.
    ///
    /// Read out of the control register rather than remembered, so it follows
    /// the engine. Anything that is not a level it makes sense to hold -- the
    /// firmware's own value, the disengaged one, or zero -- answers with a
    /// middle level instead, which is what entering a held level from BIOS
    /// mode has to start at when there is no level to inherit.
    fn held_level(&self) -> u8 {
        match self.readout.fan_ctrl {
            l @ 1..=yamato_ec::FAN_LEVEL_MAX => l,
            _ => 4,
        }
    }

    /// Sends a mode, and shows it until the engine agrees or stops answering.
    ///
    /// A command rather than a config write, because the mode is the engine's
    /// to hold: the file says what to start in, not what is running now.
    fn post_mode(&mut self, mode: u8, level: u8) {
        let Some(channel) = crate::ipc::Channel::attach() else {
            return;
        };

        // The empty name is "leave the profile alone". Which curve is
        // selected is not a statement about who should drive the fan.
        channel.post_command(mode, level, "");

        // Two polls and a little, because the command is picked up on a pass
        // and its result published by the next one. Waiting a fixed few
        // seconds instead would expire early on a slow poll and leave the row
        // snapping back, which is the bug this exists to fix.
        let wait = Duration::from_secs(u64::from(self.config.poll_secs) * 2 + 2);

        self.pending = Some(Pending { mode, level, deadline: Instant::now() + wait });
        self.show_pending();
        self.invalidate();
    }

    /// Decides whether the outstanding command has landed, and holds the rows
    /// on what was asked for until it has.
    ///
    /// The deadline matters as much as the match does. A command that never
    /// arrives -- an engine that has stopped, a write the controller refused
    /// -- must stop being shown, or the window would report a mode nothing is
    /// running. When it expires the rows go back to the truth on their own.
    fn settle_pending(&mut self) {
        let Some(p) = self.pending else { return };

        let arrived = self.readout.mode_raw == p.mode
            && (p.mode != crate::ipc::MODE_MANUAL || self.readout.fan_ctrl == p.level);

        if arrived || Instant::now() >= p.deadline {
            self.pending = None;
            return;
        }

        self.show_pending();
    }

    /// Writes the outstanding command over the readout, so every part of this
    /// window agrees: what is drawn, and what the next click counts from.
    fn show_pending(&mut self) {
        let Some(p) = self.pending else { return };

        self.readout.mode_raw = p.mode;
        self.readout.mode = match p.mode {
            crate::ipc::MODE_SMART => "Smart",
            crate::ipc::MODE_MANUAL => "Manual",
            _ => "BIOS",
        };

        if p.mode == crate::ipc::MODE_MANUAL {
            self.readout.fan_ctrl = p.level;
        }
    }

    fn show_profile_menu(&mut self) {
        // The menu below runs a loop of its own that dispatches for every
        // window on the thread, and this call already holds a `&mut Settings`.
        // Left running, the tray's refresh timer would fire inside that loop
        // and hand this window a new readout through a second `&mut Settings`,
        // and overwrite the profile the menu is in the middle of asking about.
        let _pause = crate::tray::TimerPause::new(self.tray_window);

        let path = yamato_core::Config::default_path();
        let Ok(config) = yamato_core::Config::load(&path) else {
            return;
        };

        let names: Vec<String> = config.profiles.iter().map(|p| p.name.clone()).collect();
        if names.is_empty() {
            return;
        }

        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };

            // Owned UTF-16 kept alive across the AppendMenuW calls: a
            // temporary would be freed while the menu still pointed at it.
            let wide: Vec<Vec<u16>> = names
                .iter()
                .map(|n| n.encode_utf16().chain(std::iter::once(0)).collect())
                .collect();

            let selected = self.live_profile();

            for (i, name) in names.iter().enumerate() {
                let checked = if *name == selected { MF_CHECKED } else { MF_UNCHECKED };
                let _ = AppendMenuW(
                    menu,
                    MF_STRING | checked,
                    PROFILE_MENU_BASE + i,
                    PCWSTR(wide[i].as_ptr()),
                );
            }

            // Managing profiles from the same menu that lists them, because
            // the tray is not where someone editing a curve is looking, and
            // both places go through one piece of code to do it.
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, PROFILE_NEW, w!("New..."));
            let _ = AppendMenuW(menu, MF_STRING, PROFILE_DUPLICATE, w!("Duplicate..."));
            let _ = AppendMenuW(menu, MF_STRING, PROFILE_RENAME, w!("Rename..."));
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                PROFILE_IMPORT,
                w!("Import from TPFanControl..."),
            );

            // Deleting the last profile would leave nothing to run, so the item
            // is present but unavailable. Missing would read as a bug.
            let can_delete = if names.len() > 1 { MF_ENABLED } else { MF_GRAYED };
            let _ = AppendMenuW(menu, MF_STRING | can_delete, PROFILE_DELETE, w!("Delete"));

            let (rl, _, _, rb) = self.picker_box.get();
            let mut point = POINT { x: rl as i32, y: rb as i32 };
            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(self.window, &mut point);

            let _ = SetForegroundWindow(self.window);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                point.x,
                point.y,
                0,
                self.window,
                None,
            );
            let _ = DestroyMenu(menu);

            if chosen.0 <= 0 {
                return;
            }

            let chosen = chosen.0 as usize;

            // Everything below the list's base is a management action. Checked
            // first, or the subtraction that follows would run off the bottom.
            if chosen < PROFILE_MENU_BASE {
                if chosen == PROFILE_IMPORT {
                    self.import_profile();
                    return;
                }

                let action = match chosen {
                    PROFILE_NEW => crate::tray::ProfileAction::New,
                    PROFILE_DUPLICATE => crate::tray::ProfileAction::Duplicate,
                    PROFILE_RENAME => crate::tray::ProfileAction::Rename,
                    PROFILE_DELETE => crate::tray::ProfileAction::Delete,
                    _ => return,
                };

                self.manage_profile(action);
                return;
            }

            let Some(name) = names.get(chosen - PROFILE_MENU_BASE) else {
                return;
            };
            if *name == selected {
                return;
            }

            // Save the curve on screen before moving off it, or the edits
            // since the window opened are lost to the switch.
            if self.apply().is_err() {
                // Refusing to switch is the right failure: carrying on would
                // discard the editor's contents to move to another profile.
                return;
            }

            // Re-read, because apply() has just written the file. Reusing the
            // copy loaded before it would save a stale curve straight back
            // over the one just applied.
            let Ok(mut config) = yamato_core::Config::load(&path) else {
                return;
            };

            config.active_profile = name.clone();
            if config.save(&path).is_err() {
                return;
            }

            // A command, not just a file write: the engine keeps running the
            // profile it started with when the file changes underneath it, so
            // saving alone would switch the window and leave the fan on the
            // old curve.
            //
            // MODE_KEEP, because picking a curve to look at is not a statement
            // about who should be driving the fan. Echoing the published mode
            // here cleared the 80 C manual escape latch and canceled any
            // pending recovery from a fault.
            if let Some(channel) = crate::ipc::Channel::attach() {
                channel.post_command(crate::ipc::MODE_KEEP, 0, name);
            }

            // Show the new curve now instead of waiting a poll, and move the
            // editor's target with it: from here on, saving means saving this
            // profile.
            if let Ok(curve) = config.active_curve() {
                let live = self.editor.live_temp();
                self.editor = Editor::new(&curve);
                self.editor.set_live_temp(live);
            }

            self.editing = config.active_profile.clone();
            self.config = config;
            self.invalidate();
        }
    }

    /// New, Duplicate, Rename or Delete, from the picker's own menu.
    ///
    /// The rules are the tray's rules, because it is the tray's code: only the
    /// asking and the telling happen here. Called from inside
    /// [`Settings::show_profile_menu`], which is holding the modal pause that
    /// keeps the tray's timer out of the name box below.
    fn manage_profile(&mut self, action: crate::tray::ProfileAction) {
        let active = self.live_profile();

        // What is on screen goes into the file before it is copied or renamed,
        // or a duplicate would be made of the curve as it was, not as it looks.
        // Deleting is exempt: no sense saving a profile on the way to removing
        // it.
        if action != crate::tray::ProfileAction::Delete
            && self.editor.is_dirty()
            && self.apply().is_err()
        {
            // The curve on screen does not validate. The heading already says
            // why, and carrying on would lose it.
            return;
        }

        let name = match action {
            crate::tray::ProfileAction::New => {
                crate::prompt::ask(self.hwnd(), "New profile", "")
            }
            crate::tray::ProfileAction::Duplicate => {
                crate::prompt::ask(self.hwnd(), "Duplicate profile", &format!("{active} copy"))
            }
            crate::tray::ProfileAction::Rename => {
                crate::prompt::ask(self.hwnd(), "Rename profile", &active)
            }
            // Asked here as well as in the tray. The same action offered in
            // two places has to behave the same in both, or the safer one is
            // whichever the user happened not to use.
            crate::tray::ProfileAction::Delete => {
                let answer = unsafe {
                    MessageBoxW(
                        self.window,
                        &windows::core::HSTRING::from(format!(
                            "Delete the profile \"{active}\"?\n\n\
                             Its curve goes with it, and there is no undo."
                        )),
                        windows::core::w!("Yamato"),
                        MB_YESNO | MB_ICONWARNING,
                    )
                };

                if answer == IDYES {
                    Some(active.clone())
                } else {
                    None
                }
            }
        };

        let Some(name) = name else { return };

        let config = match crate::tray::apply_profile_action(action, &active, &name) {
            Ok(config) => config,
            Err(message) => {
                self.say(message);
                return;
            }
        };

        // A command, not just a file write: the engine keeps running the
        // profile it started with when the file changes underneath it.
        // MODE_KEEP, because none of this says anything about who should be
        // driving the fan.
        if let Some(channel) = crate::ipc::Channel::attach() {
            channel.post_command(crate::ipc::MODE_KEEP, 0, &config.active_profile);
        }

        // Show whatever is active now without waiting for a sample, and point
        // the editor at it, so that saving cannot land on the profile this
        // just renamed or removed.
        if let Ok(curve) = config.active_curve() {
            let live = self.editor.live_temp();
            self.editor = Editor::new(&curve);
            self.editor.set_live_temp(live);
        }

        self.editing = config.active_profile.clone();
        self.config = config;
        self.invalidate();
    }

    /// Brings a curve over from a TPFanControl ini.
    ///
    /// Called from inside [`Settings::show_profile_menu`], which is holding the
    /// modal pause the file dialog and the name box both need.
    fn import_profile(&mut self) {
        // What is on screen goes to disk first: the import switches to the new
        // profile, and edits made under the old one would go with it.
        if self.editor.is_dirty() && self.apply().is_err() {
            return;
        }

        let Some(result) = crate::import::run(self.hwnd()) else { return };

        let outcome = match result {
            Ok(outcome) => outcome,
            Err(message) => {
                self.say(&message);
                return;
            }
        };

        if let Some(channel) = crate::ipc::Channel::attach() {
            channel.post_command(crate::ipc::MODE_KEEP, 0, &outcome.config.active_profile);
        }

        if let Ok(curve) = outcome.config.active_curve() {
            let live = self.editor.live_temp();
            self.editor = Editor::new(&curve);
            self.editor.set_live_temp(live);
        }

        self.editing = outcome.config.active_profile.clone();
        self.config = outcome.config;
        self.invalidate();

        self.say(&outcome.summary);
    }

    /// The profile that is selected, which is what the settings file says.
    ///
    /// Not what the engine last published. Both the tray and this window write
    /// their choice to the file before announcing it, and the engine picks it
    /// up within a poll, so preferring the engine's answer made the window
    /// disagree with itself for a few seconds after every switch and flip the
    /// curve on screen back and forth. The readout panel still shows the
    /// engine's answer, because that panel reports what is happening, not what
    /// has been chosen.
    fn live_profile(&self) -> String {
        self.config.active_profile.clone()
    }

    /// Throws the edits away and goes back to what is stored.
    ///
    /// Read from the file, not from anything remembered, so this lands on
    /// exactly what the engine is running, and it is cheap enough to do on a
    /// key press. Doing nothing when the editor is clean keeps Escape from
    /// reloading a curve somebody is in the middle of looking at.
    fn discard_edits(&mut self) {
        if !self.editor.is_dirty() {
            return;
        }

        let path = yamato_core::Config::default_path();
        let Ok(config) = yamato_core::Config::load(&path) else { return };

        let stored = config
            .profiles
            .iter()
            .find(|p| p.name == self.editing)
            .map(|p| p.to_curve())
            .unwrap_or_else(|| config.active_curve());

        if let Ok(curve) = stored {
            let live = self.editor.live_temp();
            self.editor = Editor::new(&curve);
            self.editor.set_live_temp(live);
            self.invalidate();
        }
    }

    /// Writes the edited curve to the active profile and saves.
    ///
    /// Without it the editor was a viewer: it let you drag points, said
    /// "Unsaved changes" and then discarded them when the window closed. The
    /// engine notices the file changing and reloads without a restart.
    fn apply(&mut self) -> std::result::Result<(), String> {
        let curve = self.editor.validate().map_err(|e| e.to_string())?;

        let path = yamato_core::Config::default_path();
        let mut config = yamato_core::Config::load(&path).map_err(|e| e.to_string())?;

        // The profile this curve came from, not whichever one the file calls
        // active now. Something else may have changed that while the window
        // was open, and writing the curve on screen into it would look like an
        // ordinary save while destroying another profile.
        let active = self.editing.clone();
        match config.profiles.iter_mut().find(|p| p.name == active) {
            Some(profile) => *profile = yamato_core::Profile::new(&active, &curve),
            // The profile went away underneath us, so keep the work instead of
            // throwing it back at the user.
            None => config.profiles.push(yamato_core::Profile::new(&active, &curve)),
        }

        config.save(&path).map_err(|e| e.to_string())?;
        self.editor.mark_saved();

        Ok(())
    }

    /// Writes the curve out if it has changed since it was last saved.
    ///
    /// Closing the window does this for itself; anything else that ends the
    /// program while it is open has to do it too, or the two ways out disagree
    /// about whether your work is kept.
    pub fn save_if_dirty(&mut self) {
        if self.editor.is_dirty() {
            let _ = self.apply();
        }
    }

    /// Keeps the editor pointed at the profile that is actually selected.
    ///
    /// The tray can switch, rename or delete profiles while this window is
    /// open. Left alone, the window went on showing the curve it loaded while
    /// naming the new profile, and the next save wrote one into the other.
    fn follow_selected_profile(&mut self) {
        let selected = self.live_profile();

        if selected.is_empty() || selected == self.editing {
            return;
        }

        let Ok(config) = yamato_core::Config::load(&yamato_core::Config::default_path()) else {
            return;
        };

        // What is on screen belongs to the profile it came from, so it goes
        // back there before moving on: a switch made somewhere else is not a
        // reason to throw away someone's dragging. Only if that profile still
        // exists, though. One deleted from the tray must not come back because
        // this window happened to be open with an edit in it.
        if self.editor.is_dirty() && config.profiles.iter().any(|p| p.name == self.editing) {
            let _ = self.apply();
        }

        let Some(profile) = config.profiles.iter().find(|p| p.name == selected) else {
            return;
        };
        let Ok(curve) = profile.to_curve() else { return };

        let live = self.editor.live_temp();
        self.editor = Editor::new(&curve);
        self.editor.set_live_temp(live);
        self.editing = selected;
    }

    /// Feeds in the engine's latest sample. Drives both the numbers beside the
    /// graph and the "now" marker drawn on it.
    pub fn set_readout(&mut self, readout: Readout) {
        self.editor.set_live_temp(readout.hottest.map(|(_, t)| t));
        self.readout = readout;
        self.settle_pending();
        // Picked up here because this is the one thing that happens on every
        // tick, so choosing Fahrenheit in the tray shows up in an open window
        // within a sample instead of not at all.
        self.config = settings_on_disk();

        // A pointer can leave the window without the window hearing about it,
        // which would leave the picker lit for as long as it stayed away. This
        // is the sample that puts it right.
        self.picker_hot.set(self.pointer_over(self.picker_box.get()));

        // And the profile may have been switched from the tray meanwhile.
        self.follow_selected_profile();

        self.invalidate();
    }

    /// A temperature the way the user asked to see it, unit and all.
    ///
    /// Takes Celsius because everything upstream of the screen is in Celsius:
    /// the reading, the thermal bands, and the curve. Only what is drawn
    /// changes.
    fn temp_text(&self, celsius: i8) -> String {
        format!(
            "{}{}",
            yamato_core::display_temp(celsius, self.config.fahrenheit),
            yamato_core::unit_suffix(self.config.fahrenheit)
        )
    }

    /// Device pixels per DIP for the display this window is on.
    fn scale(&self) -> f32 {
        let dpi = unsafe { GetDpiForWindow(self.window) };

        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    }

    /// Client size in DIPs, for anything that spans the whole window.
    fn client_size(&self) -> (f32, f32) {
        let mut rc = RECT::default();
        unsafe { let _ = GetClientRect(self.window, &mut rc); };

        let scale = self.scale();

        (rc.right as f32 / scale, rc.bottom as f32 / scale)
    }

    /// Layout happens in DIPs. Direct2D is told the real DPI, so it renders at
    /// full resolution and everything here stays in one coordinate system
    /// regardless of the display.
    fn canvas(&self) -> ERect {
        let (width, height) = self.client_size();

        // Both edges are what is left after something fixed, so both are
        // floored: a window narrower or shorter than the minimum would
        // otherwise produce a rectangle with a negative width, and every
        // coordinate derived from it is then nonsense, not merely cramped.
        let left = theme::AXIS_GUTTER;

        ERect {
            left,
            top: CANVAS_TOP,
            right: (width - READOUT_WIDTH - TUNING_WIDTH - theme::PADDING * 2.0 - theme::SPACE_MD)
                .max(left + 120.0),
            bottom: (height - CANVAS_BOTTOM_GAP).max(CANVAS_TOP + 80.0),
        }
    }

    /// Grows the window to its own minimum, if it somehow started under it.
    fn enforce_minimum(&self) {
        let (width, height) = self.client_size();

        if width >= MIN_WIDTH && height >= MIN_HEIGHT {
            return;
        }

        let mut outer = RECT::default();
        if unsafe { GetWindowRect(self.window, &mut outer) }.is_err() {
            return;
        }

        // The frame is the difference between what the window is and what can
        // be drawn in: asking for a client-sized window would leave it short by
        // exactly that much, every time.
        let scale = self.scale();
        let frame_width = (outer.right - outer.left) as f32 - width * scale;
        let frame_height = (outer.bottom - outer.top) as f32 - height * scale;

        unsafe {
            let _ = SetWindowPos(
                self.window,
                None,
                0,
                0,
                (MIN_WIDTH.max(width) * scale + frame_width) as i32,
                (MIN_HEIGHT.max(height) * scale + frame_height) as i32,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Left and right of the profile picker, which gives way to the heading
    /// beside it instead of crowding it.
    fn picker_column(&self, area: ERect) -> (f32, f32) {
        let right = area.right + PANEL_BLEED;
        let heading_ends = area.left - PANEL_BLEED + TITLE_WIDTH + theme::SPACE_MD;
        let width = (right - heading_ends).clamp(PICKER_MIN_WIDTH, PICKER_WIDTH);

        (right - width, right)
    }

    /// Left and right edges of the live readings column.
    ///
    /// The three columns are laid out from one place so the panels, the rows
    /// inside them and the hit tests cannot drift apart.
    fn readout_column(&self, area: ERect) -> (f32, f32) {
        let left = area.right + theme::PADDING;

        (left, left + READOUT_WIDTH)
    }

    /// Left and right edges of the settings column beyond it.
    fn tuning_column(&self, area: ERect) -> (f32, f32) {
        let left = self.readout_column(area).1 + theme::SPACE_MD;

        (left, left + TUNING_WIDTH)
    }

    fn ensure_target(&mut self) -> Result<()> {
        // A WM_SIZE that arrived while something modal was open went to
        // DefWindowProc, so the target's size cannot be trusted from messages
        // alone. Checked against the window every paint instead.
        if let Some(target) = &self.target {
            let mut rc = RECT::default();
            unsafe { GetClientRect(self.window, &mut rc)? };
            let actual = unsafe { target.GetPixelSize() };
            if actual.width == (rc.right - rc.left).max(1) as u32
                && actual.height == (rc.bottom - rc.top).max(1) as u32
            {
                // Size is only half of what the window tells the target. A
                // move to a monitor at another scaling arrives as a message
                // too and leaves the pixel count alone, so the check above
                // would call a target drawing at the wrong scale healthy.
                // Re-asserted, not compared: a freshly computed float against
                // the stored one risks a rounding mismatch that would rebuild
                // on every paint.
                let dpi = self.scale() * 96.0;
                unsafe { target.SetDpi(dpi, dpi) };
                return Ok(());
            }
            self.target = None;
        }

        let mut rc = RECT::default();
        unsafe { GetClientRect(self.window, &mut rc)? };

        let size = D2D_SIZE_U {
            width: (rc.right - rc.left).max(1) as u32,
            height: (rc.bottom - rc.top).max(1) as u32,
        };

        // Software rasterizing, deliberately, and this is the single largest
        // thing this program does for its memory footprint.
        //
        // The default asks for a hardware target, which creates a D3D device,
        // which loads the graphics vendor's user-mode driver into this
        // process and keeps it there. Measured on the Intel stack: a tray
        // holding 2 MB of private memory goes to 39 MB the first time this
        // window opens, and nothing gives it back. Releasing the target does
        // not; the device belongs to the factory. Releasing the factory too
        // returns about a third of it. The rest is the driver's own
        // initialization, and only ending the process frees that.
        //
        // Software costs 6.8 MB against 39.5 MB, and it is not slower here:
        // 12.6 ms a frame against 15.9 ms for eight hundred primitives, since
        // this is a panel and a line graph that repaints about once a second,
        // not a scene. There is no GPU work to be worth a GPU's setup.
        let target = unsafe {
            self.factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    ..Default::default()
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd: self.window,
                    pixelSize: size,
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )?
        };

        // Physical pixels for the buffer, real DPI for the coordinate system.
        // Leaving this at the default 96 is what makes a "DPI aware" app still
        // look soft: it renders small and Windows scales it up.
        let dpi = self.scale() * 96.0;
        unsafe { target.SetDpi(dpi, dpi) };

        self.target = Some(target);

        Ok(())
    }

    fn paint(&mut self) -> Result<()> {
        self.ensure_target()?;
        let Some(target) = self.target.clone() else { return Ok(()) };

        let area = self.canvas();

        unsafe {
            target.BeginDraw();
            target.Clear(Some(&theme::BACKGROUND));

            self.draw_window_ground(&target)?;
            self.draw_heading(&target, area)?;
            self.draw_picker(&target, area)?;
            self.draw_canvas_ground(&target, area)?;
            self.draw_grid(&target, area)?;
            self.draw_curve(&target, area)?;
            self.draw_points(&target, area)?;
            self.draw_live_marker(&target, area)?;
            self.draw_footer(&target, area)?;
            self.draw_readout(&target, area)?;
            self.draw_tuning(&target, area)?;
            self.draw_actions(&target, area)?;

            // A lost device means the GPU reset under us. Dropping the target
            // makes the next paint rebuild it instead of failing forever.
            if target.EndDraw(None, None).is_err() {
                self.target = None;
            }
        }

        Ok(())
    }

    fn brush(&self, target: &ID2D1HwndRenderTarget, color: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
        unsafe { target.CreateSolidColorBrush(&color, None) }
    }

    /// A vertical fade of one color down the canvas, for the region under a
    /// curve step. Built per call; brushes are cheap and the canvas height
    /// changes with the window.
    fn fade_brush(
        &self,
        target: &ID2D1HwndRenderTarget,
        area: ERect,
        color: D2D1_COLOR_F,
    ) -> Result<ID2D1LinearGradientBrush> {
        // Three stops, not two: the middle one pulls the fade in early so the
        // fill whispers instead of pooling at the bottom.
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: D2D1_COLOR_F { a: 0.20, ..color } },
            D2D1_GRADIENT_STOP { position: 0.55, color: D2D1_COLOR_F { a: 0.05, ..color } },
            D2D1_GRADIENT_STOP { position: 1.0, color: D2D1_COLOR_F { a: 0.0, ..color } },
        ];

        unsafe {
            let collection = target.CreateGradientStopCollection(
                &stops,
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            )?;

            target.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: D2D_POINT_2F { x: 0.0, y: area.top },
                    endPoint: D2D_POINT_2F { x: 0.0, y: area.bottom },
                },
                None,
                &collection,
            )
        }
    }

    fn text(
        &self,
        target: &ID2D1HwndRenderTarget,
        s: &str,
        rect: D2D_RECT_F,
        format: &IDWriteTextFormat,
        color: D2D1_COLOR_F,
    ) -> Result<()> {
        let brush = self.brush(target, color)?;
        let wide: Vec<u16> = s.encode_utf16().collect();

        unsafe {
            target.DrawText(
                &wide,
                format,
                &rect,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }

        Ok(())
    }

    /// Letter-spaced capitals for section labels. Spacing needs a text
    /// layout; if the interface is somehow missing the label still draws,
    /// just without the tracking.
    fn section_label(&self, target: &ID2D1HwndRenderTarget, s: &str, x: f32, y: f32) -> Result<()> {
        let wide: Vec<u16> = s.encode_utf16().collect();

        unsafe {
            let layout = self.dwrite.CreateTextLayout(&wide, &self.section, 400.0, 16.0)?;
            if let Ok(spaced) = layout.cast::<IDWriteTextLayout1>() {
                let _ = spaced.SetCharacterSpacing(
                    0.6,
                    0.6,
                    0.0,
                    DWRITE_TEXT_RANGE { startPosition: 0, length: wide.len() as u32 },
                );
            }

            let brush = self.brush(target, theme::TEXT_FAINT)?;
            target.DrawTextLayout(
                D2D_POINT_2F { x, y },
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }

        Ok(())
    }

    /// A hairline between panel sections.
    fn divider(&self, target: &ID2D1HwndRenderTarget, left: f32, right: f32, y: f32) -> Result<()> {
        let brush = self.brush(target, theme::DIVIDER)?;

        unsafe {
            target.DrawLine(
                D2D_POINT_2F { x: left, y },
                D2D_POINT_2F { x: right, y },
                &brush,
                1.0,
                None,
            );
        }

        Ok(())
    }

    /// One key-value row: muted label on the left, right-aligned value.
    /// The hover highlight behind a key-value row that answers a click.
    ///
    /// The same wash the profile picker uses, so the two clickable things in
    /// this panel light up the same way. Drawn before the row rather than
    /// inside `kv_row`, because the rows that are only readings share that
    /// function and must not gain a highlight they cannot honor.
    ///
    /// Inset by a couple of DIPs and rounded, so it reads as the row lighting
    /// up rather than a band drawn across the panel. The bounds are the ones
    /// the click is tested against, so what lights up is what is live.
    fn row_wash(
        &self,
        target: &ID2D1HwndRenderTarget,
        left: f32,
        right: f32,
        y: f32,
        height: f32,
        hot: bool,
    ) -> Result<()> {
        if !hot {
            return Ok(());
        }

        let rounded = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: left - theme::SPACE_SM,
                top: y + 1.0,
                right: right + theme::SPACE_SM,
                bottom: y + height - 1.0,
            },
            radiusX: theme::RADIUS,
            radiusY: theme::RADIUS,
        };

        let wash = self.brush(target, D2D1_COLOR_F { a: 0.10, ..theme::ACCENT })?;
        unsafe { target.FillRoundedRectangle(&rounded, &wash) };

        Ok(())
    }

    fn kv_row(
        &self,
        target: &ID2D1HwndRenderTarget,
        left: f32,
        right: f32,
        y: f32,
        height: f32,
        label: &str,
        val: &str,
        color: D2D1_COLOR_F,
    ) -> Result<()> {
        let rect = D2D_RECT_F { left, top: y, right, bottom: y + height };

        self.text(target, label, rect, &self.body, theme::TEXT_DIM)?;

        // Empty values keep their dash so the column rhythm survives a
        // half-initialized readout.
        let shown = if val.is_empty() { "--" } else { val };
        let color = if val.is_empty() { theme::TEXT_FAINT } else { color };

        // The value rect starts past the label so a long value clips instead
        // of overprinting it.
        let vrect = D2D_RECT_F { left: left + 56.0, top: y, right, bottom: y + height };

        self.text(target, shown, vrect, &self.value, color)
    }

    /// A faint top-light over the flat ground. Enough to stop the window
    /// reading as one dead slab; cheap enough to redo every frame.
    fn draw_window_ground(&self, target: &ID2D1HwndRenderTarget) -> Result<()> {
        let (width, height) = self.client_size();

        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: theme::GROUND_SHEEN },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: D2D1_COLOR_F { a: 0.0, ..theme::GROUND_SHEEN },
            },
        ];

        unsafe {
            let collection = target.CreateGradientStopCollection(
                &stops,
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            )?;
            let brush = target.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: D2D_POINT_2F { x: 0.0, y: 0.0 },
                    endPoint: D2D_POINT_2F { x: 0.0, y: height },
                },
                None,
                &collection,
            )?;

            target.FillRectangle(
                &D2D_RECT_F { left: 0.0, top: 0.0, right: width, bottom: height },
                &brush,
            );
        }

        Ok(())
    }

    fn draw_heading(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        // The heading hangs off the same left edge as the canvas panel, so
        // the top of the window and the plot agree about where "left" is.
        let left = area.left - PANEL_BLEED;

        // It shares its row with the picker, and the rectangle it is given has
        // to say so. Given 320 regardless, the words ran under the control
        // beside them the moment the window was anything but wide.
        let (picker_left, _) = self.picker_column(area);
        let title_right = (picker_left - theme::SPACE_MD).max(left + TITLE_WIDTH);

        self.text(
            target,
            "Fan curve",
            D2D_RECT_F { left, top: 18.0, right: title_right, bottom: 46.0 },
            &self.title,
            theme::TEXT,
        )?;

        let (hint, color, dot) = match self.editor.validate() {
            Ok(_) if self.editor.is_dirty() => {
                ("Unsaved changes".to_string(), theme::TEXT_DIM, true)
            }
            Ok(_) => (
                "Drag to move, double-click to add, right-click to remove".to_string(),
                theme::TEXT_FAINT,
                false,
            ),
            Err(e) => (format!("{e}"), theme::ACCENT_BRIGHT, false),
        };

        let mut text_left = left;
        if dot {
            // A small accent dot, the quietest possible "you have not saved".
            let brush = self.brush(target, theme::ACCENT_BRIGHT)?;
            unsafe {
                target.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: D2D_POINT_2F { x: left + 3.0, y: 57.0 },
                        radiusX: 3.0,
                        radiusY: 3.0,
                    },
                    &brush,
                );
            }
            text_left += theme::SPACE_MD;
        }

        // One line, trimmed if it will not fit. It sits just below the picker,
        // and a caption allowed to wrap would grow down the window into the
        // panel underneath instead of admitting it was too long.
        self.text(
            target,
            &hint,
            D2D_RECT_F { left: text_left, top: 50.0, right: area.right, bottom: 68.0 },
            &self.hint,
            color,
        )
    }

    /// The profile picker, above the right-hand end of the graph.
    ///
    /// There has been a way to switch profiles from this window since it had a
    /// readout, but it was a line of text in a column of other lines of text
    /// and read as a label, not a control. The first person to go looking for
    /// a profile picker on the curve pane concluded there was not one. Same
    /// behavior, drawn as what it is: a box with the name and a chevron in it,
    /// sitting where the thing it changes is.
    fn draw_picker(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        let (left, right) = self.picker_column(area);
        let top = PICKER_TOP;
        let bottom = top + PICKER_HEIGHT;

        self.picker_box.set((left, top, right, bottom));

        let rect = D2D_RECT_F { left, top, right, bottom };
        let rounded = D2D1_ROUNDED_RECT { rect, radiusX: theme::RADIUS, radiusY: theme::RADIUS };

        let hot = self.picker_hot.get();
        let fill = self.brush(target, theme::SURFACE)?;
        let edge = self.brush(
            target,
            if hot { D2D1_COLOR_F { a: 0.55, ..theme::ACCENT_BRIGHT } } else { theme::BORDER },
        )?;

        unsafe {
            target.FillRoundedRectangle(&rounded, &fill);

            // A wash, not a second color, so hovering brightens the control it
            // is already looking at instead of introducing a new one.
            if hot {
                let wash = self.brush(target, D2D1_COLOR_F { a: 0.10, ..theme::ACCENT })?;
                target.FillRoundedRectangle(&rounded, &wash);
            }

            target.DrawRoundedRectangle(&rounded, &edge, 1.0, None);
        }

        // Label and value both inside the box, laid out like the key-value rows
        // elsewhere. The label used to hang off the left-hand side, where a
        // narrow window drove it straight through the heading.
        self.text(
            target,
            "Profile",
            D2D_RECT_F { left: left + theme::SPACE_MD, top, right: right - theme::SPACE_XXL, bottom },
            &self.body,
            theme::TEXT_DIM,
        )?;

        // Trailing-aligned and elidable: a long name loses its tail instead of
        // reaching back over the word Profile.
        self.text(
            target,
            &self.live_profile(),
            D2D_RECT_F {
                left: left + 48.0,
                top,
                right: right - theme::SPACE_XXL,
                bottom,
            },
            &self.picker,
            theme::TEXT,
        )?;

        // The chevron: two strokes, which is all a "there is a list behind
        // this" mark needs to be.
        let ink = self.brush(target, theme::ACCENT_BRIGHT)?;
        let cx = right - theme::SPACE_LG;
        let cy = (top + bottom) / 2.0;

        unsafe {
            target.DrawLine(
                D2D_POINT_2F { x: cx - 4.0, y: cy - 2.0 },
                D2D_POINT_2F { x: cx, y: cy + 2.5 },
                &ink,
                1.6,
                &self.round_stroke,
            );
            target.DrawLine(
                D2D_POINT_2F { x: cx, y: cy + 2.5 },
                D2D_POINT_2F { x: cx + 4.0, y: cy - 2.0 },
                &ink,
                1.6,
                &self.round_stroke,
            );
        }

        Ok(())
    }

    /// Whether the pointer is inside a rectangle given in DIPs.
    fn pointer_over(&self, (left, top, right, bottom): (f32, f32, f32, f32)) -> bool {
        let mut point = POINT::default();

        unsafe {
            if GetCursorPos(&mut point).is_err() {
                return false;
            }
            if !windows::Win32::Graphics::Gdi::ScreenToClient(self.window, &mut point).as_bool() {
                return false;
            }
        }

        let scale = self.scale();
        let (x, y) = (point.x as f32 / scale, point.y as f32 / scale);

        x >= left && x <= right && y >= top && y <= bottom
    }

    /// A message box, for the handful of things a profile edit can refuse.
    fn say(&self, text: &str) {
        unsafe {
            MessageBoxW(
                self.window,
                &windows::core::HSTRING::from(text),
                w!("Yamato"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    fn draw_canvas_ground(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        let rect = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: area.left - PANEL_BLEED,
                top: area.top - PANEL_BLEED,
                right: area.right + PANEL_BLEED,
                bottom: area.bottom + PANEL_BLEED,
            },
            radiusX: theme::RADIUS,
            radiusY: theme::RADIUS,
        };

        let fill = self.brush(target, theme::SURFACE)?;
        let edge = self.brush(target, theme::BORDER)?;

        unsafe {
            target.FillRoundedRectangle(&rect, &fill);
            // A 1px lit edge does more for depth than any shadow would, and
            // costs nothing.
            target.DrawRoundedRectangle(&rect, &edge, 1.0, None);
        }

        Ok(())
    }

    fn draw_grid(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        let grid = self.brush(target, theme::GRID)?;
        let strong = self.brush(target, theme::GRID_STRONG)?;

        // Fan levels across, one line each. The baseline is a touch stronger
        // because the eye wants a floor; the BIOS handoff is dashed because
        // it is a boundary, not a speed.
        for slot in 0..=(theme::LEVEL_MAX as i32) {
            let y = self.editor.level_to_y(area, slot as f32);
            let bios = slot > yamato_ec::FAN_LEVEL_MAX as i32;

            unsafe {
                if bios {
                    target.DrawLine(
                        D2D_POINT_2F { x: area.left, y },
                        D2D_POINT_2F { x: area.right, y },
                        &strong,
                        1.0,
                        &self.dash_stroke,
                    );
                } else {
                    let pen = if slot == 0 { &strong } else { &grid };
                    target.DrawLine(
                        D2D_POINT_2F { x: area.left, y },
                        D2D_POINT_2F { x: area.right, y },
                        pen,
                        1.0,
                        None,
                    );
                }
            }

            let caption = if bios { "BIOS".to_string() } else { slot.to_string() };
            let color = if bios { theme::TEXT_DIM } else { theme::TEXT_FAINT };

            self.text(
                target,
                &caption,
                D2D_RECT_F {
                    left: theme::SPACE_XS,
                    top: y - 10.0,
                    right: area.left - PANEL_BLEED - theme::SPACE_SM,
                    bottom: y + 10.0,
                },
                &self.axis_level,
                color,
            )?;
        }

        // Temperature up the bottom, every ten degrees. Lines only in the
        // interior; the panel border already draws the two edges.
        let mut temp = theme::TEMP_MIN;
        while temp <= theme::TEMP_MAX {
            let x = self.editor.temp_to_x(area, temp);

            if temp > theme::TEMP_MIN && temp < theme::TEMP_MAX {
                unsafe {
                    target.DrawLine(
                        D2D_POINT_2F { x, y: area.top },
                        D2D_POINT_2F { x, y: area.bottom },
                        &grid,
                        1.0,
                        None,
                    );
                }
            }

            self.text(
                target,
                &format!("{temp:.0}\u{00b0}"),
                D2D_RECT_F {
                    left: x - 24.0,
                    top: area.bottom + PANEL_BLEED + 6.0,
                    right: x + 24.0,
                    bottom: area.bottom + PANEL_BLEED + 22.0,
                },
                &self.axis,
                theme::TEXT_FAINT,
            )?;

            temp += 10.0;
        }

        // A slim thermal strip under the baseline: the legend for the band
        // colors without a legend box. Stops sit at the real thresholds so the
        // strip and the tray icon tell the same story.
        let span = theme::TEMP_MAX - theme::TEMP_MIN;
        let warm_pos = (theme::WARM_AT - theme::TEMP_MIN) / span;
        let hot_pos = (theme::HOT_AT - theme::TEMP_MIN) / span;
        let strip_alpha = 0.65;
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: D2D1_COLOR_F { a: strip_alpha, ..theme::COOL } },
            D2D1_GRADIENT_STOP { position: warm_pos - 0.06, color: D2D1_COLOR_F { a: strip_alpha, ..theme::COOL } },
            D2D1_GRADIENT_STOP { position: warm_pos + 0.04, color: D2D1_COLOR_F { a: strip_alpha, ..theme::WARM } },
            D2D1_GRADIENT_STOP { position: hot_pos - 0.05, color: D2D1_COLOR_F { a: strip_alpha, ..theme::WARM } },
            D2D1_GRADIENT_STOP { position: hot_pos + 0.05, color: D2D1_COLOR_F { a: strip_alpha, ..theme::HOT } },
            D2D1_GRADIENT_STOP { position: 1.0, color: D2D1_COLOR_F { a: strip_alpha, ..theme::HOT } },
        ];

        unsafe {
            let collection = target.CreateGradientStopCollection(
                &stops,
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            )?;
            let brush = target.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: D2D_POINT_2F { x: area.left, y: 0.0 },
                    endPoint: D2D_POINT_2F { x: area.right, y: 0.0 },
                },
                None,
                &collection,
            )?;

            target.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: area.left,
                        top: area.bottom + 5.0,
                        right: area.right,
                        bottom: area.bottom + 8.0,
                    },
                    radiusX: 1.5,
                    radiusY: 1.5,
                },
                &brush,
            );
        }

        Ok(())
    }

    /// The curve itself: a soft fill underneath, a glow, then the line on
    /// top, each segment tinted by the thermal band it sits in.
    fn draw_curve(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        let points = self.editor.points();
        if points.is_empty() {
            // An empty canvas that says so, instead of a blank panel that
            // looks like the paint code failed.
            let mid = (area.top + area.bottom) / 2.0;
            self.text(
                target,
                "No curve points",
                D2D_RECT_F { left: area.left, top: mid - 22.0, right: area.right, bottom: mid - 4.0 },
                &self.axis,
                theme::TEXT_DIM,
            )?;
            return self.text(
                target,
                "Double-click the graph to add one",
                D2D_RECT_F { left: area.left, top: mid + 2.0, right: area.right, bottom: mid + 20.0 },
                &self.axis,
                theme::TEXT_FAINT,
            );
        }

        // Geometry of each horizontal run: where it starts, where it ends,
        // and the height of its level. A fan level holds until the next
        // point, so the shape is steps, not a ramp; drawing a ramp would
        // misrepresent what the engine actually does.
        let run = |i: usize| -> (f32, f32, f32) {
            let p = points[i];
            let x0 = self.editor.temp_to_x(area, p.temp as f32);
            let x1 = points
                .get(i + 1)
                .map(|n| self.editor.temp_to_x(area, n.temp as f32))
                .unwrap_or(area.right);
            let y = self.editor.level_to_y(area, axis_for_level(p.level));

            (x0, x1, y)
        };

        // Under everything else: the temperatures over which each step holds
        // instead of changing. This is what per-point hysteresis does to the
        // machine, and until it was drawn the only way to know one point was
        // twitchy and its neighbor patient was to open the settings file.
        //
        // Left of a riser is the room the higher step keeps while cooling;
        // right of it is the wait before the fan climbs. They are different
        // sizes on purpose, and seeing that is the point.
        unsafe {
            for i in 1..points.len() {
                let point = points[i];
                let x = self.editor.temp_to_x(area, point.temp as f32);
                let top = self.editor.level_to_y(area, axis_for_level(point.level));
                let bottom = self.editor.level_to_y(area, axis_for_level(points[i - 1].level));

                if point.hyst_down > 0 {
                    let from = self
                        .editor
                        .temp_to_x(area, (point.temp - point.hyst_down) as f32);
                    let brush = self.brush(
                        target,
                        D2D1_COLOR_F { a: 0.14, ..theme::band(point.temp as f32) },
                    )?;

                    target.FillRectangle(
                        &D2D_RECT_F { left: from, top, right: x, bottom },
                        &brush,
                    );
                }

                if point.hyst_up > 0 {
                    let to = self
                        .editor
                        .temp_to_x(area, (point.temp as i32 + point.hyst_up as i32) as f32);
                    let brush =
                        self.brush(target, D2D1_COLOR_F { a: 0.10, ..theme::ACCENT_BRIGHT })?;

                    target.FillRectangle(&D2D_RECT_F { left: x, top, right: to, bottom }, &brush);
                }
            }
        }

        unsafe {
            // Pass one, the fill: each step fades downward in its own band
            // color, so the region reads as "how hard, and how hot" at once.
            for i in 0..points.len() {
                let (x0, x1, y) = run(i);
                let fill = self.fade_brush(target, area, theme::band(points[i].temp as f32))?;

                target.FillRectangle(
                    &D2D_RECT_F { left: x0, top: y, right: x1, bottom: area.bottom },
                    &fill,
                );
            }

            // Pass two, a wide low-alpha stroke under the line. That is the
            // "glow": no blur, no effects, just a soft echo.
            for i in 0..points.len() {
                let (x0, x1, y) = run(i);
                let glow = self.brush(
                    target,
                    D2D1_COLOR_F { a: 0.10, ..theme::band(points[i].temp as f32) },
                )?;

                target.DrawLine(
                    D2D_POINT_2F { x: x0, y },
                    D2D_POINT_2F { x: x1, y },
                    &glow,
                    7.0,
                    &self.round_stroke,
                );

                if let Some(next) = points.get(i + 1) {
                    let ny = self.editor.level_to_y(area, axis_for_level(next.level));
                    target.DrawLine(
                        D2D_POINT_2F { x: x1, y },
                        D2D_POINT_2F { x: x1, y: ny },
                        &glow,
                        7.0,
                        &self.round_stroke,
                    );
                }
            }

            // Pass three, the crisp line. Round caps so runs and risers meet
            // in a clean corner instead of a notch.
            for i in 0..points.len() {
                let (x0, x1, y) = run(i);
                let brush = self.brush(target, theme::band(points[i].temp as f32))?;

                target.DrawLine(
                    D2D_POINT_2F { x: x0, y },
                    D2D_POINT_2F { x: x1, y },
                    &brush,
                    2.5,
                    &self.round_stroke,
                );

                if let Some(next) = points.get(i + 1) {
                    let ny = self.editor.level_to_y(area, axis_for_level(next.level));
                    target.DrawLine(
                        D2D_POINT_2F { x: x1, y },
                        D2D_POINT_2F { x: x1, y: ny },
                        &brush,
                        2.5,
                        &self.round_stroke,
                    );
                }
            }
        }

        Ok(())
    }

    fn draw_points(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        let ring = self.brush(target, theme::SURFACE)?;
        let chosen = self.editor.selected();

        for (i, p) in self.editor.points().iter().enumerate() {
            let x = self.editor.temp_to_x(area, p.temp as f32);
            let y = self.editor.level_to_y(area, axis_for_level(p.level));
            let color = theme::band(p.temp as f32);
            let fill = self.brush(target, color)?;
            let halo = self.brush(target, D2D1_COLOR_F { a: 0.15, ..color })?;

            // The chosen one wears a ring, because the rows beside the graph
            // are about it and there has to be no doubt which it is.
            if chosen == Some(i) {
                let mark = self.brush(target, theme::ACCENT_BRIGHT)?;
                unsafe {
                    target.DrawEllipse(
                        &D2D1_ELLIPSE {
                            point: D2D_POINT_2F { x, y },
                            radiusX: theme::POINT_RADIUS + 6.0,
                            radiusY: theme::POINT_RADIUS + 6.0,
                        },
                        &mark,
                        1.6,
                        None,
                    );
                }
            }

            unsafe {
                // Halo first, then a ring punched in the surface color so the
                // point stays legible where the curve passes behind it, then
                // the point itself.
                target.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: D2D_POINT_2F { x, y },
                        radiusX: theme::POINT_RADIUS + 5.0,
                        radiusY: theme::POINT_RADIUS + 5.0,
                    },
                    &halo,
                );
                target.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: D2D_POINT_2F { x, y },
                        radiusX: theme::POINT_RADIUS + 2.5,
                        radiusY: theme::POINT_RADIUS + 2.5,
                    },
                    &ring,
                );
                target.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: D2D_POINT_2F { x, y },
                        radiusX: theme::POINT_RADIUS,
                        radiusY: theme::POINT_RADIUS,
                    },
                    &fill,
                );
            }
        }

        Ok(())
    }

    /// Where the machine actually is right now, so the curve is not abstract.
    /// Dashed, not solid: it is a reference, not part of the data, and the
    /// dashes keep it from fighting the curve.
    fn draw_live_marker(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        let Some(temp) = self.editor.live_temp() else { return Ok(()) };

        let x = self.editor.temp_to_x(area, temp as f32).clamp(area.left, area.right);
        let line = self.brush(target, D2D1_COLOR_F { a: 0.55, ..theme::ACCENT_BRIGHT })?;

        unsafe {
            target.DrawLine(
                D2D_POINT_2F { x, y: area.top },
                D2D_POINT_2F { x, y: area.bottom },
                &line,
                1.2,
                &self.dash_stroke,
            );
        }

        // A small chip on the marker instead of loose text: measured so the
        // pill hugs the label whatever the number is. Only the label follows
        // the unit setting; the marker's position above is the Celsius reading
        // placed on a Celsius axis.
        let label = format!("now {}", self.temp_text(temp));
        let wide: Vec<u16> = label.encode_utf16().collect();
        let height = 22.0;

        unsafe {
            let layout = self.dwrite.CreateTextLayout(&wide, &self.chip, 160.0, height)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            let width = metrics.width + theme::SPACE_SM * 2.0 + 2.0;

            // Center on the marker, but never let the chip poke outside the
            // plot when "now" sits near an edge.
            let cx = x.clamp(
                area.left + width / 2.0 + theme::SPACE_XS,
                area.right - width / 2.0 - theme::SPACE_XS,
            );
            let rect = D2D_RECT_F {
                left: cx - width / 2.0,
                top: area.top + theme::SPACE_SM,
                right: cx + width / 2.0,
                bottom: area.top + theme::SPACE_SM + height,
            };
            let pill = D2D1_ROUNDED_RECT { rect, radiusX: height / 2.0, radiusY: height / 2.0 };

            // Opaque ground first so the dashed line cannot ghost through
            // the translucent accent wash.
            let ground = self.brush(target, theme::SURFACE)?;
            let wash = self.brush(target, D2D1_COLOR_F { a: 0.16, ..theme::ACCENT })?;
            let edge = self.brush(target, D2D1_COLOR_F { a: 0.35, ..theme::ACCENT_BRIGHT })?;
            let ink = self.brush(target, theme::ACCENT_BRIGHT)?;

            target.FillRoundedRectangle(&pill, &ground);
            target.FillRoundedRectangle(&pill, &wash);
            target.DrawRoundedRectangle(&pill, &edge, 1.0, None);
            target.DrawTextLayout(
                D2D_POINT_2F { x: rect.left + theme::SPACE_SM + 1.0, y: rect.top },
                &layout,
                &ink,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }

        Ok(())
    }

    fn draw_footer(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        // Stops short of the Save button instead of running under it, and is
        // left out when what remains is too narrow to read a sentence in: a
        // paragraph squeezed into eight characters a line is a mess where an
        // explanation was.
        //
        // Both buttons, when there are two. Reserving room for one meant the
        // sentence ran under Discard the moment anything was edited, which is
        // exactly when somebody is looking at that part of the window.
        let buttons = if self.editor.is_dirty() {
            SAVE_WIDTH * 2.0 + theme::SPACE_SM
        } else {
            SAVE_WIDTH
        };

        let right = area.right + PANEL_BLEED - buttons - theme::SPACE_MD;

        if right - area.left < 220.0 {
            return Ok(());
        }

        self.text(
            target,
            "Levels 0 to 7 are fan speeds. BIOS hands the fan back to the firmware, \
             which is the sane thing to do at the top of a curve.",
            D2D_RECT_F {
                left: area.left,
                top: area.bottom + theme::SPACE_XXL + theme::SPACE_MD,
                right,
                bottom: area.bottom + CANVAS_BOTTOM_GAP - theme::SPACE_SM,
            },
            &self.caption,
            theme::TEXT_FAINT,
        )
    }

    /// The Save button, and the keyboard paths that do the same thing.
    ///
    /// The window has saved on Ctrl+S, on Enter and on close since it could
    /// save at all, but none of that was on screen, so a curve dragged into
    /// shape looked like work the program had no way to keep.
    fn draw_actions(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        // Hung off the same edge as the panel above it and the picker above
        // that, so the right-hand side of the window is one line.
        // Discard sits on the right of the pair and Save to its left, so the
        // row still ends where the panels above it end and nothing reaches
        // further out over the column beside it.
        let edge = area.right + PANEL_BLEED;
        let dirty_now = self.editor.is_dirty();

        let (left, right) = if dirty_now {
            let save_right = edge - SAVE_WIDTH - theme::SPACE_SM;
            (save_right - SAVE_WIDTH, save_right)
        } else {
            (edge - SAVE_WIDTH, edge)
        };

        let top = area.bottom + theme::SPACE_XXL + theme::SPACE_SM;
        let bottom = top + SAVE_HEIGHT;

        // Prominent only when there is something to save. A button that looks
        // the same either way says nothing about the state of the work, and
        // this one goes inert once everything is written.
        let dirty = self.editor.is_dirty();
        let (fill, edge, ink) = if dirty {
            (theme::ACCENT, D2D1_COLOR_F { a: 0.55, ..theme::ACCENT_BRIGHT }, theme::TEXT)
        } else {
            (theme::SURFACE, theme::BORDER, theme::TEXT_FAINT)
        };

        let rect = D2D_RECT_F { left, top, right, bottom };
        let rounded = D2D1_ROUNDED_RECT { rect, radiusX: theme::RADIUS, radiusY: theme::RADIUS };

        let fill = self.brush(target, fill)?;
        let edge = self.brush(target, edge)?;

        unsafe {
            target.FillRoundedRectangle(&rounded, &fill);
            target.DrawRoundedRectangle(&rounded, &edge, 1.0, None);
        }

        self.text(target, "Save curve", rect, &self.button, ink)?;
        self.save_button.set((left, top, right, bottom));

        // Discard sits beside it, and only while there is something to
        // discard. An editor you can save but not back out of makes every
        // experiment a commitment, and the curve is the one thing here worth
        // experimenting with.
        if dirty {
            let d_left = right + theme::SPACE_SM;
            let d_right = d_left + SAVE_WIDTH;

            let d_rect = D2D_RECT_F { left: d_left, top, right: d_right, bottom };
            let d_round =
                D2D1_ROUNDED_RECT { rect: d_rect, radiusX: theme::RADIUS, radiusY: theme::RADIUS };

            let d_fill = self.brush(target, theme::SURFACE)?;
            let d_edge = self.brush(target, theme::BORDER)?;

            unsafe {
                target.FillRoundedRectangle(&d_round, &d_fill);
                target.DrawRoundedRectangle(&d_round, &d_edge, 1.0, None);
            }

            self.text(target, "Discard", d_rect, &self.button, theme::TEXT_DIM)?;
            self.discard_button.set((d_left, top, d_right, bottom));
        } else {
            self.discard_button.set((0.0, 0.0, 0.0, 0.0));
        }

        self.text(
            target,
            if dirty {
                "Ctrl+S saves, Escape discards"
            } else {
                "Ctrl+S or Enter saves"
            },
            D2D_RECT_F {
                left,
                top: bottom + theme::SPACE_XS,
                // Across both buttons when there are two, so the line is not
                // measured against one box and drawn under the other.
                right: if dirty { right + theme::SPACE_SM + SAVE_WIDTH } else { right },
                bottom: bottom + theme::SPACE_XL,
            },
            &self.axis,
            theme::TEXT_FAINT,
        )
    }

    /// The settings that are not the curve.
    ///
    /// These were reachable only by hand-editing the file, which is the one
    /// thing this program set out to make unnecessary. They sit in a column of
    /// their own because someone looking for settings opens the window called
    /// settings, and finding only a graph there reads as having none.
    fn draw_tuning(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        let (panel_left, panel_right) = self.tuning_column(area);
        let panel_top = area.top - PANEL_BLEED;
        let panel_bottom = area.bottom + PANEL_BLEED;

        let panel = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: panel_left,
                top: panel_top,
                right: panel_right,
                bottom: panel_bottom,
            },
            radiusX: theme::RADIUS,
            radiusY: theme::RADIUS,
        };

        let fill = self.brush(target, theme::SURFACE)?;
        let edge = self.brush(target, theme::BORDER)?;
        unsafe {
            target.FillRoundedRectangle(&panel, &fill);
            target.DrawRoundedRectangle(&panel, &edge, 1.0, None);
        }

        let cl = panel_left + theme::SPACE_LG;
        let cr = panel_right - theme::SPACE_LG;

        let mut y = panel_top + theme::SPACE_LG;
        self.section_label(target, "SETTINGS", cl, y)?;
        y += 16.0 + theme::SPACE_SM;

        // Accent, like the Profile row, because these are the rows that do
        // something when clicked.
        let height = 24.0;
        let mut rows = self.tuning_rows.get();
        for (row, knob) in rows.iter_mut().zip(KNOBS) {
            self.kv_row(
                target,
                cl,
                cr,
                y,
                height,
                knob.label(),
                &knob.value(&self.config),
                theme::YELLOW,
            )?;

            *row = (cl, y, cr, y + height);
            y += height;
        }
        self.tuning_rows.set(rows);

        y += theme::SPACE_SM;
        self.divider(target, cl, cr, y)?;
        y += theme::SPACE_MD;

        // Both hints in one paragraph. The sensor one has nowhere else to live
        // and the readout column is the only place it applies, so it says
        // where to go instead of sitting next to nothing.
        //
        // Given up to whichever row the pointer is over, because a row that
        // reads "Standby poll  30 s" tells nobody what it is for, and this is
        // the only line of prose within reach of it.
        // The two live rows in the readout column borrow it too. They are the
        // only clickable things in this window with no label saying so, and
        // an accent color and a highlight tell somebody that a row does
        // something without telling them what.
        // Kept to the two lines this box holds.
        let hint = if self.mode_hot.get() {
            "Click Mode for the firmware, your curve, or a level you hold yourself."
        } else if self.level_hot.get() {
            "Click Level to step it, 1 to 7. A held level turns the firmware's own \
             thermal management off."
        } else if self.profile_hot.get() {
            "Click Profile to switch curves, or to add, rename or delete one."
        } else {
            match self.hot_knob.get().and_then(|i| KNOBS.get(i)) {
                Some(knob) => knob.describe(),
                None => {
                    "Click a value to change it. Right-click a sensor to leave it out of the decision."
                }
            }
        };

        self.text(
            target,
            hint,
            D2D_RECT_F { left: cl, top: y, right: cr, bottom: y + 34.0 },
            &self.caption,
            theme::TEXT_FAINT,
        )?;
        y += 38.0;

        self.divider(target, cl, cr, y)?;
        y += theme::SPACE_MD;
        self.section_label(target, "THIS POINT", cl, y)?;
        y += 16.0 + theme::SPACE_SM;

        let mut rows = self.point_rows.get();

        match self.editor.selected_point() {
            Some(point) => {
                // Which point, in the words the graph uses: a temperature and
                // a speed.
                let level = if point.is_bios() {
                    "hands over to the BIOS".to_string()
                } else {
                    format!("runs the fan at {}", point.level)
                };

                self.text(
                    target,
                    &format!("At {}, {level}.", self.temp_text(point.temp)),
                    D2D_RECT_F { left: cl, top: y, right: cr, bottom: y + 32.0 },
                    &self.caption,
                    theme::TEXT_DIM,
                )?;
                y += 34.0;

                for (row, knob) in rows.iter_mut().zip(POINT_KNOBS) {
                    self.kv_row(
                        target,
                        cl,
                        cr,
                        y,
                        height,
                        knob.label(),
                        &knob.value(&point),
                        theme::YELLOW,
                    )?;

                    *row = (cl, y, cr, y + height);
                    y += height;
                }

                y += theme::SPACE_SM;

                self.text(
                    target,
                    "Bigger numbers mean the fan waits longer before changing \
                     speed, so it changes less often. The shaded bands on the \
                     graph are the waiting.",
                    D2D_RECT_F { left: cl, top: y, right: cr, bottom: y + 64.0 },
                    &self.caption,
                    theme::TEXT_FAINT,
                )?;
            }
            None => {
                // Nothing drawn is nothing to click, so the rows go away as
                // well as the numbers.
                rows = [(0.0, 0.0, 0.0, 0.0); POINT_KNOBS.len()];

                self.text(
                    target,
                    "Click a point on the graph to change how quickly the fan \
                     follows it.",
                    D2D_RECT_F { left: cl, top: y, right: cr, bottom: y + 48.0 },
                    &self.caption,
                    theme::TEXT_FAINT,
                )?;
            }
        }

        self.point_rows.set(rows);

        Ok(())
    }

    /// Every sensor the controller reports, the fan speed, and the mode.
    ///
    /// The program this replaces made you open a log file to see these. They
    /// are the reason anyone opens a fan utility, so they are on screen.
    fn draw_readout(&self, target: &ID2D1HwndRenderTarget, area: ERect) -> Result<()> {
        // The panel shares top and bottom edges with the canvas panel, so the
        // two surfaces read as siblings, not coincidences.
        let (panel_left, panel_right) = self.readout_column(area);
        let panel_top = area.top - PANEL_BLEED;
        let panel_bottom = area.bottom + PANEL_BLEED;

        let panel = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: panel_left,
                top: panel_top,
                right: panel_right,
                bottom: panel_bottom,
            },
            radiusX: theme::RADIUS,
            radiusY: theme::RADIUS,
        };

        let fill = self.brush(target, theme::SURFACE)?;
        let edge = self.brush(target, theme::BORDER)?;
        unsafe {
            target.FillRoundedRectangle(&panel, &fill);
            target.DrawRoundedRectangle(&panel, &edge, 1.0, None);
        }

        let cl = panel_left + theme::SPACE_LG;
        let cr = panel_right - theme::SPACE_LG;

        if self.readout.fault {
            // Nothing clickable is drawn in this state, so nothing may stay
            // clickable either. Rectangles left over from the last healthy
            // paint would answer a click in what is now empty panel.
            self.profile_row.set((0.0, 0.0, 0.0, 0.0));
            self.mode_row.set((0.0, 0.0, 0.0, 0.0));
            self.level_row.set((0.0, 0.0, 0.0, 0.0));
            self.sensor_rows.set((0.0, 0.0, 0.0, 0.0));

            // A fault is a state, not a crash. Centered, calm, and it says
            // what happens next instead of shouting in red. One exception: a
            // fan that may be held with the firmware's own management switched
            // off is drawn in the accent, because that one is not calm.
            let (headline, detail) = crate::ipc::status_words(self.readout.status, true)
                .unwrap_or(("Controller unavailable", "Readings return when it responds."));

            let serious = self.readout.status == crate::ipc::STATUS_HANDBACK_FAILED;
            let mid = (panel_top + panel_bottom) / 2.0;

            self.text(
                target,
                headline,
                D2D_RECT_F { left: cl, top: mid - 34.0, right: cr, bottom: mid - 4.0 },
                &self.axis,
                if serious { theme::ACCENT_BRIGHT } else { theme::TEXT_DIM },
            )?;
            self.text(
                target,
                detail,
                D2D_RECT_F { left: cl, top: mid + 2.0, right: cr, bottom: mid + 44.0 },
                &self.axis,
                theme::TEXT_FAINT,
            )?;

            // Under the detail, not instead of it, which is the luxury the
            // tooltip does not have. Drawn in the accent because unlike the
            // sentences above it, this one names a way out. The single-fan
            // hint first, same as the tooltip: it rests on declined writes,
            // which means a controller answering, so the two cannot honestly
            // apply at once.
            let way_out = if self.readout.single_fan_hint {
                Some(crate::ipc::SINGLE_FAN_HINT)
            } else {
                crate::ipc::layout_hint_words(self.readout.layout_hint)
            };

            if let Some(way_out) = way_out {
                self.text(
                    target,
                    way_out,
                    D2D_RECT_F { left: cl, top: mid + 48.0, right: cr, bottom: mid + 92.0 },
                    &self.axis,
                    theme::ACCENT_BRIGHT,
                )?;
            }

            return Ok(());
        }

        let mut y = panel_top + theme::SPACE_LG;

        // The reading that actually drives the curve, given real prominence:
        // one big number and a quiet caption underneath, not two labels
        // fighting on a line.
        if let Some((index, temp)) = self.readout.hottest {
            // The band color still comes from the Celsius reading: the
            // thresholds are the hardware's, not the display's.
            self.text(
                target,
                &self.temp_text(temp),
                D2D_RECT_F { left: cl, top: y, right: cr, bottom: y + 44.0 },
                &self.big,
                theme::band(temp as f32),
            )?;

            let name = SENSOR_NAMES.get(index).copied().unwrap_or("sensor");
            self.text(
                target,
                &format!("hottest sensor \u{00b7} {name}"),
                D2D_RECT_F { left: cl, top: y + 46.0, right: cr, bottom: y + 62.0 },
                &self.caption,
                theme::TEXT_DIM,
            )?;
        } else {
            self.text(
                target,
                "--\u{00b0}",
                D2D_RECT_F { left: cl, top: y, right: cr, bottom: y + 44.0 },
                &self.big,
                theme::TEXT_FAINT,
            )?;
            self.text(
                target,
                "waiting for a sensor reading",
                D2D_RECT_F { left: cl, top: y + 46.0, right: cr, bottom: y + 62.0 },
                &self.caption,
                theme::TEXT_FAINT,
            )?;
        }

        y += 44.0 + 18.0 + theme::SPACE_MD;
        self.divider(target, cl, cr, y)?;
        y += theme::SPACE_MD;

        let row = 24.0;

        // Bright yellow, because this row is user-changeable and answers a click.
        self.row_wash(target, cl, cr, y, row, self.mode_hot.get())?;
        self.kv_row(target, cl, cr, y, row, "Mode", self.readout.mode, theme::YELLOW)?;
        self.mode_row.set((cl, y, cr, y + row));
        y += row;

        // Drawn in bright yellow, because this row is user-changeable and
        // does something when clicked.
        self.row_wash(target, cl, cr, y, row, self.profile_hot.get())?;
        self.kv_row(target, cl, cr, y, row, "Profile", &self.readout.profile, theme::YELLOW)?;
        self.profile_row.set((cl, y, cr, y + row));
        y += row;

        // Display the EC controller speed byte
        let ctrl_display = if self.readout.fan_ctrl == yamato_ec::FAN_BIOS {
            "0x80 (BIOS)".to_string()
        } else if self.readout.fan_ctrl == yamato_ec::FAN_DISENGAGED {
            "0x40 (Disengaged)".to_string()
        } else if self.readout.fan_ctrl <= yamato_ec::FAN_LEVEL_MAX {
            format!("0x{:02x} (Level {})", self.readout.fan_ctrl, self.readout.fan_ctrl)
        } else {
            format!("0x{:02x}", self.readout.fan_ctrl)
        };
        self.kv_row(target, cl, cr, y, row, "EC Ctrl", &ctrl_display, theme::TEXT)?;
        y += row;

        let fan = if self.readout.fan_rpm[1] > 0 {
            format!("{} / {} rpm", self.readout.fan_rpm[0], self.readout.fan_rpm[1])
        } else {
            format!("{} rpm", self.readout.fan_rpm[0])
        };

        self.kv_row(target, cl, cr, y, row, "Fan", &fan, theme::TEXT)?;
        y += row;

        // Only while a level is held. Nothing below moves when it is absent
        // because the whole column is laid out from a running y, so the row
        // appears and disappears without leaving a gap where it was.
        if self.readout.mode_raw == crate::ipc::MODE_MANUAL {
            self.row_wash(target, cl, cr, y, row, self.level_hot.get())?;
            self.kv_row(
                target,
                cl,
                cr,
                y,
                row,
                "Level",
                &self.held_level().to_string(),
                theme::YELLOW,
            )?;
            self.level_row.set((cl, y, cr, y + row));
            y += row;
        } else {
            self.level_row.set((0.0, 0.0, 0.0, 0.0));
        }
        y += theme::SPACE_SM;

        self.divider(target, cl, cr, y)?;
        y += theme::SPACE_MD;
        self.section_label(target, "SENSORS", cl, y)?;
        y += 16.0 + theme::SPACE_SM;

        // Whatever height is left belongs to the sensor list. Whole-DIP rows
        // keep baselines crisp at 100% scaling; the clamp keeps the list from
        // going airy on a tall window, and packs it on a short one instead of
        // letting it run out of the bottom of its panel.
        let row = ((panel_bottom - theme::SPACE_MD - y) / yamato_ec::SENSOR_COUNT as f32)
            .floor()
            .clamp(12.0, 22.0);

        let hottest = self.readout.hottest.map(|(i, _)| i);

        // The rows are identical and evenly spaced, so one rectangle and a row
        // height is enough to say later which one the pointer was over.
        self.sensor_rows.set((cl, y, cr, row));

        // Then every sensor, so nothing is hidden. One that is not fitted
        // shows a dash, not a misleading zero.
        for (i, reading) in self.readout.sensors.iter().enumerate() {
            // A sensor the engine has been told to leave out is still shown,
            // because hiding it would leave nothing to click to get it back,
            // but it is drawn as what it is: a reading that is not driving
            // anything.
            let ignored = self.config.ignored_sensors.contains(&i);

            // The sensor driving the big number gets its name lifted a step,
            // tying the list back to the headline without an arrow or a badge.
            let name_color = if ignored {
                theme::TEXT_FAINT
            } else if hottest == Some(i) {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            };

            self.text(
                target,
                SENSOR_NAMES.get(i).copied().unwrap_or("--"),
                D2D_RECT_F { left: cl, top: y, right: cr - 48.0, bottom: y + row },
                &self.body,
                name_color,
            )?;

            let (val, color) = match reading {
                Some(t) if ignored => (self.temp_text(*t), theme::TEXT_FAINT),
                Some(t) => (self.temp_text(*t), theme::band(*t as f32)),
                None => ("--".to_string(), theme::TEXT_FAINT),
            };

            self.text(
                target,
                &val,
                D2D_RECT_F { left: cl, top: y, right: cr, bottom: y + row },
                &self.value,
                color,
            )?;

            if ignored {
                // Struck through as well as dimmed. Dimming alone reads as
                // "not reporting", which is a different thing.
                let pen = self.brush(target, theme::TEXT_FAINT)?;
                let middle = y + row / 2.0;

                unsafe {
                    target.DrawLine(
                        D2D_POINT_2F { x: cl, y: middle },
                        D2D_POINT_2F { x: cr, y: middle },
                        &pen,
                        1.0,
                        None,
                    );
                }
            }

            y += row;
        }

        Ok(())
    }

    /// Which settings row is under the pointer, if any.
    fn tuning_hit(&self, x: f32, y: f32) -> Option<Knob> {
        self.tuning_rows
            .get()
            .iter()
            .position(|(left, top, right, bottom)| {
                x >= *left && x <= *right && y >= *top && y <= *bottom
            })
            .map(|i| KNOBS[i])
    }

    /// Which of the chosen point's rows is under the pointer, if any.
    fn point_hit(&self, x: f32, y: f32) -> Option<PointKnob> {
        self.point_rows
            .get()
            .iter()
            .position(|(left, top, right, bottom)| {
                // A zeroed row is one that was not drawn, which is what having
                // nothing selected leaves behind.
                *bottom > *top && x >= *left && x <= *right && y >= *top && y <= *bottom
            })
            .map(|i| POINT_KNOBS[i])
    }

    /// Which sensor row is under the pointer, if any.
    fn sensor_hit(&self, x: f32, y: f32) -> Option<usize> {
        let (left, top, right, height) = self.sensor_rows.get();

        if x < left || x > right {
            return None;
        }

        row_at(top, height, y, yamato_ec::SENSOR_COUNT)
    }

    /// Cycles one setting and writes it back.
    ///
    /// Through the same load, change and save the curve goes through, so the
    /// engine picks it up with the reload it already does. Nothing here decides
    /// anything about the fan: it changes the numbers the engine decides with,
    /// on its own next pass.
    fn change_setting(&mut self, knob: Knob) {
        let path = yamato_core::Config::default_path();
        let Ok(mut config) = yamato_core::Config::load(&path) else { return };

        knob.cycle(&mut config);

        if config.save(&path).is_ok() {
            // Shown now rather than at the next sample, so the row answers the
            // click it was given.
            self.config = config;
            self.invalidate();
        }
    }

    /// Adds a sensor to the ignored list, or takes it off again.
    ///
    /// Ignoring every sensor is allowed and is not dangerous: with nothing
    /// reporting, the curve has nothing to decide on and the engine hands the
    /// fan to the firmware, which is the direction everything here fails in.
    fn toggle_ignored(&mut self, sensor: usize) {
        let path = yamato_core::Config::default_path();
        let Ok(mut config) = yamato_core::Config::load(&path) else { return };

        match config.ignored_sensors.iter().position(|i| *i == sensor) {
            Some(at) => {
                config.ignored_sensors.remove(at);
            }
            None => {
                config.ignored_sensors.push(sensor);
                config.ignored_sensors.sort_unstable();
            }
        }

        if config.save(&path).is_ok() {
            self.config = config;
            self.invalidate();
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.window, None, false);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let this = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Settings;

    // A modal loop somewhere on this thread is already inside a call holding a
    // `&mut Settings`, so nothing here may take one until it has finished.
    // Checked before the reference below is made rather than after.
    if crate::tray::MODAL_DEPTH.with(|d| d.get()) > 0 {
        // Closing is refused outright, not passed on. The default handler
        // answers WM_CLOSE by destroying the window, skipping the
        // save-and-hide this one does for itself, which would throw away
        // unsaved curve edits and leave the tray holding a handle to a window
        // that no longer exists, unreopenable until the program restarts. A
        // close arriving while a name box is open can wait.
        if msg == WM_CLOSE {
            return LRESULT(0);
        }

        return DefWindowProcW(window, msg, wparam, lparam);
    }

    if !this.is_null() {
        let this = &mut *this;
        let area = this.canvas();

        // Mouse positions arrive in physical pixels; layout is in DIPs. Without
        // this the grab points sit where they are drawn only at 100% scaling.
        let scale = this.scale();
        let x = ((lparam.0 & 0xffff) as i16 as f32) / scale;
        let y = (((lparam.0 >> 16) & 0xffff) as i16 as f32) / scale;

        match msg {
            WM_PAINT => {
                let _ = this.paint();
                let _ = ValidateRect(window, None);
                return LRESULT(0);
            }
            WM_SIZE => {
                // Rebuild the render target at the new size on next paint.
                this.target = None;
                this.invalidate();
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                // Dragged to a display with different scaling. Windows hands us
                // the rectangle to adopt; taking it keeps the window the same
                // physical size instead of jumping.
                let suggested = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    window,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );

                this.target = None;
                this.invalidate();
                return LRESULT(0);
            }
            WM_GETMINMAXINFO => {
                // The canvas is whatever is left after the fixed columns, so
                // a window squeezed far enough would compute a negative one.
                // In physical pixels, which is what the layout is not.
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                let scale = this.scale();
                info.ptMinTrackSize.x = (MIN_WIDTH * scale) as i32;
                info.ptMinTrackSize.y = (MIN_HEIGHT * scale) as i32;
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let (pl, pt, pr, pb) = this.picker_box.get();
                let (rl, rt, rr, rb) = this.profile_row.get();

                // Two ways into the same menu: the picker above the graph, and
                // the Profile row in the readout that has always been there.
                if (x >= pl && x <= pr && y >= pt && y <= pb)
                    || (x >= rl && x <= rr && y >= rt && y <= rb)
                {
                    this.show_profile_menu();
                    return LRESULT(0);
                }

                let (ml, mt, mr, mb) = this.mode_row.get();
                if mr > ml && x >= ml && x <= mr && y >= mt && y <= mb {
                    this.cycle_mode();
                    return LRESULT(0);
                }

                let (ll, lt, lr, lb) = this.level_row.get();
                if lr > ll && x >= ll && x <= lr && y >= lt && y <= lb {
                    this.cycle_level();
                    return LRESULT(0);
                }

                let (dl, dt, dr, db) = this.discard_button.get();
                if dr > dl && x >= dl && x <= dr && y >= dt && y <= db {
                    this.discard_edits();
                    return LRESULT(0);
                }

                let (bl, bt, br, bb) = this.save_button.get();
                if x >= bl && x <= br && y >= bt && y <= bb {
                    // Dimmed means disabled: with nothing changed there is
                    // nothing for this to write.
                    if this.editor.is_dirty() {
                        let _ = this.apply();
                        this.invalidate();
                    }
                    return LRESULT(0);
                }

                if let Some(knob) = this.tuning_hit(x, y) {
                    this.change_setting(knob);
                    return LRESULT(0);
                }

                if let Some(knob) = this.point_hit(x, y) {
                    match knob {
                        PointKnob::SlowDown => this.editor.step_hyst_down(),
                        PointKnob::SpeedUp => this.editor.step_hyst_up(),
                    }
                    this.invalidate();
                    return LRESULT(0);
                }

                if this.editor.begin_drag(area, x, y) {
                    SetCapture(window);
                    this.invalidate();
                    return LRESULT(0);
                }

                // A click on bare graph puts the selection down, so the rows
                // stop talking about a point nobody is looking at.
                if x >= area.left && x <= area.right && y >= area.top && y <= area.bottom {
                    this.editor.select(None);
                    this.invalidate();
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                if this.editor.is_dragging() {
                    this.editor.drag_to(area, x, y);
                    this.invalidate();
                    return LRESULT(0);
                }

                // Only when it changes, so moving the pointer across the window
                // does not repaint it a hundred times on the way.
                let (pl, pt, pr, pb) = this.picker_box.get();
                let hot = x >= pl && x <= pr && y >= pt && y <= pb;
                if hot != this.picker_hot.get() {
                    this.picker_hot.set(hot);
                    this.invalidate();
                }

                // The two readout rows that answer a click, by the same rule
                // and against the same rectangles the click is tested with.
                // An empty rectangle is the fault state, where nothing
                // clickable is drawn, and a zero-width test never matches.
                for (rect, flag) in [
                    (this.mode_row.get(), &this.mode_hot),
                    (this.level_row.get(), &this.level_hot),
                    (this.profile_row.get(), &this.profile_hot),
                ] {
                    let (l, t, r, b) = rect;
                    let over = r > l && x >= l && x <= r && y >= t && y <= b;

                    if over != flag.get() {
                        flag.set(over);
                        this.invalidate();
                    }
                }

                // Same rule for the settings rows, whose description takes
                // over the paragraph beneath them.
                let over = this
                    .tuning_rows
                    .get()
                    .iter()
                    .position(|(l, t, r, b)| x >= *l && x <= *r && y >= *t && y <= *b);

                if over != this.hot_knob.get() {
                    this.hot_knob.set(over);
                    this.invalidate();
                }

                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                if this.editor.is_dragging() {
                    this.editor.end_drag();
                    let _ = ReleaseCapture();
                    this.invalidate();
                }
                return LRESULT(0);
            }
            WM_LBUTTONDBLCLK => {
                // Inside the graph, and nowhere else. Changing a setting means
                // clicking its row, and clicking a row twice is a double
                // click: without this, adjusting the poll interval quietly
                // added curve points at the top of the range, because the
                // coordinates clamp instead of refusing.
                if x >= area.left && x <= area.right && y >= area.top && y <= area.bottom {
                    this.editor.add_point(area, x, y);
                    this.invalidate();
                }

                return LRESULT(0);
            }
            WM_RBUTTONUP => {
                // The sensor list is outside the canvas, so this can never be
                // the same click as removing a curve point.
                if let Some(sensor) = this.sensor_hit(x, y) {
                    this.toggle_ignored(sensor);
                    return LRESULT(0);
                }

                if let Some(i) = this.editor.hit_test(area, x, y) {
                    this.editor.remove_point(i);
                    this.invalidate();
                }
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                const VK_S: usize = 0x53;
                const VK_RETURN: usize = 0x0d;
                const VK_ESCAPE: usize = 0x1b;

                let ctrl = GetKeyState(0x11) < 0; // VK_CONTROL
                if (ctrl && wparam.0 == VK_S) || wparam.0 == VK_RETURN {
                    let _ = this.apply();
                    this.invalidate();
                } else if wparam.0 == VK_ESCAPE {
                    this.discard_edits();
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                // Saving on close, not discarding. Nobody drags a fan curve
                // into shape and then means to throw it away, and the
                // alternative is a dialog with one sensible answer.
                this.save_if_dirty();

                // Closing destroys, rather than hiding. The tray icon is the
                // program's real home, and a hidden window is not free: it
                // holds a D2D factory, a DirectWrite factory, eleven text
                // formats and a render target with a back buffer the size of
                // the window, none of which a window nobody can see has any
                // use for. Hiding kept all of it for the life of the process,
                // which is what took a 2 MB tray to twenty-something and left
                // it there.
                //
                // Reopening builds a fresh one. The tray already knew how:
                // its open path checks IsWindow and rebuilds when the handle
                // no longer names anything, which was written as a recovery
                // and is now the ordinary route.
                let _ = DestroyWindow(window);
                return LRESULT(0);
            }
            WM_DESTROY => {
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                return LRESULT(0);
            }
            _ => {}
        }
    }

    DefWindowProcW(window, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yamato-knob-{tag}-{}.json", std::process::id()));
        p
    }

    #[test]
    fn the_canvas_leaves_room_for_axis_labels() {
        // Temperatures are drawn under the graph and levels to its left; if
        // the canvas started at zero they would be clipped away.
        assert!(super::CANVAS_TOP > 0.0);
        assert!(super::CANVAS_BOTTOM_GAP > 24.0);
    }

    #[test]
    fn every_settings_row_is_a_different_setting() {
        // The rows are identified by their position in this list, so a
        // duplicate would be two rows changing the same thing.
        for (i, knob) in KNOBS.iter().enumerate() {
            assert!(!KNOBS[..i].contains(knob), "{knob:?} appears twice");
        }
    }

    #[test]
    fn cycling_a_setting_never_writes_what_the_loader_would_change() {
        // The safety property of these rows: a control that wrote a value the
        // loader then quietly clamped would be a control that lies about what
        // it did, and on a fan controller the poll and the watchdog are not
        // cosmetic.
        let path = temp_path("cycle");
        let mut config = yamato_core::Config::default();

        for knob in KNOBS {
            // Round the whole list of presets more than once.
            for _ in 0..12 {
                knob.cycle(&mut config);
                config.save(&path).unwrap();

                let back = yamato_core::Config::load(&path).unwrap();

                assert_eq!(back.poll_secs, config.poll_secs, "{knob:?} moved the poll");
                assert_eq!(
                    back.standby_poll_secs, config.standby_poll_secs,
                    "{knob:?} moved the standby poll"
                );
                assert_eq!(
                    back.watchdog_secs, config.watchdog_secs,
                    "{knob:?} left the watchdog below its floor"
                );
                assert_eq!(back.log_max_mb, config.log_max_mb);
                assert_eq!(
                    back.manual_escape_c, config.manual_escape_c,
                    "{knob:?} moved the escape out of range"
                );
                assert_eq!(back.startup_mode, config.startup_mode);
                assert_eq!(back.log_enabled, config.log_enabled);
                assert_eq!(back.single_fan, config.single_fan);
                assert_eq!(back.ec_layout, config.ec_layout, "{knob:?} moved the layout");
            }
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_poll_row_visits_every_preset_and_comes_back() {
        let mut config = yamato_core::Config::default();
        let mut seen = Vec::new();

        for _ in 0..POLL_PRESETS.len() {
            Knob::Poll.cycle(&mut config);
            seen.push(config.poll_secs);
        }

        for preset in POLL_PRESETS {
            assert!(seen.contains(&preset), "{preset} s was never offered");
        }
    }

    #[test]
    fn a_value_from_a_hand_edited_file_still_moves_on() {
        // 7 is nobody's preset. The row still has to do something sensible
        // with it instead of jumping back to the start of the list.
        assert_eq!(next_preset(&POLL_PRESETS, 7), 10);
        assert_eq!(next_preset(&POLL_PRESETS, 60), POLL_PRESETS[0]);
        assert_eq!(next_preset(&POLL_PRESETS, 200), POLL_PRESETS[0]);
    }

    #[test]
    fn a_slower_poll_takes_the_watchdog_with_it() {
        // Otherwise the loader raises it afterwards and the row shows a number
        // that was never written.
        let mut config = yamato_core::Config::default();

        for _ in 0..POLL_PRESETS.len() * 2 {
            Knob::Poll.cycle(&mut config);
            assert!(
                config.watchdog_secs
                    >= yamato_core::watchdog_floor(config.poll_secs, config.standby_poll_secs)
            );
        }
    }

    #[test]
    fn the_two_state_rows_go_back_as_well_as_forward() {
        let mut config = yamato_core::Config::default();

        for knob in [Knob::StartIn, Knob::Logging, Knob::SingleFan, Knob::EcLayout] {
            let before = knob.value(&config);
            knob.cycle(&mut config);
            assert_ne!(knob.value(&config), before, "{knob:?} did not change");
            knob.cycle(&mut config);
            assert_eq!(knob.value(&config), before, "{knob:?} did not come back");
        }
    }

    #[test]
    fn the_controller_mode_row_is_the_last_thing_anyone_scrolls_past() {
        // An advanced override for which ports reach the controller has no
        // business sitting among the everyday rows, so it stays at the end.
        assert_eq!(KNOBS.last(), Some(&Knob::EcLayout));
    }

    #[test]
    fn the_controller_mode_row_is_an_override_between_the_two_real_modes() {
        // A fresh file the probe has not decided reads as Standard, because
        // that is where almost every machine lives; the first click flips it
        // to Compatibility, and from there it only ever cycles between the
        // two concrete answers. There is deliberately no third state: the
        // probe runs once, at the first start, and this row exists to
        // overrule it, not to re-ask it.
        let mut config = yamato_core::Config::default();
        assert_eq!(config.ec_layout, None);
        assert_eq!(Knob::EcLayout.value(&config), "Standard");

        Knob::EcLayout.cycle(&mut config);
        assert_eq!(config.ec_layout, Some(yamato_core::EcLayout::Compat));
        assert_eq!(Knob::EcLayout.value(&config), "Compatibility");

        Knob::EcLayout.cycle(&mut config);
        assert_eq!(config.ec_layout, Some(yamato_core::EcLayout::Standard));
        assert_eq!(Knob::EcLayout.value(&config), "Standard");
    }

    #[test]
    fn the_controller_mode_words_match_the_hint_that_names_them() {
        // The tooltip tells somebody to try Compatibility mode; this row is
        // where they go to do it. If the words drift apart the advice points
        // at a control that does not exist.
        let config = yamato_core::Config {
            ec_layout: Some(yamato_core::EcLayout::Compat),
            ..yamato_core::Config::default()
        };

        let value = Knob::EcLayout.value(&config);
        let hint = crate::ipc::layout_hint_words(crate::ipc::LAYOUT_HINT_TRY_COMPAT).unwrap();

        assert!(hint.contains(&value), "the hint says {hint:?} but the row says {value:?}");
        assert!(Knob::EcLayout.describe().starts_with("Advanced"));
    }

    #[test]
    fn every_row_says_what_it_is_for_in_the_space_available() {
        // The box these land in is two lines at caption size, and text that
        // does not fit is clipped rather than wrapped. The reference is the
        // standing hint they replace, which is known to fit.
        const STANDING_HINT: &str =
            "Click a value to change it. Right-click a sensor to leave it out of the decision.";

        for knob in KNOBS {
            let text = knob.describe();

            assert!(!text.is_empty(), "{knob:?} has no description");
            assert!(
                text.len() <= STANDING_HINT.len(),
                "{knob:?} is {} characters, past the {} that are known to fit",
                text.len(),
                STANDING_HINT.len()
            );
            assert!(text.ends_with('.'), "{knob:?} is not a sentence");
        }
    }

    #[test]
    fn standby_is_not_a_row_anybody_has_to_answer() {
        // What to do when the screen goes off used to be a setting, and it was
        // a setting because nothing could tell a machine working with its lid
        // shut from one asleep in a bag. The engine measures that now, so the
        // question is answered rather than asked.
        assert!(!KNOBS.iter().any(|k| format!("{k:?}") == "Standby"));
    }

    #[test]
    fn the_fan_count_is_reachable_from_the_window_and_starts_at_dual() {
        // The setting exists for people whose machine is failing in a way
        // that looks alarming, so it has to be reachable without a text
        // editor: a drawn row, clickable like its neighbors.
        assert!(KNOBS.contains(&Knob::SingleFan));

        let mut config = yamato_core::Config::default();
        assert_eq!(Knob::SingleFan.value(&config), "Dual");

        Knob::SingleFan.cycle(&mut config);
        assert!(config.single_fan);
        assert_eq!(Knob::SingleFan.value(&config), "Single");
    }

    #[test]
    fn rows_are_found_by_arithmetic_not_by_luck() {
        // The sensor list is hit tested by dividing, so the boundaries are
        // where an off-by-one would show up.
        assert_eq!(row_at(100.0, 20.0, 100.0, 12), Some(0));
        assert_eq!(row_at(100.0, 20.0, 119.9, 12), Some(0));
        assert_eq!(row_at(100.0, 20.0, 120.0, 12), Some(1));
        assert_eq!(row_at(100.0, 20.0, 339.0, 12), Some(11));
        // Past the end of the list, above it, and a list that was never drawn.
        assert_eq!(row_at(100.0, 20.0, 341.0, 12), None);
        assert_eq!(row_at(100.0, 20.0, 99.0, 12), None);
        assert_eq!(row_at(0.0, 0.0, 0.0, 12), None);
    }

    #[test]
    fn the_point_rows_say_what_happens_rather_than_what_it_is_called() {
        // Somebody tuning their fan should not have to learn the word
        // hysteresis to find out why it keeps changing speed.
        let point = yamato_core::CurvePoint::new(70, 4).with_hysteresis(2, 6);

        assert_eq!(PointKnob::SlowDown.value(&point), "6\u{00b0} cooler");
        assert_eq!(PointKnob::SpeedUp.value(&point), "2\u{00b0} hotter");

        for knob in POINT_KNOBS {
            assert!(!knob.label().to_lowercase().contains("hyster"));
        }
    }

    #[test]
    fn a_point_with_no_wait_says_so_in_words() {
        let point = yamato_core::CurvePoint::new(70, 4).with_hysteresis(0, 0);

        for knob in POINT_KNOBS {
            assert_eq!(knob.value(&point), "right away");
        }
    }

    #[test]
    fn the_management_items_cannot_be_mistaken_for_a_profile() {
        // One comparison tells them apart, so they have to be on the right
        // side of it; the subtraction that follows would otherwise underflow.
        for id in [PROFILE_NEW, PROFILE_DUPLICATE, PROFILE_RENAME, PROFILE_DELETE] {
            assert!(id < PROFILE_MENU_BASE);
        }
    }
}
