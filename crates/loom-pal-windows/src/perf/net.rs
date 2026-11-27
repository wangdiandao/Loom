//! Windows fast-lane network adapter telemetry via `iphlpapi.dll`
//! (`GetIfTable2` + `GetAdaptersAddresses`), free of WMI and PDH.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use loom_types::{NetAdapterSample, NetConnectionType};
use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    GetAdaptersAddresses, GetIfTable2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
    IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
};

/// `MIB_IF_ROW2.InterfaceAndOperStatusFlags` bit 0: `HardwareInterface`.
const MIB_FLAG_HARDWARE_INTERFACE: u8 = 1 << 0;
/// `MIB_IF_ROW2.InterfaceAndOperStatusFlags` bit 1: `FilterInterface`.
const MIB_FLAG_FILTER_INTERFACE: u8 = 1 << 1;

/// RAII guard for a `MIB_IF_TABLE2` pointer returned by `GetIfTable2`.
struct MibTableGuard(*mut MIB_IF_TABLE2);

impl Drop for MibTableGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was allocated by `GetIfTable2` and must be freed via `FreeMibTable`.
            unsafe {
                FreeMibTable(self.0.cast::<c_void>());
            }
        }
    }
}

/// Previous byte counters and wall timestamp per interface `LUID`.
#[derive(Debug, Clone, Copy)]
struct PrevNetCounters {
    in_octets: u64,
    out_octets: u64,
    sampled_at: Instant,
}

/// Resolved IP address and DNS metadata for a single interface `LUID`.
#[derive(Debug, Default)]
struct AdapterAddressMeta {
    friendly_name: Option<Arc<str>>,
    description: Option<Arc<str>>,
    ipv4: Option<Arc<str>>,
    ipv6: Option<Arc<str>>,
    dns_suffix: Option<Arc<str>>,
}

/// Fast-lane Windows network interface sampler owned by [`crate::WindowsFastSampler`].
pub(crate) struct NetworkPerfSampler {
    /// Reusable aligned buffer for `GetAdaptersAddresses`.
    addr_buf: Vec<u64>,
    /// Reusable `LUID -> AdapterAddressMeta` lookup map.
    addr_map: HashMap<u64, AdapterAddressMeta>,
    /// Previous `(InOctets, OutOctets, Instant)` per `InterfaceLuid`.
    prev_counters: HashMap<u64, PrevNetCounters>,
    /// Reusable set of currently observed LUIDs for stale-entry pruning.
    live_luids: HashSet<u64>,
}

impl Default for NetworkPerfSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkPerfSampler {
    /// Creates a network sampler with pre-allocated reusable buffers.
    pub(crate) fn new() -> Self {
        Self {
            addr_buf: vec![0u64; 2048], // 16 KiB initial buffer
            addr_map: HashMap::with_capacity(16),
            prev_counters: HashMap::with_capacity(16),
            live_luids: HashSet::with_capacity(16),
        }
    }

