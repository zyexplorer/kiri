//! `kiri boot`: boots the guest, times every stage, and powers it off again.

use std::io;
use std::time::Duration;

use crate::guest::{BootOptions, Guest};

pub fn run(args: &[String]) -> io::Result<()> {
    let mut opts = BootOptions::default();
    let mut pings = 1000usize;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--console" => opts.console = true,
            "--pings" => {
                pings = it.next().and_then(|v| v.parse().ok()).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--pings needs a number")
                })?;
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option {other}"),
                ));
            }
        }
    }

    let mut guest = Guest::boot(&opts)?;
    let t = guest.timings;
    let hello = guest.hello;
    let latency = if pings > 0 {
        Some(guest.ping(pings)?)
    } else {
        None
    };
    let stopped = guest.shutdown();

    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    println!("kiri boot");
    println!("  VM configured + started     {:8.1} ms", ms(t.vm_started));
    println!("  agent ready                 {:8.1} ms", ms(t.agent_ready));
    println!("  guest: kernel -> PID 1      {:8.1} ms", ms(t.kernel));
    println!("  guest: PID 1 -> ready       {:8.1} ms", ms(t.init));
    println!(
        "  guest memory                {:8} MiB total, {} MiB available",
        hello.mem_total_kib / 1024,
        hello.mem_available_kib / 1024
    );
    if let Some(l) = latency {
        println!(
            "  vsock round trip ({pings:>5})    p50 {:6.1} µs   p99 {:6.1} µs",
            us(l.p50),
            us(l.p99)
        );
    }
    match stopped {
        Ok(d) => println!("  shutdown -> stopped         {:8.1} ms", ms(d)),
        Err(e) => println!("  shutdown                    {e}"),
    }
    Ok(())
}
