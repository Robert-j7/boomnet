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
    use std::cell::UnsafeCell;
    use std::os::fd::AsRawFd;
    use std::io;

    const CONN_COUNT: usize = 11;
    const TARGET_SAMPLES: usize = 200_000;
    const RX_CPU: usize = 2;
    const APP_CPU: usize = 3;
    const STREAM_PATH: &str =
        "/stream?streams=ethusdt@bookTicker/btcusdt@bookTicker/solusdt@bookTicker/ethusdc@bookTicker/btcusdc@bookTicker";

    let host = "fstream.binance.com";
    let iface = std::env::args().nth(1);
    pin_to_core(APP_CPU)?;

    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        return Err(io::Error::last_os_error().into());
    }

    let mut conns: Vec<UnsafeCell<Conn>> = Vec::with_capacity(CONN_COUNT);

    for idx in 0..CONN_COUNT {
        let stream = ConnectionInfo::new(host, 443).with_cpu(RX_CPU).into_tcp_stream()?;
        let fd = stream.as_raw_fd();

        if let Some(iface) = iface.as_deref() {
            if let Err(err) = configure_hwtstamp(fd, iface) {
                eprintln!("warn: ioctl(SIOCSHWTSTAMP) failed for {iface}: {err}");
            }
        }

        if let Err(err) = set_so_busy_poll(fd, 50) {
            eprintln!("warn: setsockopt(SO_BUSY_POLL) failed: {err}");
        }
        if let Err(err) = set_so_prefer_busy_poll(fd, true) {
            eprintln!("warn: setsockopt(SO_PREFER_BUSY_POLL) failed: {err}");
        }
        if let Err(err) = set_so_rcvlowat(fd, 1) {
            eprintln!("warn: setsockopt(SO_RCVLOWAT) failed: {err}");
        }

        enable_rx_timestamping(fd)?;
        let stream = TimestampingStream::new(stream);
        let ws = stream.into_tls_stream()?.into_websocket(STREAM_PATH);

        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT) as u32,
            u64: idx as u64,
        };
        let rc = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if rc < 0 {
            unsafe { libc::close(epfd) };
            return Err(io::Error::last_os_error().into());
        }

        conns.push(UnsafeCell::new(Conn { ws }));
    }

    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 128];
    let mut nic_to_kernel = Vec::with_capacity(TARGET_SAMPLES);
    let mut tls_to_userspace = Vec::with_capacity(TARGET_SAMPLES);
    let mut nic_to_userspace = Vec::with_capacity(TARGET_SAMPLES);
    let mut missing_hw = 0usize;
    let mut messages = 0usize;

    while messages < TARGET_SAMPLES {
        let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), events.len() as i32, 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            unsafe { libc::close(epfd) };
            return Err(err.into());
        }
        if n == 0 {
            std::hint::spin_loop();
            continue;
        }

        for ev in events.iter().take(n as usize) {
            if (ev.events & (libc::EPOLLIN | libc::EPOLLOUT) as u32) == 0 {
                continue;
            }
            let idx = ev.u64 as usize;
            if idx >= conns.len() {
                continue;
            }
            // SAFETY: each token maps to a unique connection, and we only access one at a time.
            let conn = unsafe { &mut *conns[idx].get() };

            let batch = conn.ws.read_batch_ts()?;
            let rx = batch.rx_timestamps().unwrap_or_default();
            let read_ns = clock_realtime_ns();
            for frame in batch.iter() {
                if let WebsocketFrame::Text(_fin, _body) = frame? {
                    let ready_ns = clock_realtime_ns();
                    let nic_to_kernel_ns = if rx.hw_raw_ns != 0 && read_ns != 0 {
                        read_ns.saturating_sub(rx.hw_raw_ns) as i64
                    } else {
                        missing_hw += 1;
                        0
                    };
                    let tls_to_userspace_ns = if ready_ns != 0 && read_ns != 0 {
                        ready_ns.saturating_sub(read_ns) as i64
                    } else {
                        0
                    };
                    let nic_to_userspace_ns = if rx.hw_raw_ns != 0 && ready_ns != 0 {
                        ready_ns.saturating_sub(rx.hw_raw_ns) as i64
                    } else {
                        0
                    };

                    nic_to_kernel.push(nic_to_kernel_ns);
                    tls_to_userspace.push(tls_to_userspace_ns);
                    nic_to_userspace.push(nic_to_userspace_ns);
                    messages += 1;
                    if messages >= TARGET_SAMPLES {
                        break;
                    }
                }
            }
            if messages >= TARGET_SAMPLES {
                break;
            }
        }
    }

    unsafe { libc::close(epfd) };
    print_stats("nic_to_kernel_ns", &nic_to_kernel);
    print_stats("tls_to_userspace_ns", &tls_to_userspace);
    print_stats("nic_to_userspace_ns", &nic_to_userspace);
    println!("missing_hw={missing_hw}");
    Ok(())
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
type WsStream = boomnet::stream::tls::TlsStream<
    boomnet::stream::timestamping::TimestampingStream<boomnet::stream::tcp::TcpStream>,
