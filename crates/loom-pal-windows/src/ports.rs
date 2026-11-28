//! On-demand TCP/UDP listening port enumeration via dynamically loaded `iphlpapi.dll`.
//!
//! Uses `GetExtendedTcpTable` (`TCP_TABLE_OWNER_PID_ALL`) and `GetExtendedUdpTable`
//! (`UDP_TABLE_OWNER_PID`) across both `AF_INET` and `AF_INET6` without WMI/PDH or
//! extra external crates.

use std::ffi::c_void;
use std::mem::{size_of, transmute};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ptr;
use std::sync::Arc;

use loom_pal_api::PalError;
use loom_types::{ProcessSocketRecord, SocketProtocol};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows_sys::core::BOOL;

use crate::handle::{OwnedModule, map_win32_error};

/// IPv4 address family (`AF_INET`).
const AF_INET: u32 = 2;
/// IPv6 address family (`AF_INET6`).
const AF_INET6: u32 = 23;

/// `TCP_TABLE_CLASS::TCP_TABLE_OWNER_PID_ALL` (`tcpmib.h`).
const TCP_TABLE_OWNER_PID_ALL: u32 = 5;
/// `UDP_TABLE_CLASS::UDP_TABLE_OWNER_PID` (`udpmib.h`).
const UDP_TABLE_OWNER_PID: u32 = 1;

/// `MIB_TCP_STATE::MIB_TCP_STATE_LISTEN` (`tcpmib.h`).
const MIB_TCP_STATE_LISTEN: u32 = 2;

/// Upper bound on table allocation (16 MiB) to guard against pathological responses.
const MAX_TABLE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum retry iterations on `ERROR_INSUFFICIENT_BUFFER`.
const MAX_RETRIES: usize = 4;

type GetExtendedTcpTableFn = unsafe extern "system" fn(
    p_tcp_table: *mut c_void,
    pdw_size: *mut u32,
    b_order: BOOL,
    ul_af: u32,
    table_class: u32,
    reserved: u32,
) -> u32;

type GetExtendedUdpTableFn = unsafe extern "system" fn(
    p_udp_table: *mut c_void,
    pdw_size: *mut u32,
    b_order: BOOL,
    ul_af: u32,
    table_class: u32,
    reserved: u32,
) -> u32;

/// `MIB_TCPROW_OWNER_PID` (`tcpmib.h`, 24 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct MibTcpRowOwnerPid {
    dw_state: u32,
    dw_local_addr: u32,
    dw_local_port: u32,
    dw_remote_addr: u32,
    dw_remote_port: u32,
    dw_owning_pid: u32,
}

/// `MIB_TCP6ROW_OWNER_PID` (`tcpmib.h`, 56 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct MibTcp6RowOwnerPid {
    uc_local_addr: [u8; 16],
    dw_local_scope_id: u32,
    dw_local_port: u32,
    uc_remote_addr: [u8; 16],
    dw_remote_scope_id: u32,
    dw_remote_port: u32,
    dw_state: u32,
    dw_owning_pid: u32,
}

/// `MIB_UDPROW_OWNER_PID` (`udpmib.h`, 12 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct MibUdpRowOwnerPid {
    dw_local_addr: u32,
    dw_local_port: u32,
    dw_owning_pid: u32,
}

/// `MIB_UDP6ROW_OWNER_PID` (`udpmib.h`, 28 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct MibUdp6RowOwnerPid {
    uc_local_addr: [u8; 16],
    dw_local_scope_id: u32,
    dw_local_port: u32,
    dw_owning_pid: u32,
}

