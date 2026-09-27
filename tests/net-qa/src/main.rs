//! Bounded UDP receive probe; TCP negotiates the run and carries the final report.
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"SNETQA01";
const HEADER: usize = 32;
const MAX_PACKETS: usize = 2_000_000;
const MAX_DURATION_MS: u64 = 600_000;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut bytes = Vec::new();
    // Bounded even when a peer never sends a newline.
    use std::io::Read;
    reader.take(256).read_until(b'\n', &mut bytes)?;
    if bytes.last() != Some(&b'\n') {
        return Err(invalid("missing or oversized control line"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("non-UTF8 control line"))
}

fn numbers(line: &str, verb: &str, count: usize) -> io::Result<Vec<u64>> {
    let mut fields = line.split_whitespace();
    if fields.next() != Some(verb) {
        return Err(invalid("unexpected control command"));
    }
    let values: Vec<u64> = fields
        .map(|s| s.parse().map_err(|_| invalid("invalid control number")))
        .collect::<io::Result<_>>()?;
    if values.len() != count {
        return Err(invalid("wrong control field count"));
    }
    Ok(values)
}

struct Config {
    run: u64,
    source_port: u16,
    size: usize,
    planned: usize,
    duration_ms: u64,
}

impl Config {
    fn parse(text: &str) -> io::Result<Self> {
        let n = numbers(text, "NETQA1", 5)?;
        if !(1..=65535).contains(&n[1])
            || !(64..=1472).contains(&n[2])
            || !(1..=MAX_PACKETS as u64).contains(&n[3])
            || !(1000..=MAX_DURATION_MS).contains(&n[4])
        {
            return Err(invalid("configuration outside supported limits"));
        }
        Ok(Self {
            run: n[0],
            source_port: n[1] as u16,
            size: n[2] as usize,
            planned: n[3] as usize,
            duration_ms: n[4],
        })
    }
}

struct Received {
    seen: Vec<bool>,
    unique: usize,
    duplicates: usize,
    reordered: usize,
    invalid: usize,
    high: Option<usize>,
    previous: Option<(u64, u64)>,
    gaps_us: Vec<u32>,
    jitter_ns: f64,
    intervals: Vec<usize>,
}

impl Received {
    fn new(planned: usize) -> Self {
        Self {
            seen: vec![false; planned],
            unique: 0,
            duplicates: 0,
            reordered: 0,
            invalid: 0,
            high: None,
            previous: None,
            gaps_us: Vec::with_capacity(planned),
            jitter_ns: 0.0,
            intervals: vec![0; (MAX_DURATION_MS / 1000 + 11) as usize],
        }
    }

    fn record(&mut self, bytes: &[u8], cfg: &Config, arrival_ns: u64) {
        if bytes.len() != cfg.size || bytes.len() < HEADER || &bytes[..8] != MAGIC {
            self.invalid += 1;
            return;
        }
        let read = |at| u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());
        let (run, seq, sent_ns) = (read(8), read(16), read(24));
        if run != cfg.run || seq >= cfg.planned as u64 {
            self.invalid += 1;
            return;
        }
        let seq = seq as usize;
        if self.seen[seq] {
            self.duplicates += 1;
            return;
        }
        self.seen[seq] = true;
        self.unique += 1;
        if self.high.is_some_and(|high| seq < high) {
            self.reordered += 1;
        }
        self.high = Some(self.high.map_or(seq, |high| high.max(seq)));
        if let Some((last_arrival, last_sent)) = self.previous {
            let gap = arrival_ns.saturating_sub(last_arrival);
            self.gaps_us
                .push((gap / 1000).min(u64::from(u32::MAX)) as u32);
            // Difference of deltas needs no synchronized host/guest clocks.
            let delta = (i128::from(gap) - (i128::from(sent_ns) - i128::from(last_sent))).abs();
            self.jitter_ns += (delta as f64 - self.jitter_ns) / 16.0;
        }
        self.previous = Some((arrival_ns, sent_ns));
        let bin = (arrival_ns / 1_000_000_000) as usize;
        if let Some(count) = self.intervals.get_mut(bin) {
            *count += 1;
        }
    }

    fn report(&mut self, cfg: &Config, sent: usize, elapsed_ns: u64, complete: bool) -> String {
        // Missing sequence numbers become loss only after the drain period.
        let received = self.seen[..sent].iter().filter(|&&seen| seen).count();
        let lost = sent - received;
        let mut longest_loss = 0;
        let mut streak = 0;
        for &seen in &self.seen[..sent] {
            streak = if seen { 0 } else { streak + 1 };
            longest_loss = longest_loss.max(streak);
        }
        self.gaps_us.sort_unstable();
        let percentile = |percent: usize| {
            self.gaps_us
                .get(
                    (self.gaps_us.len() * percent)
                        .div_ceil(100)
                        .saturating_sub(1),
                )
                .copied()
                .unwrap_or(0)
        };
        let gap_max = self.gaps_us.last().copied().unwrap_or(0);
        let denominator = elapsed_ns.max(1) as f64;
        let end = self
            .intervals
            .iter()
            .rposition(|&n| n != 0)
            .map_or(0, |i| i + 1);
        format!(
            concat!(
                "{{\"protocol\":1,\"complete\":{},\"run_id\":{},\"packet_bytes\":{},",
                "\"sent_packets\":{},\"received_packets\":{},\"lost_packets\":{},",
                "\"loss_percent\":{:.6},\"duplicates\":{},\"reordered\":{},\"invalid\":{},",
                "\"longest_loss_run\":{},\"sender_elapsed_ns\":{},",
                "\"offered_mbps\":{:.6},\"received_mbps\":{:.6},",
                "\"gap_p95_us\":{},\"gap_p99_us\":{},\"gap_max_us\":{},",
                "\"jitter_us\":{:.3},\"received_per_second\":{:?}}}"
            ),
            complete,
            cfg.run,
            cfg.size,
            sent,
            received,
            lost,
            lost as f64 * 100.0 / sent.max(1) as f64,
            self.duplicates,
            self.reordered,
            self.invalid,
            longest_loss,
            elapsed_ns,
            sent as f64 * cfg.size as f64 * 8000.0 / denominator,
            received as f64 * cfg.size as f64 * 8000.0 / denominator,
            percentile(95),
            percentile(99),
            gap_max,
            self.jitter_ns / 1000.0,
            &self.intervals[..end]
        )
    }
}

