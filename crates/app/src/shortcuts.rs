//! Keyboard shortcuts: one table drives the key handling, the menus' shortcut labels and the
//! Help > Keyboard Shortcuts window.
//!
//! The timeline widget handles its own keys while it has focus (it consumes them first); the app
//! handles the rest after the panels ran, so a key is never acted on twice.

use eframe::egui::{Key, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    PlayPause,
    PlayFromIn,
    ShuttleForward,
    ShuttleReverse,
    ShuttleStop,
    FrameBack,
    FrameForward,
    FrameBack10,
    FrameForward10,
    PrevEdit,
    NextEdit,
    GoStart,
    GoEnd,
    SetIn,
    SetOut,
    ClearInOut,
    ToggleLoop,
    Undo,
    Redo,
    New,
    Open,
    Save,
    SaveAs,
    Close,
    Import,
    Export,
    Split,
    Delete,
    RippleDelete,
    SelectAll,
    Deselect,
    AddMarker,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ToggleSafe,
    ToolSelect,
    ToolRazor,
    ToolSlip,
    ToolSlide,
    ToolRipple,
    ToolRoll,
    ProjectSettings,
    Preferences,
    AiPanel,
    Shortcuts,
}

/// When a shortcut applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Always (unless a text field has the keyboard).
    Global,
    /// Only while the program viewer has focus (the last panel clicked).
    Viewer,
}

#[derive(Clone, Copy, Debug)]
pub struct Shortcut {
    pub key: Key,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub cmd: Command,
    pub scope: Scope,
    pub help: &'static str,
}

const fn sc(key: Key, ctrl: bool, shift: bool, alt: bool, cmd: Command, scope: Scope, help: &'static str) -> Shortcut {
    Shortcut { key, ctrl, shift, alt, cmd, scope, help }
}

use Command as C;
use Scope::{Global as G, Viewer as V};

pub static SHORTCUTS: &[Shortcut] = &[
    sc(Key::Space, false, false, false, C::PlayPause, G, "Play / pause"),
    sc(Key::Space, false, true, false, C::PlayFromIn, G, "Play from the in point (to the out point)"),
    sc(Key::L, false, false, false, C::ShuttleForward, G, "Play forward; again = 2x, 4x, 8x"),
    sc(Key::J, false, false, false, C::ShuttleReverse, G, "Play backward; again = 2x, 4x, 8x"),
    sc(Key::K, false, false, false, C::ShuttleStop, G, "Stop (hold K + J/L = step one frame)"),
    sc(Key::ArrowLeft, false, false, false, C::FrameBack, V, "One frame back"),
    sc(Key::ArrowRight, false, false, false, C::FrameForward, V, "One frame forward"),
    sc(Key::ArrowLeft, false, true, false, C::FrameBack10, V, "Ten frames back"),
    sc(Key::ArrowRight, false, true, false, C::FrameForward10, V, "Ten frames forward"),
    sc(Key::ArrowUp, false, false, false, C::PrevEdit, V, "Previous edit point"),
    sc(Key::ArrowDown, false, false, false, C::NextEdit, V, "Next edit point"),
    sc(Key::Home, false, false, false, C::GoStart, G, "Go to start"),
    sc(Key::End, false, false, false, C::GoEnd, G, "Go to end"),
    sc(Key::I, false, false, false, C::SetIn, G, "Set in point"),
    sc(Key::O, false, false, false, C::SetOut, G, "Set out point"),
    sc(Key::X, false, false, true, C::ClearInOut, G, "Clear in and out"),
    sc(Key::L, true, false, false, C::ToggleLoop, G, "Loop playback"),
    sc(Key::Z, true, false, false, C::Undo, G, "Undo"),
    sc(Key::Y, true, false, false, C::Redo, G, "Redo"),
    sc(Key::Z, true, true, false, C::Redo, G, "Redo"),
    sc(Key::N, true, false, false, C::New, G, "New project"),
    sc(Key::O, true, false, false, C::Open, G, "Open project"),
    sc(Key::S, true, false, false, C::Save, G, "Save"),
    sc(Key::S, true, true, false, C::SaveAs, G, "Save as"),
    sc(Key::W, true, false, false, C::Close, G, "Close project"),
    sc(Key::I, true, false, false, C::Import, G, "Import media"),
    sc(Key::M, true, false, false, C::Export, G, "Export"),
    sc(Key::K, true, false, false, C::Split, G, "Split at playhead"),
    sc(Key::S, false, false, false, C::Split, G, "Split at playhead"),
    sc(Key::Delete, false, false, false, C::Delete, G, "Delete (leave gap)"),
    sc(Key::Backspace, false, false, false, C::Delete, G, "Delete (leave gap)"),
    sc(Key::Delete, false, true, false, C::RippleDelete, G, "Ripple delete"),
    sc(Key::A, true, false, false, C::SelectAll, G, "Select all clips"),
    sc(Key::Escape, false, false, false, C::Deselect, G, "Clear selection"),
    sc(Key::M, false, false, false, C::AddMarker, G, "Add marker at playhead"),
    sc(Key::Equals, false, false, false, C::ZoomIn, G, "Zoom timeline in"),
    sc(Key::Plus, false, false, false, C::ZoomIn, G, "Zoom timeline in"),
    sc(Key::Minus, false, false, false, C::ZoomOut, G, "Zoom timeline out"),
    sc(Key::Backslash, false, false, false, C::ZoomFit, G, "Fit timeline"),
    sc(Key::Quote, true, false, false, C::ToggleSafe, G, "Safe-area guides"),
    sc(Key::V, false, false, false, C::ToolSelect, G, "Selection tool"),
    sc(Key::C, false, false, false, C::ToolRazor, G, "Razor tool"),
    sc(Key::Y, false, false, false, C::ToolSlip, G, "Slip tool (move the media under a clip)"),
    sc(Key::U, false, false, false, C::ToolSlide, G, "Slide tool (move a clip between its neighbours)"),
    sc(Key::B, false, false, false, C::ToolRipple, G, "Ripple edit tool"),
    sc(Key::R, false, false, false, C::ToolRoll, G, "Rolling edit tool"),
    sc(Key::Comma, true, false, false, C::Preferences, G, "Preferences"),
    sc(Key::P, true, true, false, C::ProjectSettings, G, "Project settings"),
    sc(Key::Space, true, false, false, C::AiPanel, G, "Ask the AI"),
    sc(Key::F1, false, false, false, C::Shortcuts, G, "Keyboard shortcuts"),
];