/// Lists all listening TCP (`LISTEN`) and bound UDP endpoints across IPv4 and IPv6.
///
/// Results are sorted deterministically by `(local_port, protocol, pid, local_addr)`.
pub fn list_listening_ports() -> Result<Vec<ProcessSocketRecord>, PalError> {
    let iphlpapi = OwnedModule::load("iphlpapi.dll")?;

    let tcp_addr = iphlpapi
        .proc_addr(b"GetExtendedTcpTable\0")
        .ok_or(PalError::Unsupported(
            "GetExtendedTcpTable is unavailable in iphlpapi.dll",
        ))?;
    let udp_addr = iphlpapi
        .proc_addr(b"GetExtendedUdpTable\0")
        .ok_or(PalError::Unsupported(
            "GetExtendedUdpTable is unavailable in iphlpapi.dll",
        ))?;

    // SAFETY: Function signatures match `iphlpapi.h` definitions for `GetExtendedTcpTable` and `GetExtendedUdpTable`.
    let get_tcp_table: GetExtendedTcpTableFn = unsafe { transmute(tcp_addr) };
    let get_udp_table: GetExtendedUdpTableFn = unsafe { transmute(udp_addr) };

    let mut records = Vec::with_capacity(64);
    let state_listen: Arc<str> = Arc::from("LISTEN");
    let state_udp: Arc<str> = Arc::from("UDP");

    // 1. IPv4 TCP listening endpoints.
    if let Some(buf) = query_iphlpapi_table(
        get_tcp_table,
        AF_INET,
        TCP_TABLE_OWNER_PID_ALL,
        "GetExtendedTcpTable",
    )? {
        for_each_table_row::<MibTcpRowOwnerPid>(&buf, |row| {
            if row.dw_state != MIB_TCP_STATE_LISTEN {
                return;
            }
            records.push(ProcessSocketRecord {
                protocol: SocketProtocol::Tcp,
                local_addr: Arc::from(decode_ipv4(row.dw_local_addr)),
                local_port: decode_port(row.dw_local_port),
                remote_addr: None,
                remote_port: None,
                state: Arc::clone(&state_listen),
                pid: row.dw_owning_pid,
            });
        });
    }

    // 2. IPv6 TCP listening endpoints.
    if let Some(buf) = query_iphlpapi_table(
        get_tcp_table,
        AF_INET6,
        TCP_TABLE_OWNER_PID_ALL,
        "GetExtendedTcpTable",
    )? {
        for_each_table_row::<MibTcp6RowOwnerPid>(&buf, |row| {
            if row.dw_state != MIB_TCP_STATE_LISTEN {
                return;
            }
            records.push(ProcessSocketRecord {
                protocol: SocketProtocol::Tcp6,
                local_addr: Arc::from(decode_ipv6(row.uc_local_addr)),
                local_port: decode_port(row.dw_local_port),
                remote_addr: None,
                remote_port: None,
                state: Arc::clone(&state_listen),
                pid: row.dw_owning_pid,
            });
        });
    }

    // 3. IPv4 UDP bound endpoints.
    if let Some(buf) = query_iphlpapi_table(
        get_udp_table,
        AF_INET,
        UDP_TABLE_OWNER_PID,
        "GetExtendedUdpTable",
    )? {
        for_each_table_row::<MibUdpRowOwnerPid>(&buf, |row| {
            records.push(ProcessSocketRecord {
                protocol: SocketProtocol::Udp,
                local_addr: Arc::from(decode_ipv4(row.dw_local_addr)),
                local_port: decode_port(row.dw_local_port),
                remote_addr: None,
                remote_port: None,
                state: Arc::clone(&state_udp),
                pid: row.dw_owning_pid,
            });
        });
    }

    // 4. IPv6 UDP bound endpoints.
    if let Some(buf) = query_iphlpapi_table(
        get_udp_table,
        AF_INET6,
        UDP_TABLE_OWNER_PID,
        "GetExtendedUdpTable",
    )? {
        for_each_table_row::<MibUdp6RowOwnerPid>(&buf, |row| {
            records.push(ProcessSocketRecord {
                protocol: SocketProtocol::Udp6,
                local_addr: Arc::from(decode_ipv6(row.uc_local_addr)),
                local_port: decode_port(row.dw_local_port),
                remote_addr: None,
                remote_port: None,
                state: Arc::clone(&state_udp),
                pid: row.dw_owning_pid,
            });
        });
    }

    records.sort_by(|a, b| {
        (a.local_port, a.protocol, a.pid, a.local_addr.as_ref()).cmp(&(
            b.local_port,
            b.protocol,
            b.pid,
            b.local_addr.as_ref(),
        ))
    });
    records.dedup();
    Ok(records)
}

fn query_iphlpapi_table(
    query_fn: GetExtendedTcpTableFn,
    af: u32,
    table_class: u32,
    api_name: &str,
) -> Result<Option<Vec<u32>>, PalError> {
    let mut size: u32 = 0;
    // SAFETY: Null buffer queries required byte count.
    let rc = unsafe { query_fn(ptr::null_mut(), &mut size, 0, af, table_class, 0) };
    if rc == NO_ERROR || size == 0 {
        return Ok(None);
    }
    if rc != ERROR_INSUFFICIENT_BUFFER {
        // IPv6 may be disabled on some hosts; treat unsupported address family gracefully.
        if af == AF_INET6 {
            return Ok(None);
        }
        return Err(map_win32_error(rc, None, api_name));
    }

    for _ in 0..MAX_RETRIES {
        let alloc_bytes = (size as usize).min(MAX_TABLE_BYTES);
        if alloc_bytes < size_of::<u32>() {
            return Ok(None);
        }
        let mut buf = vec![0u32; alloc_bytes.div_ceil(size_of::<u32>())];
        let mut buf_bytes = (buf.len() * size_of::<u32>()) as u32;
        // SAFETY: `buf` is 4-byte aligned and writable for `buf_bytes`.
        let rc = unsafe {
            query_fn(
                buf.as_mut_ptr().cast::<c_void>(),
                &mut buf_bytes,
                0,
                af,
                table_class,
                0,
            )
        };
        if rc == NO_ERROR {
            return Ok(Some(buf));
        }
        if rc == ERROR_INSUFFICIENT_BUFFER && (buf_bytes as usize) <= MAX_TABLE_BYTES {
            size = buf_bytes;
            continue;
        }
        if af == AF_INET6 {
            return Ok(None);
        }
        return Err(map_win32_error(rc, None, api_name));
    }

    Ok(None)
}

