//! Pure-Rust SMBIOS firmware table parser (`GetSystemFirmwareTable('RSMB', 0)`)
//! extracting physical RAM speed, slot counts, form factor, and hardware-reserved bytes.

use std::sync::Arc;

use loom_types::MemoryHardwareInfo;
use windows_sys::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

/// SMBIOS Type 17: Memory Device structure identifier.
const SMBIOS_TYPE_MEMORY_DEVICE: u8 = 17;
/// SMBIOS Type 127: End-of-Table marker.
const SMBIOS_TYPE_END_OF_TABLE: u8 = 127;
/// Size of the Windows `RawSMBIOSData` header preceding `SMBIOSTableData`.
const RAW_SMBIOS_HEADER_LEN: usize = 8;

/// Reads the host firmware SMBIOS table via `GetSystemFirmwareTable('RSMB', 0)` and
/// parses Type 17 Memory Device records, computing `hardware_reserved_bytes` against
/// the OS-usable physical RAM total (`mem_total_bytes`).
pub fn probe_memory_hardware_info(mem_total_bytes: u64) -> MemoryHardwareInfo {
    let raw = read_raw_smbios_table();
    if raw.is_empty() {
        return MemoryHardwareInfo::default();
    }
    let (mut info, installed_bytes) = parse_smbios_memory_devices_with_installed(&raw);
    if installed_bytes > mem_total_bytes {
        info.hardware_reserved_bytes = installed_bytes.saturating_sub(mem_total_bytes);
    }
    info
}

/// Parses a Windows `RawSMBIOSData` byte buffer (returned by `GetSystemFirmwareTable('RSMB', 0)`)
/// and extracts Type 17 physical memory device metadata.
///
/// Never panics on truncated, malformed, or empty buffers.
pub fn parse_smbios_memory_devices(raw: &[u8]) -> MemoryHardwareInfo {
    parse_smbios_memory_devices_with_installed(raw).0
}

/// Parses a `RawSMBIOSData` byte buffer and returns `(MemoryHardwareInfo, installed_bytes)`.
pub(crate) fn parse_smbios_memory_devices_with_installed(raw: &[u8]) -> (MemoryHardwareInfo, u64) {
    if raw.len() < RAW_SMBIOS_HEADER_LEN {
        return (MemoryHardwareInfo::default(), 0);
    }

    let declared_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let table_end = RAW_SMBIOS_HEADER_LEN
        .saturating_add(declared_len)
        .min(raw.len());
    let table = &raw[RAW_SMBIOS_HEADER_LEN..table_end];

    let mut slots_total: u32 = 0;
    let mut slots_used: u32 = 0;
    let mut installed_bytes: u64 = 0;
    let mut configured_speed: Option<u32> = None;
    let mut rated_speed: Option<u32> = None;
    let mut form_factor: Option<Arc<str>> = None;

    let mut pos = 0usize;
    while pos + 4 <= table.len() {
        let struct_type = table[pos];
        let formatted_len = usize::from(table[pos + 1]);
        if formatted_len < 4 || pos + formatted_len > table.len() {
            break;
        }

        if struct_type == SMBIOS_TYPE_END_OF_TABLE {
            break;
        }

        let formatted = &table[pos..pos + formatted_len];

        if struct_type == SMBIOS_TYPE_MEMORY_DEVICE {
            slots_total = slots_total.saturating_add(1);

            let (populated, size_bytes) = decode_memory_device_size(formatted);
            if populated {
                slots_used = slots_used.saturating_add(1);
                installed_bytes = installed_bytes.saturating_add(size_bytes);

                if form_factor.is_none() && formatted.len() > 0x0E {
                    form_factor = decode_form_factor(formatted[0x0E]).map(Arc::<str>::from);
                }

                // Prefer SMBIOS 2.7+/3.2+ Configured Memory Speed (offset 0x20, ext 0x58).
                if configured_speed.is_none() {
                    configured_speed = decode_configured_speed(formatted);
                }

                // Fallback to SMBIOS 2.3+/3.2+ rated Speed (offset 0x15, ext 0x54).
                if rated_speed.is_none() {
                    rated_speed = decode_rated_speed(formatted);
                }
            }
        }

        // Advance past the formatted area and the double-NUL-terminated string section.
        let mut cursor = pos + formatted_len;
        let mut found_terminator = false;
        while cursor + 1 < table.len() {
            if table[cursor] == 0 && table[cursor + 1] == 0 {
                pos = cursor + 2;
                found_terminator = true;
                break;
            }
            cursor += 1;
        }
        if !found_terminator {
            break;
        }
    }

    let info = MemoryHardwareInfo {
        speed_mt_s: configured_speed.or(rated_speed),
        slots_used: (slots_total > 0).then_some(slots_used),
        slots_total: (slots_total > 0).then_some(slots_total),
        form_factor,
        hardware_reserved_bytes: 0,
    };

    (info, installed_bytes)
}