fn run(mut control: TcpStream, udp_port: u16) -> io::Result<()> {
    control.set_read_timeout(Some(Duration::from_secs(5)))?;
    control.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(control.try_clone()?);
    let cfg = Config::parse(&line(&mut reader)?)?;
    let peer = control.peer_addr()?;
    let udp = UdpSocket::bind(("0.0.0.0", udp_port))?;
    udp.connect(SocketAddr::new(peer.ip(), cfg.source_port))?;
    udp.set_read_timeout(Some(Duration::from_millis(100)))?;
    let mut stats = Received::new(cfg.planned);
    control.set_read_timeout(Some(Duration::from_millis(cfg.duration_ms + 10_000)))?;
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = line(&mut reader).and_then(|text| numbers(&text, "DONE", 2));
        let _ = tx.send(result);
    });
    let outcome = (|| -> io::Result<String> {
        writeln!(control, "READY {udp_port}")?;
        let started = Instant::now();
        let hard_limit = Duration::from_millis(cfg.duration_ms + 10_000);
        let mut done = None;
        let mut drain_until = None;
        let mut bytes = [0u8; 2048];
        loop {
            match udp.recv(&mut bytes) {
                Ok(length) => {
                    stats.record(&bytes[..length], &cfg, started.elapsed().as_nanos() as u64)
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
            if done.is_none() {
                match rx.try_recv() {
                    Ok(result) => {
                        let n = result?;
                        if n[0] > cfg.planned as u64
                            || n[1] == 0
                            || n[1] > hard_limit.as_nanos() as u64
                        {
                            return Err(invalid("invalid sender completion"));
                        }
                        done = Some((n[0] as usize, n[1]));
                        drain_until = Some(Instant::now() + Duration::from_millis(500));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(invalid("control reader exited"));
                    }
                }
            }
            if drain_until.is_some_and(|deadline| Instant::now() >= deadline) {
                let (sent, elapsed) = done.unwrap();
                if stats.seen[sent..].iter().any(|&seen| seen) {
                    return Err(invalid("received sequence beyond sender completion"));
                }
                return Ok(stats.report(&cfg, sent, elapsed, true));
            }
            if started.elapsed() >= hard_limit {
                // Planned but unsent packets are NOT reported as measured loss.
                return Err(invalid(
                    "run deadline exceeded; sender totals unavailable or drain incomplete",
                ));
            }
        }
    })();
    match &outcome {
        Ok(report) => {
            println!("{report}");
            let _ = writeln!(control, "{report}");
        }
        Err(error) => {
            let _ = writeln!(control, "ERROR {error}");
        }
    }
    let _ = control.shutdown(Shutdown::Both);
    let _ = worker.join();
    outcome.map(|_| ())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 && args.len() != 3 {
        eprintln!("usage: net-qa BIND_IPV4:TCP_PORT [UDP_PORT]  (one run, then exit)");
        std::process::exit(2);
    }
    let bind: SocketAddr = args[1]
        .parse()
        .map_err(|_| invalid("invalid IPv4 bind address"))?;
    if !bind.is_ipv4() {
        return Err(invalid("IPv4 only"));
    }
    let udp_port: u16 = if let Some(port) = args.get(2) {
        port.parse().map_err(|_| invalid("invalid UDP port"))?
    } else {
        bind.port()
            .checked_add(1)
            .ok_or_else(|| invalid("UDP port overflow"))?
    };
    if bind.port() == 0 || udp_port == 0 {
        return Err(invalid("ports must be nonzero"));
    }
    let listener = TcpListener::bind(bind)?;
    println!("NETQA listening tcp={bind} udp={udp_port}");
    let (control, _) = listener.accept()?;
    run(control, udp_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(seq: u64, timestamp: u64) -> Vec<u8> {
        let mut p = vec![0; 64];
        p[..8].copy_from_slice(MAGIC);
        p[8..16].copy_from_slice(&7u64.to_be_bytes());
        p[16..24].copy_from_slice(&seq.to_be_bytes());
        p[24..32].copy_from_slice(&timestamp.to_be_bytes());
        p
    }
    #[test]
    fn late_packets_recover_holes_without_hiding_duplicates_or_tail_loss() {
        let cfg = Config::parse("NETQA1 7 1234 64 8 1000").unwrap();
        let mut stats = Received::new(8);
        for (i, seq) in [0, 3, 1, 3, 5, 2].into_iter().enumerate() {
            stats.record(&packet(seq, seq * 1000), &cfg, i as u64 * 2000);
        }
        let report = stats.report(&cfg, 8, 1_000_000_000, true);
        assert!(report.contains("\"received_packets\":5"));
        assert!(report.contains("\"lost_packets\":3"));
        assert!(report.contains("\"duplicates\":1"));
        assert!(report.contains("\"reordered\":2"));
        assert!(report.contains("\"longest_loss_run\":2"));
    }
    #[test]
    fn rejects_truncated_foreign_and_out_of_range_datagrams() {
        let cfg = Config::parse("NETQA1 7 1234 64 2 1000").unwrap();
        let mut stats = Received::new(2);
        stats.record(&[0; 12], &cfg, 0);
        stats.record(&packet(2, 0), &cfg, 0);
        let mut foreign = packet(0, 0);
        foreign[15] = 8;
        stats.record(&foreign, &cfg, 0);
        assert_eq!(stats.invalid, 3);
        assert_eq!(stats.unique, 0);
    }
    #[test]
    fn sender_short_run_does_not_count_unsent_tail_as_loss() {
        let cfg = Config::parse("NETQA1 7 1234 64 8 1000").unwrap();
        let mut stats = Received::new(8);
        stats.record(&packet(0, 0), &cfg, 0);
        assert!(
            stats
                .report(&cfg, 2, 1_000_000_000, true)
                .contains("\"lost_packets\":1")
        );
    }
    #[test]
    fn bounds_control_input_and_allocations() {
        assert!(Config::parse("NETQA1 7 1234 64 2000001 1000").is_err());
        assert!(Config::parse("NETQA1 7 1234 65535 8 1000").is_err());
        assert!(Config::parse("NETQA1 7 1234 64 8 0").is_err());
        assert!(line(&mut &b"no newline"[..]).is_err());
        assert!(line(&mut &vec![b'a'; 300][..]).is_err());
    }
}
