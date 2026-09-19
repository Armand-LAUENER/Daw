/// Linear ramp towards a target value over a fixed number of samples.
///
/// Used to de-zipper control changes: a parameter written from the control
/// thread is reached progressively instead of jumping at a buffer boundary.
/// A new target restarts the ramp from the current value, so retargeting
/// mid-ramp never introduces a step.
#[derive(Debug, Clone)]
pub struct LinearSmoother {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
    ramp_len: u32,
}

impl LinearSmoother {
    /// `ramp_len` is the ramp duration in samples; 0 means changes apply immediately.
    pub fn new(initial: f32, ramp_len: u32) -> Self {
        Self { current: initial, target: initial, step: 0.0, remaining: 0, ramp_len }
    }

    /// Builds a smoother whose ramp lasts `ramp_ms` at `sample_rate`.
    pub fn with_duration(initial: f32, ramp_ms: f32, sample_rate: f32) -> Self {
        Self::new(initial, (ramp_ms * 1e-3 * sample_rate).round().max(0.0) as u32)
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
        if self.ramp_len == 0 {
            self.current = target;
            self.remaining = 0;
        } else {
            self.remaining = self.ramp_len;
            self.step = (target - self.current) / self.ramp_len as f32;
        }
    }

    /// Returns the current value and advances the ramp by one sample.
    #[inline]
    pub fn next_value(&mut self) -> f32 {
        let out = self.current;
        if self.remaining > 0 {
            self.remaining -= 1;
            // Derived from the target instead of accumulated: rounding errors do not
            // build up, the ramp never overshoots, and it lands exactly on the target.
            self.current = self.target - self.step * self.remaining as f32;
        }
        out
    }

    pub fn is_ramping(&self) -> bool {
        self.remaining > 0
    }

    pub fn target(&self) -> f32 {
        self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaches_target_exactly_after_ramp() {
        let mut s = LinearSmoother::new(0.0, 100);
        s.set_target(1.0);
        for _ in 0..100 {
            s.next_value();
        }
        assert!(!s.is_ramping());
        assert_eq!(s.next_value(), 1.0);
    }

    #[test]
    fn ramp_is_monotonic_with_bounded_steps() {
        let mut s = LinearSmoother::new(220.0, 480);
        s.set_target(880.0);
        let values: Vec<f32> = (0..480).map(|_| s.next_value()).collect();
        let max_step = (880.0 - 220.0) / 480.0 * 1.001;
        assert!(values.windows(2).all(|w| w[1] >= w[0] && w[1] - w[0] <= max_step));
    }

    #[test]
    fn retarget_mid_ramp_starts_from_current_value() {
        let mut s = LinearSmoother::new(0.0, 10);
        s.set_target(1.0);
        for _ in 0..5 {
            s.next_value();
        }
        let before = s.next_value();
        s.set_target(0.0);
        let after = s.next_value();
        assert!((after - before).abs() <= 0.1 + 1e-6, "jump from {before} to {after}");
    }

    #[test]
    fn zero_length_applies_immediately() {
        let mut s = LinearSmoother::new(0.0, 0);
        s.set_target(0.5);
        assert_eq!(s.next_value(), 0.5);
    }
}
