//! Sine-wave test tone generator, used by the milestone-1 demo.
//!
//! The generator keeps its own phase (a position in the sine cycle, 0.0–1.0)
//! instead of an absolute sample counter, so it can run for hours without
//! losing floating-point precision.

use cpal::{FromSample, SizedSample};

pub struct Tone {
    /// Current position in the sine cycle: 0.0 = start of cycle, 1.0 = one full
    /// cycle. Kept in f64 so small per-sample steps don't get rounded away.
    phase: f64,
    /// How much the phase advances per sample: frequency / sample rate.
    increment: f64,
    /// Peak amplitude, 0.0–1.0.
    amplitude: f64,
}

impl Tone {
    /// `increment` must be < 1.0 (i.e. frequency below the sample rate);
    /// callers should additionally stay below 0.5 to respect Nyquist.
    pub fn new(freq_hz: f64, sample_rate: u32, amplitude: f64) -> Self {
        Self {
            phase: 0.0,
            increment: freq_hz / sample_rate as f64,
            amplitude,
        }
    }

    /// Fill `buf` (interleaved samples, `channels` per frame) with the tone.
    /// Both channels get the same value, so the tone is centered.
    ///
    /// Generic over the output sample format: cpal hands the callback a slice
    /// of f32, i16 or i32 depending on what the device was opened with, so we
    /// need `FromSample<f32>` to convert our float sine into that format.
    pub fn write<S: SizedSample + FromSample<f32>>(&mut self, buf: &mut [S], channels: usize) {
        // cpal always hands us a frame-aligned buffer; max(1) guards against a
        // zero-channel config producing a zero-sized chunk panic.
        let channels = channels.max(1);
        for frame in buf.chunks_mut(channels) {
            let value = (self.phase * std::f64::consts::TAU).sin() * self.amplitude;
            // cpal::Sample (re-exported from dasp) converts f32 in -1.0..1.0
            // into whatever format the output device expects.
            let sample = S::from_sample(value as f32);
            for channel in frame.iter_mut() {
                *channel = sample;
            }
            self.phase += self.increment;
            if self.phase >= 1.0 {
                // increment < 1.0 is validated in main, so one subtraction is
                // enough to wrap back into 0.0–1.0.
                self.phase -= 1.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(freq: f64, rate: u32, n: u64) -> f64 {
        ((n as f64) * freq / rate as f64 * std::f64::consts::TAU).sin()
    }

    #[test]
    fn starts_at_zero_and_tracks_reference_sine() {
        let mut tone = Tone::new(440.0, 48_000, 1.0);
        let mut buf = [0.0f32; 480]; // mono, 10 ms
        tone.write(&mut buf, 1);
        assert_eq!(buf[0], 0.0); // sin(0)
        for (n, sample) in buf.iter().enumerate() {
            let expected = reference(440.0, 48_000, n as u64) as f32;
            assert!(
                (*sample - expected).abs() < 1e-4,
                "sample {n}: got {sample}, expected {expected}"
            );
        }
    }

    #[test]
    fn stereo_frames_hold_the_same_value() {
        let mut tone = Tone::new(1000.0, 48_000, 0.5);
        let mut buf = [0.0f32; 6]; // 3 stereo frames
        tone.write(&mut buf, 2);
        assert_eq!(buf[0], buf[1]);
        assert_eq!(buf[2], buf[3]);
        assert_eq!(buf[4], buf[5]);
    }

    #[test]
    fn wraps_and_stays_continuous_across_buffers() {
        // 11025 Hz at 48 kHz means exactly 4 samples per cycle; run long enough
        // that the phase wraps many times and check continuity against the
        // reference sine.
        let mut tone = Tone::new(11_025.0, 48_000, 1.0);
        let mut buf = [0.0f32; 1024]; // not frame-aligned for 4-sample cycles? mono, so fine
        for frame_start in 0..200usize {
            tone.write(&mut buf, 1);
            for (i, sample) in buf.iter().enumerate() {
                let n = (frame_start * 1024 + i) as u64;
                let expected = reference(11_025.0, 48_000, n) as f32;
                assert!(
                    (*sample - expected).abs() < 1e-3,
                    "sample {n}: got {sample}, expected {expected}"
                );
            }
        }
    }

    #[test]
    fn i16_output_is_scaled_and_clamped() {
        let mut tone = Tone::new(12_000.0, 48_000, 1.0);
        let mut buf = [0i16; 4000]; // long enough to cross the sine peak
        tone.write(&mut buf, 1);
        // A full-scale sine should reach close to both peaks in i16 terms.
        assert!(
            *buf.iter().max().unwrap() > 30_000,
            "i16 tone positive peak too quiet to be full scale"
        );
        assert!(
            *buf.iter().min().unwrap() < -30_000,
            "i16 tone negative peak too quiet to be full scale"
        );
    }
}