/// Visits each `T` row in a `MIB_*TABLE_OWNER_PID` buffer (`dwNumEntries: u32` followed by `T` rows)
/// without allocating an intermediate `Vec<T>`.
fn for_each_table_row<T: Copy>(buf: &[u32], mut visit: impl FnMut(T)) {
    if buf.is_empty() {
        return;
    }
    let count = buf[0] as usize;
    let payload_bytes = (buf.len() - 1) * size_of::<u32>();
    let max_fit = payload_bytes / size_of::<T>();
    let n = count.min(max_fit);
    // SAFETY: `buf[1..]` is 4-byte aligned and has at least `n * size_of::<T>()` readable bytes.
    let base_ptr = unsafe { buf.as_ptr().add(1).cast::<T>() };
    for i in 0..n {
        // SAFETY: `i < n <= max_fit`, so reading `T` at `base_ptr.add(i)` is strictly in bounds.
        let row = unsafe { ptr::read_unaligned(base_ptr.add(i)) };
        visit(row);
    }
}

/// Decodes a Windows MIB port word (network byte order stored in the low 16 bits of a `u32`).
#[inline]
pub(crate) fn decode_port(raw_port: u32) -> u16 {
    u16::from_be((raw_port & 0xFFFF) as u16)
}

/// Formats a Windows MIB IPv4 address (`u32` in network byte order in memory) into dotted-quad text.
#[inline]
pub(crate) fn decode_ipv4(raw_addr: u32) -> String {
    Ipv4Addr::from(raw_addr.to_ne_bytes()).to_string()
}

/// Formats a 16-byte IPv6 address into canonical RFC 5952 text.
#[inline]
pub(crate) fn decode_ipv6(raw_addr: [u8; 16]) -> String {
    Ipv6Addr::from(raw_addr).to_string()
}

#[cfg(test)]
mod tests {
    use std::mem::{align_of, size_of};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn abi_mib_row_layouts_match_windows_sdk() {
        assert_eq!(size_of::<MibTcpRowOwnerPid>(), 24);
        assert_eq!(align_of::<MibTcpRowOwnerPid>(), 4);
        assert_eq!(size_of::<MibTcp6RowOwnerPid>(), 56);
        assert_eq!(align_of::<MibTcp6RowOwnerPid>(), 4);
        assert_eq!(size_of::<MibUdpRowOwnerPid>(), 12);
        assert_eq!(align_of::<MibUdpRowOwnerPid>(), 4);
        assert_eq!(size_of::<MibUdp6RowOwnerPid>(), 28);
        assert_eq!(align_of::<MibUdp6RowOwnerPid>(), 4);
    }

    #[test]
    fn ipv4_ipv6_and_port_decoding_match_network_byte_order() {
        // Port 8080 (0x1F90) in network byte order stored in low 16 bits -> 0x901F.
        assert_eq!(decode_port(u32::from(8080u16.to_be())), 8080);
        assert_eq!(decode_port(u32::from(443u16.to_be())), 443);

        // 127.0.0.1 in memory bytes [127, 0, 0, 1].
        let loopback = u32::from_ne_bytes([127, 0, 0, 1]);
        assert_eq!(decode_ipv4(loopback), "127.0.0.1");
        assert_eq!(decode_ipv4(0), "0.0.0.0");

        let mut v6_loopback = [0u8; 16];
        v6_loopback[15] = 1;
        assert_eq!(decode_ipv6(v6_loopback), "::1");
    }

    #[test]
    fn list_listening_ports_detects_live_ephemeral_tcp_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback port");
        let bound_port = listener.local_addr().expect("local_addr").port();
        let my_pid = std::process::id();

        let ports = list_listening_ports().expect("list_listening_ports");
        let found = ports.iter().find(|r| {
            r.pid == my_pid && r.local_port == bound_port && r.protocol == SocketProtocol::Tcp
        });
        let record = found.expect("current test process ephemeral TCP listener must be reported");
        assert_eq!(record.local_addr.as_ref(), "127.0.0.1");
        assert_eq!(record.state.as_ref(), "LISTEN");
        assert!(record.remote_addr.is_none());
        assert!(record.remote_port.is_none());
        drop(listener);
    }
}