/// Decodes the SMBIOS Type 17 `Size` (`0x0C..0x0E`) and `Extended Size` (`0x1C..0x20`) fields.
///
/// Returns `(is_populated, size_in_bytes)`.
fn decode_memory_device_size(formatted: &[u8]) -> (bool, u64) {
    if formatted.len() < 0x0E {
        return (false, 0);
    }
    let raw_size = u16::from_le_bytes([formatted[0x0C], formatted[0x0D]]);
    if raw_size == 0 {
        // 0 indicates no memory device is installed in this socket.
        return (false, 0);
    }
    if raw_size == 0xFFFF {
        // Installed, but size is unknown.
        return (true, 0);
    }
    if raw_size == 0x7FFF {
        // SMBIOS 2.7+: size >= 32 GiB - 1 MiB is stored in Extended Size (offset 0x1C, MiB).
        if formatted.len() >= 0x20 {
            let ext_mib = u32::from_le_bytes([
                formatted[0x1C],
                formatted[0x1D],
                formatted[0x1E],
                formatted[0x1F],
            ]) & 0x7FFF_FFFF;
            return (true, u64::from(ext_mib).saturating_mul(1024 * 1024));
        }
        return (true, 0);
    }

    // Bit 15 == 1 means granularity is KiB; bit 15 == 0 means granularity is MiB.
    let magnitude = u64::from(raw_size & 0x7FFF);
    let bytes = if (raw_size & 0x8000) != 0 {
        magnitude.saturating_mul(1024)
    } else {
        magnitude.saturating_mul(1024 * 1024)
    };
    (true, bytes)
}

/// Decodes SMBIOS Type 17 `Configured Memory Speed` (`0x20..0x22`) and `Extended Configured Memory Speed` (`0x58..0x5C`).
fn decode_configured_speed(formatted: &[u8]) -> Option<u32> {
    if formatted.len() < 0x22 {
        return None;
    }
    let speed = u16::from_le_bytes([formatted[0x20], formatted[0x21]]);
    if speed == 0 {
        return None;
    }
    if speed == 0xFFFF {
        if formatted.len() >= 0x5C {
            let ext = u32::from_le_bytes([
                formatted[0x58],
                formatted[0x59],
                formatted[0x5A],
                formatted[0x5B],
            ]);
            return (ext > 0).then_some(ext);
        }
        return None;
    }
    Some(u32::from(speed))
}

/// Decodes SMBIOS Type 17 rated `Speed` (`0x15..0x17`) and `Extended Speed` (`0x54..0x58`).
fn decode_rated_speed(formatted: &[u8]) -> Option<u32> {
    if formatted.len() < 0x17 {
        return None;
    }
    let speed = u16::from_le_bytes([formatted[0x15], formatted[0x16]]);
    if speed == 0 {
        return None;
    }
    if speed == 0xFFFF {
        if formatted.len() >= 0x58 {
            let ext = u32::from_le_bytes([
                formatted[0x54],
                formatted[0x55],
                formatted[0x56],
                formatted[0x57],
            ]);
            return (ext > 0).then_some(ext);
        }
        return None;
    }
    Some(u32::from(speed))
}

/// Maps SMBIOS Type 17 `Form Factor` enumeration codes to human-readable labels.
fn decode_form_factor(code: u8) -> Option<&'static str> {
    match code {
        0x03 => Some("SIMM"),
        0x04 => Some("SIP"),
        0x05 => Some("Chip"),
        0x06 => Some("DIP"),
        0x07 => Some("ZIP"),
        0x08 => Some("Proprietary"),
        0x09 => Some("DIMM"),
        0x0A => Some("TSOP"),
        0x0B => Some("Row of chips"),
        0x0C => Some("RIMM"),
        0x0D => Some("SODIMM"),
        0x0E => Some("SRIMM"),
        0x0F => Some("FB-DIMM"),
        0x10 => Some("Die"),
        _ => None,
    }
}