/// Keys only the timeline handles (while it is the active panel), for the shortcuts window. The
/// timeline consumes them before the app's shortcuts run; none is bound to something else here.
pub static TIMELINE_KEYS: &[(&str, &str)] = &[
    ("Q", "Ripple trim the start to the playhead"),
    ("W", "Ripple trim the end to the playhead"),
    ("Alt+[", "Trim the start to the playhead"),
    ("Alt+]", "Trim the end to the playhead"),
    ("Alt+I", "Clear the in point"),
    ("Alt+O", "Clear the out point"),
    ("Shift+M", "Add a chapter marker"),
    ("N", "Snapping on/off"),
    ("A", "Selection tool"),
    ("←/→", "Nudge the selection one frame (Shift: ten)"),
    ("↑/↓", "Previous / next edit point"),
];

/// The timeline tool a tool command selects.
pub fn tool_of(c: Command) -> Option<aiframes_ui::timeline::Tool> {
    use aiframes_ui::timeline::Tool;
    Some(match c {
        Command::ToolSelect => Tool::Select,
        Command::ToolRazor => Tool::Razor,
        Command::ToolSlip => Tool::Slip,
        Command::ToolSlide => Tool::Slide,
        Command::ToolRipple => Tool::Ripple,
        Command::ToolRoll => Tool::Roll,
        _ => return None,
    })
}

/// The tool command of a timeline tool (for menu shortcut labels).
pub fn tool_command(t: aiframes_ui::timeline::Tool) -> Command {
    use aiframes_ui::timeline::Tool;
    match t {
        Tool::Select => Command::ToolSelect,
        Tool::Razor => Command::ToolRazor,
        Tool::Slip => Command::ToolSlip,
        Tool::Slide => Command::ToolSlide,
        Tool::Ripple => Command::ToolRipple,
        Tool::Roll => Command::ToolRoll,
    }
}

/// The command bound to `key` with exactly these modifiers, for the given focus.
pub fn lookup(key: Key, mods: Modifiers, viewer_focused: bool) -> Option<Command> {
    let ctrl = mods.ctrl || mods.command || mods.mac_cmd;
    SHORTCUTS
        .iter()
        .find(|s| s.key == key && s.ctrl == ctrl && s.shift == mods.shift && s.alt == mods.alt && (s.scope == Scope::Global || viewer_focused))
        .map(|s| s.cmd)
}

/// Display text of a shortcut ("Ctrl+Shift+S").
pub fn text(s: &Shortcut) -> String {
    let mut out = String::new();
    if s.ctrl {
        out.push_str("Ctrl+");
    }
    if s.shift {
        out.push_str("Shift+");
    }
    if s.alt {
        out.push_str("Alt+");
    }
    out.push_str(key_name(s.key));
    out
}

