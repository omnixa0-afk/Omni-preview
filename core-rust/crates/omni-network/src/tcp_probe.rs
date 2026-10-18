//! The counters of a TCP door's socket, for the pipe stats in the trace (Windows `SIO_TCP_INFO`):
//! bytes out and retransmitted, window, RTT. They told a loss of the path from a ceiling of the sender's
//! uplink (docs/bugs/door-throughput.md, «Потолок отдачи»).

/// The TCP socket's own counters, read through its handle while the connection lives.
pub(crate) struct TcpProbe {
    #[cfg_attr(not(windows), allow(dead_code))]
    socket: u64,
    last: Option<TcpCounters>,
}

#[derive(Clone, Copy)]
struct TcpCounters {
    bytes_out: u64,
    bytes_retrans: u32,
    fast_retrans: u32,
    timeouts: u32,
    cwnd: u32,
    in_flight: u32,
    rtt_us: u32,
    min_rtt_us: u32,
}

impl TcpProbe {
    pub(crate) fn of(stream: &tokio::net::TcpStream) -> Option<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            Some(Self { socket: stream.as_raw_socket(), last: None })
        }
        #[cfg(not(windows))]
        {
            let _ = stream;
            None
        }
    }

    /// The counters since the last call, for the trace; empty if the OS gives none.
    pub(crate) fn report(&mut self) -> String {
        let Some(now) = self.read() else {
            return String::new();
        };
        let line = match self.last {
            Some(last) => {
                let out = now.bytes_out.saturating_sub(last.bytes_out);
                let retrans = now.bytes_retrans.wrapping_sub(last.bytes_retrans) as u64;
                format!(
                    "tcp_out={} tcp_retrans={} retrans_pct={:.2} fast_retrans={} timeouts={} cwnd={} in_flight={} rtt_ms={} min_rtt_ms={}",
                    out,
                    retrans,
                    100.0 * retrans as f64 / out.max(1) as f64,
                    now.fast_retrans.wrapping_sub(last.fast_retrans),
                    now.timeouts.wrapping_sub(last.timeouts),
                    now.cwnd,
                    now.in_flight,
                    now.rtt_us / 1000,
                    now.min_rtt_us / 1000,
                )
            }
            None => String::new(),
        };
        self.last = Some(now);
        line
    }

    #[cfg(windows)]
    fn read(&self) -> Option<TcpCounters> {
        windows_tcp_info::read(self.socket)
    }

    #[cfg(not(windows))]
    fn read(&self) -> Option<TcpCounters> {
        None
    }
}

#[cfg(windows)]
mod windows_tcp_info {
    use super::TcpCounters;
    use core::ffi::c_void;

    /// `SIO_TCP_INFO` — `_WSAIORW(IOC_VENDOR, 39)`, Windows 10 1703+.
    const SIO_TCP_INFO: u32 = 0xD800_0027;

    /// `TCP_INFO_v0` from mstcpip.h.
    #[repr(C)]
    #[derive(Default)]
    struct TcpInfoV0 {
        state: i32,
        mss: u32,
        connection_time_ms: u64,
        timestamps_enabled: u8,
        rtt_us: u32,
        min_rtt_us: u32,
        bytes_in_flight: u32,
        cwnd: u32,
        snd_wnd: u32,
        rcv_wnd: u32,
        rcv_buf: u32,
        bytes_out: u64,
        bytes_in: u64,
        bytes_reordered: u32,
        bytes_retrans: u32,
        fast_retrans: u32,
        dup_acks_in: u32,
        timeout_episodes: u32,
        syn_retrans: u8,
    }

    #[link(name = "ws2_32")]
    extern "system" {
        #[link_name = "WSAIoctl"]
        fn wsa_ioctl(
            socket: usize,
            code: u32,
            in_buf: *const c_void,
            in_len: u32,
            out_buf: *mut c_void,
            out_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
            completion: *mut c_void,
        ) -> i32;
    }

    pub(super) fn read(socket: u64) -> Option<TcpCounters> {
        let version: u32 = 0;
        let mut info = TcpInfoV0::default();
        let mut returned = 0u32;
        // SAFETY: plain buffers of the sizes passed; no overlapped I/O, so the call completes here.
        let rc = unsafe {
            wsa_ioctl(
                socket as usize,
                SIO_TCP_INFO,
                (&version as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
                (&mut info as *mut TcpInfoV0).cast(),
                std::mem::size_of::<TcpInfoV0>() as u32,
                &mut returned,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (rc == 0).then_some(TcpCounters {
            bytes_out: info.bytes_out,
            bytes_retrans: info.bytes_retrans,
            fast_retrans: info.fast_retrans,
            timeouts: info.timeout_episodes,
            cwnd: info.cwnd,
            in_flight: info.bytes_in_flight,
            rtt_us: info.rtt_us,
            min_rtt_us: info.min_rtt_us,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_counters_of_a_live_socket_can_be_read() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, server) = tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let client = client.unwrap();
        let _server = server.unwrap();
        let Some(mut probe) = TcpProbe::of(&client) else {
            return; // no counters on this OS
        };
        assert_eq!(probe.report(), "", "the first read only sets the baseline");
        assert!(probe.report().contains("tcp_out="), "the second reports the counters");
    }
}