    /// Samples operational Ethernet and Wi-Fi adapters into `out`, returning aggregate
    /// `(total_recv_bytes_per_sec, total_sent_bytes_per_sec)`.
    pub(crate) fn sample(&mut self, out: &mut Vec<NetAdapterSample>) -> (u64, u64) {
        out.clear();
        self.refresh_adapter_addresses();

        let mut table_ptr: *mut MIB_IF_TABLE2 = ptr::null_mut();
        // SAFETY: `GetIfTable2` writes an OS-allocated table pointer into `table_ptr`.
        let rc = unsafe { GetIfTable2(&mut table_ptr) };
        if rc != NO_ERROR || table_ptr.is_null() {
            return (0, 0);
        }
        let guard = MibTableGuard(table_ptr);

        let now = Instant::now();
        self.live_luids.clear();

        // SAFETY: `guard.0` points to a valid `MIB_IF_TABLE2` header.
        let num_entries = unsafe { (*guard.0).NumEntries } as usize;
        let table_base = guard.0.cast::<u8>();
        let rows_offset = offset_of!(MIB_IF_TABLE2, Table);

        let mut total_recv_bps: u64 = 0;
        let mut total_sent_bps: u64 = 0;

        // First pass: hardware physical/virtual NICs (`HardwareInterface == 1 && FilterInterface == 0`).
        // If zero adapters match (e.g., unusual virtual environment), run a permissive fallback pass
        // that still excludes filter interfaces.
        for require_hardware_flag in [true, false] {
            if !require_hardware_flag && !out.is_empty() {
                break;
            }

            for i in 0..num_entries {
                // SAFETY: `GetIfTable2` allocates `NumEntries` contiguous `MIB_IF_ROW2` elements.
                let row = unsafe {
                    &*table_base
                        .add(rows_offset + i * size_of::<MIB_IF_ROW2>())
                        .cast::<MIB_IF_ROW2>()
                };

                if row.OperStatus != IfOperStatusUp {
                    continue;
                }

                let conn_type = match row.Type {
                    IF_TYPE_ETHERNET_CSMACD => NetConnectionType::Ethernet,
                    IF_TYPE_IEEE80211 => NetConnectionType::WiFi,
                    _ => continue,
                };

                let flags = row.InterfaceAndOperStatusFlags._bitfield;
                if (flags & MIB_FLAG_FILTER_INTERFACE) != 0 {
                    continue;
                }
                if require_hardware_flag && (flags & MIB_FLAG_HARDWARE_INTERFACE) == 0 {
                    continue;
                }

                // SAFETY: `NET_LUID_LH` is a C union over `Value: u64`.
                let luid = unsafe { row.InterfaceLuid.Value };
                if !self.live_luids.insert(luid) {
                    continue;
                }

                let (recv_bps, sent_bps) = compute_net_rates(
                    &mut self.prev_counters,
                    luid,
                    row.InOctets,
                    row.OutOctets,
                    now,
                );

                total_recv_bps = total_recv_bps.saturating_add(recv_bps);
                total_sent_bps = total_sent_bps.saturating_add(sent_bps);

                let addr_meta = self.addr_map.get(&luid);
                let name = addr_meta
                    .and_then(|m| m.friendly_name.as_ref().map(Arc::clone))
                    .unwrap_or_else(|| Arc::from(decode_wide_nul(&row.Alias)));
                let description = addr_meta
                    .and_then(|m| m.description.as_ref().map(Arc::clone))
                    .unwrap_or_else(|| Arc::from(decode_wide_nul(&row.Description)));

                let raw_speed = row.ReceiveLinkSpeed.max(row.TransmitLinkSpeed);
                let link_speed_bps = if raw_speed == u64::MAX { 0 } else { raw_speed };

                out.push(NetAdapterSample {
                    luid,
                    name,
                    description,
                    connection_type: conn_type,
                    link_speed_bps,
                    ipv4: addr_meta.and_then(|m| m.ipv4.as_ref().map(Arc::clone)),
                    ipv6: addr_meta.and_then(|m| m.ipv6.as_ref().map(Arc::clone)),
                    dns_suffix: addr_meta.and_then(|m| m.dns_suffix.as_ref().map(Arc::clone)),
                    recv_bytes_per_sec: recv_bps,
                    sent_bytes_per_sec: sent_bps,
                });
            }
        }

        // Prune removed LUIDs from delta history.
        let live = &self.live_luids;
        self.prev_counters.retain(|luid, _| live.contains(luid));

        // Deterministic ordering: Ethernet before Wi-Fi, then by friendly name, then LUID.
        out.sort_by(|a, b| {
            conn_sort_rank(a.connection_type)
                .cmp(&conn_sort_rank(b.connection_type))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.luid.cmp(&b.luid))
        });

