//! User preference, view mode, sorting, and display enums shared across config, engine, and UI.

use serde::{Deserialize, Serialize};

/// Presentation mode of the Processes page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProcessViewMode {
    /// Three-section categorized grouping (Apps / Background / Windows).
    #[default]
    Grouped,
    /// Verified parent-child process tree view.
    Tree,
}

/// Presentation mode of the Details page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DetailsViewMode {
    /// Flat sortable process table.
    #[default]
    Flat,
    /// Verified parent-child process tree with full detail columns.
    Tree,
}

/// Sortable columns in the Processes table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProcessSortColumn {
    /// Process / application display name.
    Name,
    /// Process grouping category (`App` / `Background` / `System`).
    Category,
    /// Numeric process identifier.
    Pid,
    /// Raw executable/process image name.
    ProcessName,
    /// Process status flags.
    Status,
    /// CPU utilization.
    #[default]
    Cpu,
    /// Memory footprint.
    Memory,
    /// Disk I/O throughput.
    Disk,
    /// Network throughput.
    Network,
    /// GPU utilization / memory.
    Gpu,
    /// Binary publisher / company name.
    Publisher,
    /// Full process command line string.
    CommandLine,
}

/// Sortable columns in the Details table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DetailsSortColumn {
    /// Process executable name.
    #[default]
    Name,
    /// Numeric process identifier.
    Pid,
    /// Dynamic process status.
    Status,
    /// Owning user account name.
    UserName,
    /// OS login session identifier.
    SessionId,
    /// CPU utilization.
    Cpu,
    /// Cumulative CPU time (kernel + user).
    CpuTime,
    /// Private working set memory.
    Memory,
    /// Total resident working set memory.
    WorkingSet,
    /// Signed change in total resident working set memory since the previous tick.
    WorkingSetDelta,
    /// Private committed virtual memory.
    CommitSize,
    /// Scheduling priority class.
    Priority,
    /// Open kernel handle / file descriptor count.
    Handles,
    /// Active OS thread count.
    Threads,
    /// Process CPU architecture.
    Arch,
    /// UAC virtualization state.
    UacVirtualization,
    /// Process token elevation status.
    Elevated,
    /// Dedicated GPU memory.
    GpuMemory,
    /// Binary publisher / company name.
    Publisher,
    /// Binary file description.
    Description,
    /// Full executable image path.
    ExePath,
    /// Full command line string.
    CommandLine,
}

/// Sortable columns in the Services table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServicesSortColumn {
    /// Internal service name.
    #[default]
    Name,
    /// Hosting process identifier.
    Pid,
    /// Service description or display name.
    Description,
    /// Service lifecycle status.
    Status,
    /// Service load-order or host group.
    Group,
}

/// Sortable columns in the Startup Applications table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StartupSortColumn {
    /// Display name of the startup application.
    #[default]
    Name,
    /// Publisher or vendor name.
    Publisher,
    /// Enabled or disabled status.
    Status,
    /// Estimated boot latency impact.
    Impact,
}

/// Sortable columns in the Users table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UsersSortColumn {
    /// User account or process display name.
    User,
    /// Session connection state or process running status.
    Status,
    /// Aggregated or per-process CPU utilization.
    #[default]
    Cpu,
    /// Aggregated or per-process memory usage.
    Memory,
    /// Aggregated or per-process disk I/O throughput.
    Disk,
    /// Aggregated or per-process network throughput.
    Network,
}

/// Sortable columns in the App History table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppHistorySortColumn {
    /// Application display name.
    Name,
    /// Cumulative CPU time.
    #[default]
    CpuTime,
    /// Cumulative total network traffic.
    Network,
    /// Cumulative metered network traffic.
    MeteredNetwork,
}

/// Sort ordering direction for tables.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortDirection {
    /// Smallest / A-to-Z first.
    Ascending,
    /// Largest / Z-to-A first.
    #[default]
    Descending,
}

impl SortDirection {
    /// Returns the opposite sort direction.
    #[inline]
    pub const fn toggle(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

/// Formatting mode for resource columns (memory, disk, network) in the Processes table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResourceDisplayMode {
    /// Display absolute units (e.g., `128.4 MB`, `12.0 MB/s`).
    #[default]
    Values,
    /// Display percentages of total system capacity (e.g., `4.2%`).
    Percent,
}

/// Real-time sampling cadence for the fast-lane engine thread.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefreshSpeed {
    /// High frequency: `500 ms` per tick.
    High500Ms,
    /// Normal default frequency: `1000 ms` per tick.
    #[default]
    Normal1S,
    /// Low frequency: `2000 ms` per tick.
    Low2S,
    /// Fast sampling timer paused until resumed by user.
    Paused,
}

impl RefreshSpeed {
    /// Returns the tick interval in milliseconds, or `None` when [`RefreshSpeed::Paused`].
    #[inline]
    pub const fn interval_ms(self) -> Option<u64> {
        match self {
            Self::High500Ms => Some(500),
            Self::Normal1S => Some(1_000),
            Self::Low2S => Some(2_000),
            Self::Paused => None,
        }
    }
}