>;

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
struct Conn {
    ws: boomnet::ws::Websocket<WsStream>,
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

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn pin_to_core(core_id: usize) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core_id, &mut set);
        let rc = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn set_so_busy_poll(fd: std::os::fd::RawFd, us: libc::c_int) -> std::io::Result<()> {
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BUSY_POLL,
            (&us as *const libc::c_int).cast(),
            std::mem::size_of_val(&us) as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn set_so_prefer_busy_poll(fd: std::os::fd::RawFd, enable: bool) -> std::io::Result<()> {
    let val: libc::c_int = if enable { 1 } else { 0 };
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PREFER_BUSY_POLL,
            (&val as *const libc::c_int).cast(),
            std::mem::size_of_val(&val) as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn set_so_rcvlowat(fd: std::os::fd::RawFd, val: libc::c_int) -> std::io::Result<()> {
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVLOWAT,
            (&val as *const libc::c_int).cast(),
            std::mem::size_of_val(&val) as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
struct Stats {
    n: usize,
    mean: f64,
    stddev: f64,
    p50: i64,
    p90: i64,
    p99: i64,
}

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn print_stats(label: &str, values: &[i64]) {
    let mut data: Vec<i64> = values.iter().copied().filter(|v| *v > 0).collect();
    if data.is_empty() {
        println!("{label}: n=0");
        return;
    }
    data.sort_unstable();
    let stats = calc_stats(&data);
    println!(
        "{label}: n={} mean={:.1} stddev={:.1} p50={} p90={} p99={}",
        stats.n, stats.mean, stats.stddev, stats.p50, stats.p90, stats.p99
    );
}

#[cfg(all(
    target_os = "linux",
    feature = "timestamping",
    feature = "ws",
    any(feature = "rustls", feature = "openssl")
))]
fn calc_stats(sorted: &[i64]) -> Stats {
    let n = sorted.len();
    let mut sum: i128 = 0;
    let mut sumsq: u128 = 0;
    for &v in sorted {
        let x = v as i128;
        sum += x;
        sumsq += (x * x) as u128;
    }
    let nf = n as f64;
    let mean = (sum as f64) / nf;
    let ex2 = (sumsq as f64) / nf;
    let var = (ex2 - mean * mean).max(0.0);
    let stddev = var.sqrt();

    fn pick(sorted: &[i64], p: f64) -> i64 {
        let n = sorted.len();
        let idx = ((p * (n as f64 - 1.0)).round() as usize).min(n - 1);
        sorted[idx]
    }

    Stats {
        n,
        mean,
        stddev,
        p50: pick(sorted, 0.50),
        p90: pick(sorted, 0.90),
        p99: pick(sorted, 0.99),
    }
}
