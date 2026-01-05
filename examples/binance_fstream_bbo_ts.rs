#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn main() -> anyhow::Result<()> {
    use boomnet::stream::ConnectionInfo;
    use boomnet::stream::timestamping::{enable_rx_timestamping, TimestampingStream};
    use boomnet::stream::tls::IntoTlsStream;
    use boomnet::ws::{IntoWebsocket, WebsocketFrame};
    use std::os::fd::AsRawFd;

    let host = "fstream.binance.com";
    let stream = ConnectionInfo::new(host, 443).into_tcp_stream()?;

    enable_rx_timestamping(stream.as_raw_fd())?;
    let stream = TimestampingStream::new(stream);

    let mut ws = stream
        .into_tls_stream()?
        .into_websocket("/ws/ethusdt@bookTicker");

    loop {
        let batch = ws.read_batch_ts()?;
        let rx = batch.rx_timestamps().unwrap_or_default();
        for frame in batch.iter() {
            if let WebsocketFrame::Text(_fin, body) = frame? {
                println!(
                    "hw_raw_ns={} hw_sys_ns={} sw_ns={} msg={}",
                    rx.hw_raw_ns,
                    rx.hw_sys_ns,
                    rx.sw_ns,
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