/// Application visual theme preference.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThemePreference {
    /// Follow the operating-system light/dark theme automatically.
    #[default]
    System,
    /// Force light theme.
    Light,
    /// Force dark theme.
    Dark,
}

/// Application theme accent color preference.
///
/// Defaults to [`AccentColorPreference::System`], following the operating system's
/// active accent color. When set to [`AccentColorPreference::Custom`], Loom derives
/// its primary and highlight tokens from the chosen sRGB color.
///
/// Serializes as `"System"` or uppercase `"#RRGGBB"` in `settings.toml`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccentColorPreference {
    /// Follow the operating-system accent color automatically.
    #[default]
    System,
    /// Use an explicit user-selected sRGB accent color.
    Custom {
        /// Red channel (`0..=255`).
        r: u8,
        /// Green channel (`0..=255`).
        g: u8,
        /// Blue channel (`0..=255`).
        b: u8,
    },
}

impl AccentColorPreference {
    /// Constructs a custom sRGB accent color preference.
    #[inline]
    pub const fn custom(r: u8, g: u8, b: u8) -> Self {
        Self::Custom { r, g, b }
    }

    /// Returns `true` when following the operating-system accent color.
    #[inline]
    pub const fn is_system(self) -> bool {
        matches!(self, Self::System)
    }

    /// Returns the explicit `(r, g, b)` tuple when set to [`Self::Custom`].
    #[inline]
    pub const fn rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Self::System => None,
            Self::Custom { r, g, b } => Some((r, g, b)),
        }
    }

    /// Formats an `(r, g, b)` tuple as an uppercase `#RRGGBB` hex string.
    #[inline]
    pub fn format_hex(r: u8, g: u8, b: u8) -> String {
        format!("#{r:02X}{g:02X}{b:02X}")
    }

    /// Returns the uppercase `#RRGGBB` hex string when set to [`Self::Custom`].
    #[inline]
    pub fn to_hex(self) -> Option<String> {
        self.rgb().map(|(r, g, b)| Self::format_hex(r, g, b))
    }

    /// Parses a `#RRGGBB`, `RRGGBB`, `#RGB`, or `RGB` hex color string into `(r, g, b)`.
    pub fn parse_hex(raw: &str) -> Option<(u8, u8, u8)> {
        let trimmed = raw.trim();
        let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
        let bytes = hex.as_bytes();
        match bytes.len() {
            6 if bytes.iter().all(u8::is_ascii_hexdigit) => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some((r, g, b))
            }
            3 if bytes.iter().all(u8::is_ascii_hexdigit) => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
                Some((r, g, b))
            }
            _ => None,
        }
    }
}

impl Serialize for AccentColorPreference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        match *self {
            Self::System => serializer.serialize_str("System"),
            Self::Custom { r, g, b } => {
                let buf = [
                    b'#',
                    HEX[usize::from(r >> 4)],
                    HEX[usize::from(r & 0x0F)],
                    HEX[usize::from(g >> 4)],
                    HEX[usize::from(g & 0x0F)],
                    HEX[usize::from(b >> 4)],
                    HEX[usize::from(b & 0x0F)],
                ];
                let hex = std::str::from_utf8(&buf).unwrap_or("#000000");
                serializer.serialize_str(hex)
            }
        }
    }
}

impl<'de> Deserialize<'de> for AccentColorPreference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AccentColorVisitor;

        impl serde::de::Visitor<'_> for AccentColorVisitor {
            type Value = AccentColorPreference;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("\"System\" or a hex color such as \"#0078D4\"")
            }

            fn visit_str<E>(self, raw: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let trimmed = raw.trim();
                if trimmed.eq_ignore_ascii_case("system") {
                    return Ok(AccentColorPreference::System);
                }
                AccentColorPreference::parse_hex(trimmed)
                    .map(|(r, g, b)| AccentColorPreference::Custom { r, g, b })
                    .ok_or_else(|| {
                        E::custom(format!(
                            "expected \"System\" or a hex color such as \"#0078D4\", found \"{raw}\""
                        ))
                    })
            }
        }

        deserializer.deserialize_str(AccentColorVisitor)
    }
}

/// Application UI localization preference.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguagePreference {
    /// Simplified Chinese (`zh-CN`).
    #[default]
    ZhCn,
    /// English (`en-US`).
    EnUs,
}

impl LanguagePreference {
    /// Returns the BCP-47 locale tag string (`"zh-CN"` or `"en-US"`).
    #[inline]
    pub const fn locale_tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }
}

/// Default navigation page opened when Loom launches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StartupPage {
    /// Processes view.
    #[default]
    Processes,
    /// Performance monitoring view.
    Performance,
    /// Application usage history view.
    AppHistory,
    /// Startup applications view.
    Startup,
    /// Logged-in users and sessions view.
    Users,
    /// Full process details view.
    Details,
    /// System services view.
    Services,
}