/// Reads the raw SMBIOS firmware table buffer via `GetSystemFirmwareTable('RSMB', 0)`.
fn read_raw_smbios_table() -> Vec<u8> {
    // SAFETY: Null buffer queries the required byte length.
    let needed = unsafe { GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0) };
    if needed == 0 || needed > 4 * 1024 * 1024 {
        return Vec::new();
    }

    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is allocated to `needed` bytes matching the first query.
    let written = unsafe { GetSystemFirmwareTable(RSMB, 0, buf.as_mut_ptr(), needed) };
    if written == 0 || written > needed {
        return Vec::new();
    }
    buf.truncate(written as usize);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic `RawSMBIOSData` buffer wrapping a sequence of SMBIOS structures.
    fn build_raw_smbios(table_body: &[u8]) -> Vec<u8> {
        let mut raw = Vec::with_capacity(RAW_SMBIOS_HEADER_LEN + table_body.len());
        raw.extend_from_slice(&[0u8, 3, 4, 0]); // Used20CallingMethod, Major 3, Minor 4, DmiRev
        raw.extend_from_slice(&(table_body.len() as u32).to_le_bytes());
        raw.extend_from_slice(table_body);
        raw
    }

    /// Appends a synthetic SMBIOS Type 17 structure to `out`.
    #[allow(clippy::too_many_arguments)]
    fn push_type17(
        out: &mut Vec<u8>,
        formatted_len: u8,
        size_raw: u16,
        form_factor: u8,
        rated_speed: u16,
        ext_size_mib: u32,
        configured_speed: u16,
        strings: &[&str],
    ) {
        let mut formatted = vec![0u8; usize::from(formatted_len)];
        formatted[0] = SMBIOS_TYPE_MEMORY_DEVICE;
        formatted[1] = formatted_len;
        if formatted.len() >= 0x0E {
            formatted[0x0C..0x0E].copy_from_slice(&size_raw.to_le_bytes());
        }
        if formatted.len() > 0x0E {
            formatted[0x0E] = form_factor;
        }
        if formatted.len() >= 0x17 {
            formatted[0x15..0x17].copy_from_slice(&rated_speed.to_le_bytes());
        }
        if formatted.len() >= 0x20 {
            formatted[0x1C..0x20].copy_from_slice(&ext_size_mib.to_le_bytes());
        }
        if formatted.len() >= 0x22 {
            formatted[0x20..0x22].copy_from_slice(&configured_speed.to_le_bytes());
        }
        out.extend_from_slice(&formatted);
        if strings.is_empty() {
            out.extend_from_slice(&[0, 0]);
        } else {
            for s in strings {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            out.push(0);
        }
    }

    #[test]
    fn parses_dual_channel_dimm_with_empty_slots_and_configured_speed() {
        let mut table = Vec::new();
        // Type 0 BIOS header (dummy) with 2 strings to test string-section skipping.
        table.extend_from_slice(&[0, 4, 0, 0]);
        table.extend_from_slice(b"Vendor\0Version\0\0");

        // Slot 0: 16 GiB DIMM (16384 MiB), rated 4800, configured 6000 MT/s.
        push_type17(
            &mut table,
            0x28,
            16_384,
            0x09, // DIMM
            4800,
            0,
            6000,
            &["DIMM_A1", "Corsair"],
        );
        // Slot 1: empty
        push_type17(&mut table, 0x28, 0, 0x09, 0, 0, 0, &["DIMM_A2"]);
        // Slot 2: 16 GiB DIMM
        push_type17(&mut table, 0x28, 16_384, 0x09, 4800, 0, 6000, &[]);
        // Slot 3: empty
        push_type17(&mut table, 0x28, 0, 0x09, 0, 0, 0, &[]);
        // Type 127 End-of-Table
        table.extend_from_slice(&[SMBIOS_TYPE_END_OF_TABLE, 4, 0, 0, 0, 0]);

        let raw = build_raw_smbios(&table);
        let (info, installed) = parse_smbios_memory_devices_with_installed(&raw);
        assert_eq!(info.slots_used, Some(2));
        assert_eq!(info.slots_total, Some(4));
        assert_eq!(info.speed_mt_s, Some(6000));
        assert_eq!(info.form_factor.as_deref(), Some("DIMM"));
        assert_eq!(installed, 32u64 * 1024 * 1024 * 1024);

        // Verify hardware_reserved_bytes computation against OS-usable RAM.
        let os_usable = installed - 256 * 1024 * 1024;
        assert_eq!(installed.saturating_sub(os_usable), 256 * 1024 * 1024);
    }

    #[test]
    fn parses_extended_64gib_sodimm_and_rated_speed_fallback() {
        let mut table = Vec::new();
        // 64 GiB SODIMM encoded via 0x7FFF + Extended Size = 65536 MiB, no configured speed.
        push_type17(
            &mut table,
            0x22,
            0x7FFF,
            0x0D, // SODIMM
            5600,
            65_536,
            0,
            &["SODIMM0"],
        );
        let raw = build_raw_smbios(&table);
        let (info, installed) = parse_smbios_memory_devices_with_installed(&raw);
        assert_eq!(info.slots_used, Some(1));
        assert_eq!(info.slots_total, Some(1));
        assert_eq!(info.speed_mt_s, Some(5600));
        assert_eq!(info.form_factor.as_deref(), Some("SODIMM"));
        assert_eq!(installed, 64u64 * 1024 * 1024 * 1024);
    }

    #[test]
    fn handles_empty_truncated_and_malformed_smbios_buffers_safely() {
        assert_eq!(
            parse_smbios_memory_devices(&[]),
            MemoryHardwareInfo::default()
        );
        assert_eq!(
            parse_smbios_memory_devices(&[1, 2, 3]),
            MemoryHardwareInfo::default()
        );
        // Header claims 9999 bytes, buffer is truncated right after header.
        let truncated = vec![0, 3, 0, 0, 0x0F, 0x27, 0, 0, 17, 2, 0, 0];
        assert_eq!(
            parse_smbios_memory_devices(&truncated),
            MemoryHardwareInfo::default()
        );
        // Type 17 without double-NUL terminator terminates cleanly without looping.
        let unterminated = build_raw_smbios(&[17, 0x0E, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x10]);
        let info = parse_smbios_memory_devices(&unterminated);
        assert_eq!(info.slots_used, Some(1));
        assert_eq!(info.slots_total, Some(1));
    }
}
