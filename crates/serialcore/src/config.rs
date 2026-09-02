//! Configuration and serde types: port identity, per-port config, and global
//! settings. These are UI-agnostic and serialize to TOML.

use serde::{Deserialize, Serialize};

/// Identifies a physical device, NOT a path. This is what makes reconnect work.
///
/// Two identities are considered the "same device" by the matching rules in
/// [`crate::enumerate`], not by structural equality — see that module.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortIdentity {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    /// Fallback only, for non-USB ports (built-in UARTs, virtual ports).
    #[serde(default)]
    pub path_fallback: String,
    /// Disambiguates multi-interface devices (e.g. a debug probe exposing two
    /// CDC interfaces with the same serial number).
    #[serde(default)]
    pub interface_hint: Option<u8>,
}

impl PortIdentity {
    /// True when this identity carries USB VID/PID information.
    pub fn has_usb(&self) -> bool {
        self.vid.is_some() && self.pid.is_some()
    }

    /// A short human label for tabs and log filenames.
    pub fn label(&self) -> String {
        if let Some(product) = &self.product {
            product.clone()
        } else if self.has_usb() {
            format!(
                "{:04x}:{:04x}",
                self.vid.unwrap_or(0),
                self.pid.unwrap_or(0)
            )
        } else {
            self.path_fallback.clone()
        }
    }
}

/// Number of data bits. Mirror of `serialport::DataBits` with serde support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

impl From<DataBits> for u8 {
    fn from(b: DataBits) -> u8 {
        match b {
            DataBits::Five => 5,
            DataBits::Six => 6,
            DataBits::Seven => 7,
            DataBits::Eight => 8,
        }
    }
}

impl TryFrom<u8> for DataBits {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, String> {
        match v {
            5 => Ok(DataBits::Five),
            6 => Ok(DataBits::Six),
            7 => Ok(DataBits::Seven),
            8 => Ok(DataBits::Eight),
            other => Err(format!("invalid data_bits: {other} (expected 5..=8)")),
        }
    }
}

impl From<DataBits> for serialport::DataBits {
    fn from(b: DataBits) -> serialport::DataBits {
        match b {
            DataBits::Five => serialport::DataBits::Five,
            DataBits::Six => serialport::DataBits::Six,
            DataBits::Seven => serialport::DataBits::Seven,
            DataBits::Eight => serialport::DataBits::Eight,
        }
    }
}

/// Parity checking mode. Mirror of `serialport::Parity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    None,
    Odd,
    Even,
}

impl From<Parity> for serialport::Parity {
    fn from(p: Parity) -> serialport::Parity {
        match p {
            Parity::None => serialport::Parity::None,
            Parity::Odd => serialport::Parity::Odd,
            Parity::Even => serialport::Parity::Even,
        }
    }
}

/// Number of stop bits. Mirror of `serialport::StopBits`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum StopBits {
    One,
    Two,
}

impl From<StopBits> for u8 {
    fn from(b: StopBits) -> u8 {
        match b {
            StopBits::One => 1,
            StopBits::Two => 2,
        }
    }
}

impl TryFrom<u8> for StopBits {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, String> {
        match v {
            1 => Ok(StopBits::One),
            2 => Ok(StopBits::Two),
            other => Err(format!("invalid stop_bits: {other} (expected 1 or 2)")),
        }
    }
}

impl From<StopBits> for serialport::StopBits {
    fn from(b: StopBits) -> serialport::StopBits {
        match b {
            StopBits::One => serialport::StopBits::One,
            StopBits::Two => serialport::StopBits::Two,
        }
    }
}

/// Flow control mode. Mirror of `serialport::FlowControl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowControl {
    None,
    Software,
    Hardware,
}

impl From<FlowControl> for serialport::FlowControl {
    fn from(f: FlowControl) -> serialport::FlowControl {
        match f {
            FlowControl::None => serialport::FlowControl::None,
            FlowControl::Software => serialport::FlowControl::Software,
            FlowControl::Hardware => serialport::FlowControl::Hardware,
        }
    }
}

/// How incoming carriage returns are turned into lines (the "terminal type").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalMode {
    /// VT100/Linux: only `\n` (or `\r\n`) ends a line; a bare `\r` rewinds the
    /// current line so following text overwrites it (progress bars/spinners).
    #[default]
    Vt100,
    /// Only `\n` ends a line; every bare `\r` is discarded.
    LfOnly,
    /// `\n`, `\r\n`, and a bare `\r` each end a line (legacy behavior).
    Classic,
}

impl TerminalMode {
    pub fn label(self) -> &'static str {
        match self {
            TerminalMode::Vt100 => "VT100 / Linux",
            TerminalMode::LfOnly => "LF only (strip CR)",
            TerminalMode::Classic => "Classic (CR or LF)",
        }
    }
}

