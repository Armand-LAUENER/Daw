//! Throughput of the DSP building blocks, in nanoseconds per sample.
//!
//! The figure that matters is the last column: how much of one 10 ms buffer at
//! 48 kHz a single instance eats. Multiply by the number of voices or tracks to
//! see what fits. Run with `cargo run --release -p engine-dsp --example bench`;
//! a debug build measures the optimiser, not the code.
//!
//! This is deliberately a plain loop rather than a statistical harness: it is
//! here to catch an order-of-magnitude regression, not to resolve 2% changes.

use std::hint::black_box;
use std::time::Instant;

use engine_dsp::{Biquad, Coefficients, FilterKind, LinearSmoother, SineOsc, SmoothedBiquad};

const SR: f32 = 48_000.0;
/// One 10 ms buffer at 48 kHz, the size WASAPI imposes in shared mode.
const BUFFER_FRAMES: f64 = 480.0;
const SAMPLES: usize = 2_000_000;

fn main() {
    println!("{SAMPLES} samples per case, {} kHz\n", SR / 1_000.0);
    println!("{:<34} {:>10} {:>13} {:>16}", "case", "ns/sample", "Msamples/s", "% of a buffer");

    let mut osc = SineOsc::new(SR);
    bench("SineOsc", || osc.next_sample(440.0));

    let mut smoother = LinearSmoother::with_duration(0.0, 10.0, SR);
    smoother.set_target(1.0);
    bench("LinearSmoother", || smoother.next_value());

    let mut biquad = Biquad::new(Coefficients::low_pass(1_000.0, 0.707, SR));
    let mut x = 0.5;
    bench("Biquad", || {
        // Feeding the output back keeps the loop from being hoisted out, and the
        // rescale keeps the signal from decaying into denormals.
        x = biquad.process_sample(x).mul_add(0.5, 0.25);
        x
    });

    let mut settled = SmoothedBiquad::new(FilterKind::LowPass, 1_000.0, 0.707, SR, 10.0);
    let mut y = 0.5;
    bench("SmoothedBiquad, settled", || {
        y = settled.process_sample(y).mul_add(0.5, 0.25);
        y
    });

    // The worst case: a cutoff that never stops moving, so the coefficients are
    // recomputed and interpolated throughout.
    let mut sweeping = SmoothedBiquad::new(FilterKind::LowPass, 200.0, 0.707, SR, 10.0);
    let mut z = 0.5;
    let mut i = 0usize;
    bench("SmoothedBiquad, sweeping", || {
        if i.is_multiple_of(480) {
            sweeping.set_cutoff(if (i / 480).is_multiple_of(2) { 8_000.0 } else { 200.0 });
        }
        i += 1;
        z = sweeping.process_sample(z).mul_add(0.5, 0.25);
        z
    });

    println!();

    // Filtering in place would feed each block its own output and decay into
    // denormals, so the harness refills the buffer every time. That refill is
    // inside the timing: this row is what it costs, to be subtracted from the
    // ones below.
    bench_block("(buffer refill, subtract this)", |_| {});

    let mut biquad_block = Biquad::new(Coefficients::low_pass(1_000.0, 0.707, SR));
    bench_block("Biquad, block", |buf| biquad_block.process_block(buf));

    let mut settled_block = SmoothedBiquad::new(FilterKind::LowPass, 1_000.0, 0.707, SR, 10.0);
    bench_block("SmoothedBiquad settled, block", |buf| settled_block.process_block(buf));

    let mut sweeping_block = SmoothedBiquad::new(FilterKind::LowPass, 200.0, 0.707, SR, 10.0);
    let mut n = 0usize;
    bench_block("SmoothedBiquad sweeping, block", |buf| {
        sweeping_block.set_cutoff(if n.is_multiple_of(2) { 8_000.0 } else { 200.0 });
        n += 1;
        sweeping_block.process_block(buf);
    });
}

/// Same measurement, for code that takes a whole buffer at a time.
fn bench_block(name: &str, mut run: impl FnMut(&mut [f32])) {
    let mut buf = vec![0.0f32; BUFFER_FRAMES as usize];
    let blocks = SAMPLES / buf.len();
    let fill = |buf: &mut Vec<f32>| {
        // Refill from a bounded signal so the filter never decays into denormals.
        for (i, x) in buf.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 0.5 } else { -0.5 };
        }
    };
    for _ in 0..blocks / 10 {
        fill(&mut buf);
        run(black_box(&mut buf));
    }
    let started = Instant::now();
    for _ in 0..blocks {
        fill(&mut buf);
        run(black_box(&mut buf));
    }
    black_box(&buf);
    let ns = started.elapsed().as_secs_f64() * 1e9 / (blocks * buf.len()) as f64;
    report(name, ns);
}

fn bench(name: &str, mut run: impl FnMut() -> f32) {
    // Warm the caches and let the CPU settle on a clock before measuring.
    for _ in 0..SAMPLES / 10 {
        black_box(run());
    }
    let started = Instant::now();
    for _ in 0..SAMPLES {
        black_box(run());
    }
    report(name, started.elapsed().as_secs_f64() * 1e9 / SAMPLES as f64);
}

fn report(name: &str, ns: f64) {
    let buffer_ns = BUFFER_FRAMES / f64::from(SR) * 1e9;
    let share = ns * BUFFER_FRAMES / buffer_ns * 100.0;
    println!("{name:<34} {ns:>10.2} {:>13.1} {share:>15.3}%", 1_000.0 / ns);
}
