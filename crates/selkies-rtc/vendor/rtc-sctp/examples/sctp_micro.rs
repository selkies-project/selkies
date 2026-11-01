//! Fixed-work SCTP packet-marshalling micro-benchmark for `perf`/`poop`.
//!
//! Marshals a representative DATA packet (parsed once) in a tight loop so that
//! before/after `poop` comparisons and `perf` profiles isolate the steady-state
//! per-packet marshalling cost on the DataChannel send path.
//!
//! Usage: sctp_micro [payload_len] [num_chunks] [iterations]

use rtc_sctp::fuzzing;

fn main() {
    let mut args = std::env::args().skip(1);
    let payload_len: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1200);
    let num_chunks: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let iters: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200_000);

    let packet = fuzzing::sample_data_packet(payload_len, num_chunks);
    let n = fuzzing::bench_packet_marshal(&packet, iters).unwrap();
    std::hint::black_box(n);
}