/// A line ending appended to transmitted text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineEnding {
    None,
    #[default]
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            LineEnding::None => b"",
            LineEnding::Lf => b"\n",
            LineEnding::CrLf => b"\r\n",
            LineEnding::Cr => b"\r",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::None => "none",
            LineEnding::Lf => "\\n",
            LineEnding::CrLf => "\\r\\n",
            LineEnding::Cr => "\\r",
        }
    }
}

/// Serial line parameters used both to open a port and to reopen it on reconnect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortConfig {
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
    /// Toggling DTR resets many boards; make it explicit and configurable.
    pub dtr_on_open: bool,
    pub rts_on_open: bool,
    /// How incoming `\r` is framed. Serde-default so older configs load.
    #[serde(default)]
    pub terminal: TerminalMode,
    /// Line ending appended to sent input.
    #[serde(default)]
    pub line_ending: LineEnding,
    /// Echo sent input as a line in the log. Off by default: a device that
    /// echoes its own input would otherwise show every keystroke twice.
    #[serde(default)]
    pub local_echo: bool,
    /// Recall sent-input history with Up/Down (captured locally, never sent).
    /// Off by default so Up/Down reach the device's own shell history.
    #[serde(default)]
    pub local_history: bool,
}

impl Default for PortConfig {
    fn default() -> Self {
        PortConfig {
            baud: 115_200,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            dtr_on_open: true,
            rts_on_open: false,
            terminal: TerminalMode::default(),
            line_ending: LineEnding::default(),
            local_echo: false,
            local_history: false,
        }
    }
}

impl PortConfig {
    /// A compact `115200 8N1` style summary for the status bar.
    pub fn summary(&self) -> String {
        let parity = match self.parity {
            Parity::None => 'N',
            Parity::Odd => 'O',
            Parity::Even => 'E',
        };
        format!(
            "{} {}{}{}",
            self.baud,
            u8::from(self.data_bits),
            parity,
            u8::from(self.stop_bits)
        )
    }
}

/// How timestamps are rendered in the log gutter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimestampFormat {
    /// Wall clock, dated. A console can hold days of output — restored history
    /// above this session's — so the date is what tells one day's lines from
    /// another's.
    #[default]
    Absolute,
    /// Wall clock, time of day only. Half the width of the dated form, for a
    /// session short enough that the date is the same on every line and the
    /// columns are better spent on the text.
    Time,
    Delta,
    Mark,
    None,
}

/// A render-time highlight rule. First matching rule wins (see spec §7.9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightRule {
    pub pattern: String,
    /// `#rrggbb`.
    pub color: String,
    /// Match the pattern with exact letter case. Missing in older configs,
    /// where highlights were always case-insensitive.
    #[serde(default)]
    pub case_sensitive: bool,
    /// Not rendered, and no longer offered in the UI.
    ///
    /// egui has no synthetic bold and no bold monospace face is bundled, so
    /// there is nothing to draw a bold run *with* — highlighting is expressed
    /// through colour alone (see `pigtail`'s `CompiledHighlight`). The field
    /// stays so a config written when the checkbox existed still loads, and so
    /// the setting is preserved rather than dropped on the next save, should a
    /// bold face ever be added (issue #45).
    #[serde(default)]
    pub bold: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Extraction rule mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractMode {
    /// `temp:23.4, rpm:1200` — every key becomes a series.
    Kv,
    /// Named capture groups become series.
    Regex,
}

/// A numeric-series extraction rule (see spec §7.13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractRule {
    pub mode: ExtractMode,
    /// Only lines starting with this prefix are parsed, when set.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Regex pattern (regex mode only).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Key/value separators for kv mode; defaults cover `:` and `=`.
    #[serde(default)]
    pub kv_separators: Option<Vec<char>>,
}

/// Bounds for [`Settings::console_font_size`], shared by the settings pane and
/// the console's Ctrl+wheel gesture. Anything outside this is unreadable or
/// leaves no room for a line of output.
pub const MIN_CONSOLE_FONT_SIZE: u8 = 6;
pub const MAX_CONSOLE_FONT_SIZE: u8 = 40;

