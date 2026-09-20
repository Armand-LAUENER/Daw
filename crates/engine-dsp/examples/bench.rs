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
    let ns = started.elapsed().as_secs_f64() * 1e9 / SAMPLES as f64;
    let buffer_ns = BUFFER_FRAMES / f64::from(SR) * 1e9;
    let share = ns * BUFFER_FRAMES / buffer_ns * 100.0;
    println!("{name:<34} {ns:>10.2} {:>13.1} {share:>15.3}%", 1_000.0 / ns);
}