        (total_recv_bps, total_sent_bps)
    }

    /// Queries `GetAdaptersAddresses` into `self.addr_buf` and populates `self.addr_map`.
    fn refresh_adapter_addresses(&mut self) {
        self.addr_map.clear();

        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        for _ in 0..3 {
            let mut buf_bytes = (self.addr_buf.len() * size_of::<u64>()) as u32;
            // SAFETY: `self.addr_buf` is 8-byte aligned and writable for `buf_bytes`.
            let rc = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC),
                    flags,
                    ptr::null(),
                    self.addr_buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                    &mut buf_bytes,
                )
            };

            if rc == ERROR_BUFFER_OVERFLOW
                && buf_bytes > 0
                && (buf_bytes as usize) <= 4 * 1024 * 1024
            {
                let needed_u64s = (buf_bytes as usize)
                    .saturating_add(1024)
                    .div_ceil(size_of::<u64>());
                self.addr_buf.resize(needed_u64s, 0);
                continue;
            }

            if rc != ERROR_SUCCESS {
                return;
            }

            let mut cursor = self.addr_buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            while !cursor.is_null() {
                // SAFETY: `GetAdaptersAddresses` guarantees linked list nodes lie inside `addr_buf`.
                let adapter = unsafe { &*cursor };
                // SAFETY: `NET_LUID_LH` is a C union over `Value: u64`.
                let luid = unsafe { adapter.Luid.Value };

                let friendly_name = decode_pwstr(adapter.FriendlyName).map(Arc::<str>::from);
                let description = decode_pwstr(adapter.Description).map(Arc::<str>::from);
                let dns_suffix = decode_pwstr(adapter.DnsSuffix)
                    .filter(|s| !s.is_empty())
                    .map(Arc::<str>::from);

                let (ipv4, ipv6) = extract_unicast_ips(adapter.FirstUnicastAddress);

                self.addr_map.insert(
                    luid,
                    AdapterAddressMeta {
                        friendly_name,
                        description,
                        ipv4,
                        ipv6,
                        dns_suffix,
                    },
                );

                cursor = adapter.Next;
            }
            return;
        }
    }
}

/// Computes `(recv_bytes_per_sec, sent_bytes_per_sec)` against the previous LUID sample.
fn compute_net_rates(
    prev_map: &mut HashMap<u64, PrevNetCounters>,
    luid: u64,
    in_octets: u64,
    out_octets: u64,
    now: Instant,
) -> (u64, u64) {
    let next = PrevNetCounters {
        in_octets,
        out_octets,
        sampled_at: now,
    };

    let Some(prev) = prev_map.insert(luid, next) else {
        return (0, 0);
    };

    let elapsed_ns = now.saturating_duration_since(prev.sampled_at).as_nanos();
    if elapsed_ns == 0 {
        return (0, 0);
    }

    let (Some(d_in), Some(d_out)) = (
        in_octets.checked_sub(prev.in_octets),
        out_octets.checked_sub(prev.out_octets),
    ) else {
        // Counter reset (e.g., NIC driver reload or interface reconnect): baseline was already
        // refreshed above, so emit zero rather than fabricating a throughput spike.
        return (0, 0);
    };

    let recv_bps =
        ((u128::from(d_in) * 1_000_000_000) / elapsed_ns).min(u128::from(u64::MAX)) as u64;
    let sent_bps =
        ((u128::from(d_out) * 1_000_000_000) / elapsed_ns).min(u128::from(u64::MAX)) as u64;

    (recv_bps, sent_bps)
}