/// Global settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    /// Point size of the console's monospace text (see the bounds above).
    #[serde(default = "default_console_font_size")]
    pub console_font_size: u8,
    /// Fold a line too long for the window onto further rows instead of letting
    /// it run off the right edge.
    #[serde(default = "default_true")]
    pub wrap_lines: bool,
    #[serde(default)]
    pub timestamp_format: TimestampFormat,
    #[serde(default = "default_retention")]
    pub session_retention_days: u32,
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Ask GitHub for a newer release at startup. This is pigtail's only
    /// outbound network request; off means it makes none.
    #[serde(default = "default_check_updates")]
    pub check_updates: bool,
    /// A release the user chose to skip, which silences the startup notice until
    /// something newer than this is published.
    #[serde(default)]
    pub skipped_version: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_lines: default_max_lines(),
            console_font_size: default_console_font_size(),
            wrap_lines: true,
            timestamp_format: TimestampFormat::default(),
            session_retention_days: default_retention(),
            theme: default_theme(),
            check_updates: default_check_updates(),
            skipped_version: None,
        }
    }
}

/// A named serial-port configuration preset, reusable across devices. Saved and
/// loaded from the new-connection dialog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedConfig {
    pub name: String,
    #[serde(flatten)]
    pub config: PortConfig,
}

/// A connection that was open when the app last exited, so it can be reopened on
/// the next launch (remembered session).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub identity: PortIdentity,
    /// Optional user-assigned name shown in the tab and merged view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub config: PortConfig,
}

/// Top-level config, matching the TOML layout in spec §7.14.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    /// Global highlight rules applied to every connection (spec §7.9).
    #[serde(default)]
    pub highlight: Vec<HighlightRule>,
    /// Named port-config presets for the new-connection dialog.
    #[serde(default, rename = "preset")]
    pub presets: Vec<NamedConfig>,
    /// Connections open at last exit, reopened on the next launch.
    #[serde(default, rename = "last_open")]
    pub last_open: Vec<SavedConnection>,
}