/// The first shortcut of a command, for menu labels.
pub fn label(cmd: Command) -> String {
    SHORTCUTS.iter().find(|s| s.cmd == cmd).map(text).unwrap_or_default()
}

fn key_name(k: Key) -> &'static str {
    match k {
        Key::Space => "Space",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::Home => "Home",
        Key::End => "End",
        Key::Delete => "Del",
        Key::Backspace => "Backspace",
        Key::Escape => "Esc",
        Key::Equals => "=",
        Key::Plus => "+",
        Key::Minus => "-",
        Key::Backslash => "\\",
        Key::Quote => "'",
        Key::Comma => ",",
        Key::F1 => "F1",
        other => other.name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
        Modifiers { alt, ctrl, shift, mac_cmd: false, command: ctrl }
    }

    #[test]
    fn mapping() {
        assert_eq!(lookup(Key::Space, m(false, false, false), false), Some(Command::PlayPause));
        assert_eq!(lookup(Key::Space, m(false, true, false), false), Some(Command::PlayFromIn));
        assert_eq!(lookup(Key::Z, m(true, false, false), false), Some(Command::Undo));
        assert_eq!(lookup(Key::Z, m(true, true, false), false), Some(Command::Redo));
        assert_eq!(lookup(Key::Y, m(true, false, false), false), Some(Command::Redo));
        assert_eq!(lookup(Key::S, m(true, false, false), false), Some(Command::Save));
        assert_eq!(lookup(Key::S, m(true, true, false), false), Some(Command::SaveAs));
        assert_eq!(lookup(Key::S, m(false, false, false), false), Some(Command::Split));
        assert_eq!(lookup(Key::I, m(true, false, false), false), Some(Command::Import));
        assert_eq!(lookup(Key::I, m(false, false, false), false), Some(Command::SetIn));
        assert_eq!(lookup(Key::M, m(true, false, false), false), Some(Command::Export));
        assert_eq!(lookup(Key::L, m(false, false, false), false), Some(Command::ShuttleForward));
        assert_eq!(lookup(Key::J, m(false, false, false), false), Some(Command::ShuttleReverse));
        assert_eq!(lookup(Key::K, m(false, false, false), false), Some(Command::ShuttleStop));
        assert_eq!(lookup(Key::Home, m(false, false, false), false), Some(Command::GoStart));
        assert_eq!(lookup(Key::Delete, m(false, true, false), false), Some(Command::RippleDelete));
        assert_eq!(lookup(Key::Q, m(true, true, true), false), None);
    }

    #[test]
    fn arrows_need_viewer_focus() {
        assert_eq!(lookup(Key::ArrowLeft, m(false, false, false), false), None);
        assert_eq!(lookup(Key::ArrowLeft, m(false, false, false), true), Some(Command::FrameBack));
        assert_eq!(lookup(Key::ArrowRight, m(false, true, false), true), Some(Command::FrameForward10));
    }

    #[test]
    fn tools_round_trip() {
        for t in aiframes_ui::timeline::Tool::ALL {
            let c = tool_command(t);
            assert_eq!(tool_of(c), Some(t));
            // The app's key for the tool is the timeline's own key for it.
            assert_eq!(label(c), t.key(), "{t:?}");
        }
        assert_eq!(lookup(Key::U, m(false, false, false), false), Some(Command::ToolSlide));
    }

    #[test]
    fn labels() {
        assert_eq!(label(Command::SaveAs), "Ctrl+Shift+S");
        assert_eq!(label(Command::PlayPause), "Space");
        assert_eq!(label(Command::Export), "Ctrl+M");
    }

    #[test]
    fn timeline_keys_do_not_clash_with_app_shortcuts() {
        // A key the timeline consumes must not mean something else as an app shortcut (it would
        // only work while the timeline is not active).
        for (k, _) in TIMELINE_KEYS {
            if let Some(s) = SHORTCUTS.iter().find(|s| text(s) == *k) {
                panic!("{k} is the timeline's key but the app binds it to {:?}", s.cmd);
            }
        }
    }

    #[test]
    fn no_conflicting_bindings() {
        for (i, a) in SHORTCUTS.iter().enumerate() {
            for b in &SHORTCUTS[i + 1..] {
                let same = a.key == b.key && a.ctrl == b.ctrl && a.shift == b.shift && a.alt == b.alt;
                assert!(!same || a.cmd == b.cmd, "{:?} and {:?} share {}", a.cmd, b.cmd, text(a));
            }
        }
    }
}