/// Walks an `IP_ADAPTER_UNICAST_ADDRESS_LH` linked list extracting primary IPv4 and IPv6 strings.
fn extract_unicast_ips(
    first: *mut windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_UNICAST_ADDRESS_LH,
) -> (Option<Arc<str>>, Option<Arc<str>>) {
    let mut ipv4: Option<Arc<str>> = None;
    let mut ipv6: Option<Arc<str>> = None;
    let mut ipv6_link_local: Option<Arc<str>> = None;

    let mut cur = first;
    while !cur.is_null() {
        // SAFETY: Linked list produced by `GetAdaptersAddresses` remains valid inside `addr_buf`.
        let node = unsafe { &*cur };
        let sa_ptr = node.Address.lpSockaddr;
        let sa_len = node.Address.iSockaddrLength as usize;

        if !sa_ptr.is_null() && sa_len >= size_of::<u16>() {
            // SAFETY: `sa_ptr` points to a valid `SOCKADDR` with `sa_family` at offset 0.
            let family = unsafe { (*sa_ptr).sa_family };
            if family == AF_INET && sa_len >= size_of::<SOCKADDR_IN>() && ipv4.is_none() {
                // SAFETY: `family == AF_INET` and `sa_len >= size_of::<SOCKADDR_IN>()`.
                let sin = unsafe { ptr::read_unaligned(sa_ptr.cast::<SOCKADDR_IN>()) };
                // SAFETY: `IN_ADDR_0` holds `S_addr: u32` in network byte order.
                let raw_v4 = unsafe { sin.sin_addr.S_un.S_addr };
                let addr = Ipv4Addr::from(raw_v4.to_ne_bytes());
                ipv4 = Some(Arc::from(addr.to_string()));
            } else if family == AF_INET6 && sa_len >= size_of::<SOCKADDR_IN6>() && ipv6.is_none() {
                // SAFETY: `family == AF_INET6` and `sa_len >= size_of::<SOCKADDR_IN6>()`.
                let sin6 = unsafe { ptr::read_unaligned(sa_ptr.cast::<SOCKADDR_IN6>()) };
                // SAFETY: `IN6_ADDR_0` holds `Byte: [u8; 16]`.
                let octets = unsafe { sin6.sin6_addr.u.Byte };
                let addr = Ipv6Addr::from(octets);
                if (octets[0] == 0xFE) && ((octets[1] & 0xC0) == 0x80) {
                    if ipv6_link_local.is_none() {
                        ipv6_link_local = Some(Arc::from(addr.to_string()));
                    }
                } else {
                    ipv6 = Some(Arc::from(addr.to_string()));
                }
            }
        }

        cur = node.Next;
    }

    (ipv4, ipv6.or(ipv6_link_local))
}

/// Decodes a NUL-terminated UTF-16 fixed array to a trimmed `String`.
fn decode_wide_nul(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_owned()
}

/// Decodes a nullable Win32 `PWSTR` pointer to a trimmed `String`.
fn decode_pwstr(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    const MAX_LEN: usize = 512;
    while len < MAX_LEN {
        // SAFETY: `ptr` is a valid NUL-terminated UTF-16 string from `GetAdaptersAddresses`.
        if unsafe { *ptr.add(len) } == 0 {
            break;
        }
        len += 1;
    }
    if len == 0 {
        return None;
    }
    // SAFETY: Verified `len` UTF-16 code units before the NUL terminator.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf16_lossy(slice).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[inline]
fn conn_sort_rank(kind: NetConnectionType) -> u8 {
    match kind {
        NetConnectionType::Ethernet => 0,
        NetConnectionType::WiFi => 1,
        NetConnectionType::Other => 2,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn compute_net_rates_calculates_expected_throughput() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);

        assert_eq!(compute_net_rates(&mut prev, 42, 1000, 2000, t0), (0, 0));
        assert_eq!(
            compute_net_rates(&mut prev, 42, 1000 + 20_000, 2000 + 4_000, t1),
            (10_000, 2_000)
        );
    }

    #[test]
    fn compute_net_rates_returns_zero_on_counter_reset_without_spike() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t1 + Duration::from_secs(1);
        let t3 = t2 + Duration::from_secs(1);

        assert_eq!(
            compute_net_rates(&mut prev, 7, 500_000, 300_000, t0),
            (0, 0)
        );
        // Simulate NIC reconnect resetting InOctets below previous value.
        assert_eq!(
            compute_net_rates(&mut prev, 7, 1_000, 350_000, t1),
            (0, 0),
            "backward InOctets must emit 0 instead of underflowing or spiking"
        );
        // Simulate OutOctets counter reset on next tick.
        assert_eq!(
            compute_net_rates(&mut prev, 7, 2_000, 500, t2),
            (0, 0),
            "backward OutOctets must emit 0 instead of underflowing or spiking"
        );
        // Subsequent monotonic tick must resume accurate delta reporting from reset baseline.
        assert_eq!(
            compute_net_rates(&mut prev, 7, 12_000, 5_500, t3),
            (10_000, 5_000)
        );
    }

    #[test]
    fn compute_net_rates_handles_zero_elapsed_time() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();

        assert_eq!(compute_net_rates(&mut prev, 9, 100, 200, t0), (0, 0));
        assert_eq!(compute_net_rates(&mut prev, 9, 500, 900, t0), (0, 0));
    }
}