impl Config {
    /// Parse from a TOML string.
    pub fn from_toml(s: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a TOML string.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

/// A fresh config (no file on disk yet, or an unreadable one) starts with a
/// starter set of highlight rules — ERROR/WARNING/INFO — left *disabled* so
/// they're discoverable without changing anyone's log colours unasked.
///
/// This is intentionally *not* wired up as `highlight`'s `#[serde(default)]`:
/// that path also fires when parsing a config that has zero highlights
/// because the user cleared them, and an empty `Vec` serializes identically
/// to an absent key, so that would resurrect these rules on every load and
/// break the (de)serialization round-trip.
impl Default for Config {
    fn default() -> Config {
        Config {
            settings: Settings::default(),
            highlight: default_highlight_rules(),
            presets: Vec::new(),
            last_open: Vec::new(),
        }
    }
}

fn default_highlight_rules() -> Vec<HighlightRule> {
    vec![
        HighlightRule {
            pattern: "ERROR".into(),
            color: "#ff5555".into(),
            case_sensitive: false,
            bold: false,
            enabled: false,
        },
        HighlightRule {
            pattern: "WARNING".into(),
            color: "#e5c040".into(),
            case_sensitive: false,
            bold: false,
            enabled: false,
        },
        HighlightRule {
            pattern: "INFO".into(),
            color: "#6cb6ff".into(),
            case_sensitive: false,
            bold: false,
            enabled: false,
        },
    ]
}

fn default_true() -> bool {
    true
}
fn default_max_lines() -> usize {
    1_000_000
}
/// Matches egui's own monospace text style, so an existing install looks
/// unchanged until the size is touched.
fn default_console_font_size() -> u8 {
    12
}
fn default_retention() -> u32 {
    30
}
fn default_theme() -> String {
    "dark".to_string()
}
fn default_check_updates() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn databits_roundtrip_toml() {
        let cfg = PortConfig::default();
        let s = toml::to_string(&cfg).unwrap();
        let back: PortConfig = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn databits_rejects_bad_value() {
        assert!(DataBits::try_from(9).is_err());
        assert!(StopBits::try_from(3).is_err());
    }

    #[test]
    fn config_summary() {
        let cfg = PortConfig::default();
        assert_eq!(cfg.summary(), "115200 8N1");
    }

    #[test]
    fn fresh_config_seeds_disabled_error_warning_info_highlights() {
        let cfg = Config::default();
        let patterns: Vec<&str> = cfg.highlight.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(patterns, vec!["ERROR", "WARNING", "INFO"]);
        assert!(
            cfg.highlight.iter().all(|r| !r.enabled),
            "seeded highlight rules must start disabled"
        );
    }

    /// The bold checkbox is gone from the UI (issue #45: nothing could draw a
    /// bold run), but the field stays — a config written while it existed has
    /// to keep loading, and the setting has to survive the next save rather
    /// than being silently dropped, in case a bold face is ever added.
    #[test]
    fn a_config_written_with_bold_highlights_still_loads_and_keeps_them() {
        // `r##` not `r#`: the colour literal contains `"#`.
        let cfg = Config::from_toml(
            r##"
[[highlight]]
pattern = "PANIC"
color = "#ff0000"
bold = true
enabled = true
"##,
        )
        .expect("a config from before the checkbox was removed still parses");
        assert_eq!(cfg.highlight.len(), 1);
        assert!(
            cfg.highlight[0].bold,
            "the setting is preserved, not dropped"
        );

        let back = Config::from_toml(&cfg.to_toml().unwrap()).unwrap();
        assert!(
            back.highlight[0].bold,
            "and survives a round trip through save"
        );
    }

    #[test]
    fn highlight_case_sensitivity_is_backward_compatible_and_round_trips() {
        let old = Config::from_toml(
            r##"
[[highlight]]
pattern = "ERROR"
color = "#ff0000"
enabled = true
"##,
        )
        .expect("a config from before case sensitivity was added still parses");
        assert!(
            !old.highlight[0].case_sensitive,
            "existing highlights keep their case-insensitive behaviour"
        );

        let mut cfg = old;
        cfg.highlight[0].case_sensitive = true;
        let back = Config::from_toml(&cfg.to_toml().unwrap()).unwrap();
        assert!(back.highlight[0].case_sensitive);
    }

    #[test]
    fn config_with_explicitly_empty_highlights_round_trips_empty() {
        // An empty `Vec` and an absent `[[highlight]]` key look identical on
        // the wire, so parsing must not resurrect the seeded defaults here —
        // only `Config::default()` (no file / unreadable file) should.
        let mut cfg = Config::default();
        cfg.highlight.clear();
        let s = cfg.to_toml().unwrap();
        let back = Config::from_toml(&s).unwrap();
        assert!(back.highlight.is_empty());
    }

    #[test]
    fn update_keys_absent_means_checking_is_on() {
        // A config written before these keys existed must still get the startup
        // check, and must not look like it has already skipped a version.
        let settings: Settings = toml::from_str("max_lines = 1000").unwrap();
        assert!(settings.check_updates);
        assert_eq!(settings.skipped_version, None);
    }

    #[test]
    fn console_font_size_absent_means_the_default() {
        let settings: Settings = toml::from_str("max_lines = 1000").unwrap();
        assert_eq!(settings.console_font_size, default_console_font_size());
        let back: Settings = toml::from_str(
            &toml::to_string(&Settings {
                console_font_size: 20,
                ..Settings::default()
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(back.console_font_size, 20);
    }

    #[test]
    fn skipped_version_and_opt_out_round_trip() {
        let mut cfg = Config::default();
        cfg.settings.check_updates = false;
        cfg.settings.skipped_version = Some("v0.2.0".into());
        let back = Config::from_toml(&cfg.to_toml().unwrap()).unwrap();
        assert!(!back.settings.check_updates);
        assert_eq!(back.settings.skipped_version.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn parses_spec_example_config() {
        let toml_src = r##"
[settings]
max_lines = 1000000
timestamp_format = "delta"
session_retention_days = 30
theme = "dark"

[[preset]]
name = "fast"
baud = 921600
data_bits = 8
parity = "none"
stop_bits = 1
flow_control = "none"
dtr_on_open = true
rts_on_open = false

[[highlight]]
pattern = "ERROR|FATAL"
color = "#ff5555"
bold = true
"##;
        let cfg = Config::from_toml(toml_src).expect("parse");
        assert_eq!(cfg.settings.timestamp_format, TimestampFormat::Delta);
        assert_eq!(cfg.settings.max_lines, 1_000_000);
        assert_eq!(cfg.presets.len(), 1);
        assert_eq!(cfg.presets[0].config.baud, 921_600);
        assert_eq!(cfg.highlight.len(), 1);
    }

    #[test]
    fn config_roundtrips() {
        let cfg = Config {
            settings: Settings::default(),
            highlight: vec![],
            presets: vec![NamedConfig {
                name: "fast".into(),
                config: PortConfig {
                    baud: 921_600,
                    ..PortConfig::default()
                },
            }],
            last_open: vec![SavedConnection {
                identity: PortIdentity {
                    vid: Some(3),
                    pid: Some(4),
                    ..Default::default()
                },
                name: Some("debug probe".into()),
                config: PortConfig::default(),
            }],
        };
        let s = cfg.to_toml().unwrap();
        let back = Config::from_toml(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn saved_connection_name_is_backward_compatible() {
        let cfg = Config::from_toml(
            r#"
                [[last_open]]
                baud = 115200
                data_bits = 8
                parity = "none"
                stop_bits = 1
                flow_control = "none"
                dtr_on_open = true
                rts_on_open = false
                [last_open.identity]
                path_fallback = "/dev/ttyS0"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.last_open.len(), 1);
        assert_eq!(cfg.last_open[0].name, None);
    }
}
