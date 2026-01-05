#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn main() -> anyhow::Result<()> {
    use boomnet::stream::ConnectionInfo;
    use boomnet::stream::timestamping::{configure_hwtstamp, enable_rx_timestamping, TimestampingStream};
    use boomnet::stream::tls::IntoTlsStream;
    use boomnet::ws::{IntoWebsocket, WebsocketFrame};
    use std::os::fd::AsRawFd;

    let host = "fstream.binance.com";
    let iface = std::env::args().nth(1);
    let stream = ConnectionInfo::new(host, 443).into_tcp_stream()?;

    if let Some(iface) = iface.as_deref() {
        if let Err(err) = configure_hwtstamp(stream.as_raw_fd(), iface) {
            eprintln!("warn: ioctl(SIOCSHWTSTAMP) failed for {iface}: {err}");
        }
    }

    enable_rx_timestamping(stream.as_raw_fd())?;
    let stream = TimestampingStream::new(stream);

    let mut ws = stream.into_tls_stream()?.into_websocket(
        "/stream?streams=ethusdt@bookTicker/btcusdt@bookTicker/solusdt@bookTicker/ethusdc@bookTicker/btcusdc@bookTicker",
    );

    loop {
        let batch = ws.read_batch_ts()?;
        let rx = batch.rx_timestamps().unwrap_or_default();
        let read_ns = clock_realtime_ns();
        for frame in batch.iter() {
            if let WebsocketFrame::Text(_fin, body) = frame? {
                let ready_ns = clock_realtime_ns();
                let nic_to_kernel_ns = if rx.hw_raw_ns != 0 && read_ns != 0 {
                    read_ns.saturating_sub(rx.hw_raw_ns)
                } else {
                    0
                };
                let tls_to_userspace_ns = if ready_ns != 0 && read_ns != 0 {
                    ready_ns.saturating_sub(read_ns)
                } else {
                    0
                };
                println!(
                    "hw_raw_ns={} nic_to_kernel_ns={} tls_to_userspace_ns={} msg={}",
                    rx.hw_raw_ns,
                    nic_to_kernel_ns,
                    tls_to_userspace_ns,
                    String::from_utf8_lossy(body)
                );
            }
        }
    }
}

#[cfg(not(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
)))]
fn main() {
    eprintln!("This example requires Linux and features: ws, timestamping, and rustls-* or openssl.");
}

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn clock_realtime_ns() -> u64 {
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) != 0 {
            return 0;
        }
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    }
}
