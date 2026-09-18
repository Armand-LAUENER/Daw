/// Messages sent from the control side to the audio thread.
///
/// Must stay `Copy` and heap-free: they cross the real-time boundary by value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// Oscillator frequency in Hz, reached with a short glide.
    SetFrequency(f32),
    /// Linear output gain, reached with a short ramp.
    SetGain(f32),
}
