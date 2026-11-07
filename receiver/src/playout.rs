//! The playout thread: turns the jitter buffer's frames into a continuous
//! f32 sample stream at (slightly drift-corrected) real-time pace, and feeds
//! the lock-free render ring that the audio callback consumes.
//!
//! Data flow: UDP thread -> [jitter buffer] -> playout tick (this module)
//! -> [render ring] -> audio callback. Locks exist only on the left side of
//! this file; the audio callback still pops samples with no locks.

use std::collections::VecDeque;

use crate::drift::DriftController;
use crate::jitter::JitterBuffer;

pub struct PlayoutConfig {
    pub sample_rate: u32,
    pub channels: usize,
    /// Added to the adaptive jitter target: manual lip-sync hold.
    pub extra_delay_ms: f64,
}

/// Frame-interpolating resampler for ratios ~1.0 (drift correction only).
///
/// `step` is input frames consumed per output frame (= 1/ratio). Linear
/// interpolation between adjacent frames; missing input yields zero pads
/// (silence), never a repeated sample.
pub struct FrameInterpolator {
    channels: usize,
    buf: VecDeque<f32>,
    /// Fractional read position in frames (>= 1 while at least two frames
    /// are available; frame floor(pos) and floor(pos)+1 are interpolated).
    pos: f64,
}

impl FrameInterpolator {
    pub fn new(channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            buf: VecDeque::new(),
            pos: 0.0,
        }
    }

    pub fn push_pcm(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().map(|&s| s as f32 / 32768.0));
    }

    /// Number of whole frames currently available.
    pub fn available_frames(&self) -> usize {
        self.buf.len() / self.channels
    }

    /// Produce `out_frames` frames of interleaved f32 samples. Input that
    /// runs out is filled with silence, never a repeated sample.
    pub fn render(&mut self, out_frames: usize, step: f64) -> Vec<f32> {
        let ch = self.channels;
        let mut out = vec![0.0f32; out_frames * ch];
        for i in 0..out_frames {
            let idx = self.pos as usize;
            // Need frames idx and idx+1.
            if idx + 1 >= self.available_frames() {
                // Underrun: the rest of this tick is silence. Leave the
                // read position pointing at the last real frame so audio
                // resumes cleanly when data arrives.
                self.pos = idx as f64;
                out[i * ch..].fill(0.0);
                return out;
            }
            let frac = (self.pos - idx as f64) as f32;
            let (a_base, b_base) = (idx * ch, (idx + 1) * ch);
            for c in 0..ch {
                let a = self.buf[a_base + c];
                let b = self.buf[b_base + c];
                out[i * ch + c] = a + (b - a) * frac;
            }
            self.pos += step;
            // Drop fully-consumed frames but keep one lead-in frame for the
            // next interpolation pair.
            let drop = self.pos as usize;
            for _ in 0..drop {
                self.pos -= 1.0;
                for _ in 0..ch {
                    self.buf.pop_front();
                }
            }
        }
        out
    }
}

pub struct Playout {
    pub cfg: PlayoutConfig,
    drift: DriftController,
    interp: FrameInterpolator,
    started: bool,
    /// Frames handed to the render ring in total (including prefill
    /// silence; see `level_ms` comment for the approximation this makes).
    produced_frames: u64,
    /// Fractional out-frame accumulator so rounding never biases the pace.
    out_accum: f64,
    pub underrun_ticks: u64,
    /// Audio frames discarded to recover from big level overshoots.
    pub flushed_frames: u64,
}

impl Playout {
    pub fn new(cfg: PlayoutConfig) -> Self {
        // channels pulled out before cfg moves into the struct.
        let channels = cfg.channels;
        Self {
            cfg,
            drift: DriftController::new(crate::drift::DriftConfig::default()),
            interp: FrameInterpolator::new(channels),
            started: false,
            produced_frames: 0,
            out_accum: 0.0,
            underrun_ticks: 0,
            flushed_frames: 0,
        }
    }

    /// Total audio currently ahead of the DAC, in ms.
    pub fn level_ms(&self, consumed_frames: u64, jitter: &JitterBuffer) -> f64 {
        let ring_frames = self.produced_frames.saturating_sub(consumed_frames);
        let ring_ms = ring_frames as f64 * 1000.0 / self.cfg.sample_rate as f64;
        // Approximation: prefill/underrun silence counts as buffered audio.
        // It self-corrects (silence passes through, real audio is not
        // re-counted), and the drift controller has a deadband anyway.
        ring_ms + jitter.buffered_ms()
    }

    pub fn target_ms(&self, jitter: &JitterBuffer) -> f64 {
        (jitter.target_ms() + self.cfg.extra_delay_ms).min(200.0 + self.cfg.extra_delay_ms)
    }

    /// One tick of `dt_secs`. `consumed_frames` is the audio callback's
    /// running total. Returns interleaved f32 samples for the render ring.
    ///
    /// The jitter buffer is passed by &mut borrow (guarded by the session
    /// Mutex in net.rs): the UDP thread hands it frames, we take them out.
    pub fn tick(
        &mut self,
        dt_secs: f64,
        consumed_frames: u64,
        jitter: &mut JitterBuffer,
    ) -> Vec<f32> {
        let level = self.level_ms(consumed_frames, jitter);
        let target = self.target_ms(jitter);

        // Prefill: hold real audio in the jitter buffer until we have the
        // target cushion. Output silence meanwhile — that is exactly the
        // latency the buffer represents.
        if !self.started {
            if level >= target * 0.7 {
                self.started = true;
            } else {
                let n = (dt_secs * self.cfg.sample_rate as f64).round() as usize;
                self.produced_frames += n as u64;
                return vec![0.0f32; n * self.cfg.channels];
            }
        }

        // Big transient (a bursty sender, or a resume after a stall):
        // the drift ratio is capped at 0.05 % precisely so it can never
        // pitch-shift noticeably — which means it also can't chew through
        // 500 ms of excess audio in reasonable time. Discard whole frames
        // until we are back near the cushion. This trades a chunk of audio
        // (a skip, like aggressive fast-forward) for latency — the right
        // call for a live stream, where stale audio is worthless.
        if level - target > 200.0 {
            let mut flush_target_us =
                ((level - target - 50.0) * 1_000.0) as u64;
            while flush_target_us > 0 {
                match jitter.pop() {
                    Some(frame) => {
                        self.flushed_frames += 1;
                        flush_target_us = flush_target_us.saturating_sub(frame.duration_us);
                    }
                    None => break,
                }
            }
        }

        let ratio = self.drift.update(level - target, dt_secs);

        // Exact fractional frame count; carry the remainder to the next tick.
        self.out_accum += dt_secs * self.cfg.sample_rate as f64 * ratio;
        let out_frames = self.out_accum as usize;
        self.out_accum -= out_frames as f64;
        if out_frames == 0 {
            return Vec::new();
        }

        // Make sure the interpolator has enough input frames: pull whole
        // frames from the jitter buffer until there's headroom, or pad
        // silence on underrun (once counted per starved tick).
        let want_input_frames = (out_frames as f64 / ratio).ceil() as usize + 2;
        let mut starved = false;
        while self.interp.available_frames() < want_input_frames {
            match jitter.pop() {
                Some(frame) => self.interp.push_pcm(&frame.samples),
                None => {
                    starved = true;
                    break;
                }
            }
        }
        if starved {
            self.underrun_ticks += 1;
            // render() zero-pads whatever the buffer can't cover, which is
            // silence, never a repeat of stale samples.
        }

        let step = 1.0 / ratio;
        let samples = self.interp.render(out_frames, step);
        self.produced_frames += out_frames as u64;
        samples
    }

    /// Exposed for stats logging by the playout thread.
    pub fn produced_frames(&self) -> u64 {
        self.produced_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jitter::Frame;

    fn jb() -> JitterBuffer {
        JitterBuffer::new(1, 48_000, 30.0)
    }

    fn playout() -> Playout {
        Playout::new(PlayoutConfig {
            sample_rate: 48_000,
            channels: 1,
            extra_delay_ms: 0.0,
        })
    }

    #[test]
    fn interpolator_step_one_is_near_passthrough() {
        let mut it = FrameInterpolator::new(1);
        let ramp: Vec<i16> = (0..480).map(|i| (i as f64 * 60.0) as i16).collect();
        it.push_pcm(&ramp);
        let out = it.render(478, 1.0);
        for (i, expected) in ramp.iter().enumerate().take(478) {
            let got = out[i];
            let want = *expected as f32 / 32768.0;
            assert!(
                (got - want).abs() < 2.0 / 32768.0,
                "frame {i}: got {got} want {want}"
            );
        }
    }

    #[test]
    fn interpolator_slight_stretch_interpolates() {
        // step 2.0: consumes every other frame -> output follows a smooth
        // ramp using linear interpolation between consumed points.
        let mut it = FrameInterpolator::new(1);
        let v: Vec<i16> = (0..100).map(|i| i as i16 * 10).collect();
        it.push_pcm(&v);
        let out = it.render(49, 2.0);
        let scale = 32768.0f32;
        // first out frame = input frame 0; second = input frame 2, etc.
        for (i, &got) in out.iter().take(40).enumerate() {
            let want = (2 * i) as f32 * 10.0 / scale;
            assert!((got - want).abs() < 0.5 / scale, "{i}: {got}");
        }
    }

    #[test]
    fn interpolator_underrun_yields_silence_not_repeats() {
        let mut it = FrameInterpolator::new(1);
        it.push_pcm(&[1000, 2000, 3000, 4000]);
        let out = it.render(10, 1.0);
        // first two real frames produce a couple of outputs; after that:
        for &s in &out[4..] {
            assert_eq!(s, 0.0, "underrun must not repeat stale samples");
        }
    }

    #[test]
    fn big_burst_is_flushed_back_to_cushion() {
        let mut p = playout();
        let mut jitter = jb();
        // A sender dumps 500 ms at once (level >> target + 200).
        for i in 0..100u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![100i16; 240], 1, 48_000),
                i as f64 * 0.1,
            );
        }
        let out = p.tick(0.01, 0, &mut jitter);
        assert!(!out.is_empty());
        assert!(p.flushed_frames > 80, "should chew through the burst: {}", p.flushed_frames);
        // Now the level sits near the cushion, not near 500 ms.
        let level = p.level_ms(0, &jitter);
        assert!(level < 120.0, "level after flush: {level}");
    }

    #[test]
    fn starved_tick_counts_one_underrun() {
        let mut p = playout();
        let mut jitter = jb();
        // Start with cushion.
        for i in 0..12u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![7i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        p.tick(0.01, 0, &mut jitter);
        assert!(p.started);
        jitter.clear();
        p.tick(0.01, 960, &mut jitter);
        assert_eq!(p.underrun_ticks, 1, "one starved tick must count as one, not per pad frame");
    }

    #[test]
    fn prefill_holds_back_until_target_cushion() {
        let mut p = playout();
        let mut jitter = jb();
        // Tick with an empty buffer: output must be pure silence and the
        // engine must not have started.
        let out = p.tick(0.01, 0, &mut jitter);
        assert_eq!(out.len(), 480);
        assert!(out.iter().all(|&s| s == 0.0));
        assert!(!p.started);
        // Fill to the target (30 ms base -> 6 frames of 5 ms).
        for i in 0..8u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![12_345i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        let out = p.tick(0.01, 480, &mut jitter); // consumed keeps pace with prefill
        assert!(p.started, "level {} should start", p.level_ms(480, &jitter));
        // Once started the real samples flow (non-zero after a moment).
        assert!(out.iter().any(|&s| s != 0.0));
    }

    #[test]
    fn long_run_produces_steady_frame_rate() {
        let mut p = playout();
        let mut jitter = jb();
        // Feed 3 seconds of audio up front.
        for i in 0..600u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![500i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        let mut produced = 0u64;
        for _ in 0..300 {
            let out = p.tick(0.01, produced, &mut jitter);
            produced += (out.len() / p.cfg.channels) as u64;
        }
        // 300 ticks x 10 ms = 3 s at 48 kHz = 144_000 frames, within the
        // drift budget (level error was large but ratio caps at 0.05 %).
        let expected = 144_000.0f64;
        let got = produced as f64;
        let err = (got - expected) / expected;
        assert!(err.abs() < 0.001, "produced {got}, err {err}");
    }

    #[test]
    fn underrun_counts_and_recovers() {
        let mut p = playout();
        let mut jitter = jb();
        // Start with cushion.
        for i in 0..12u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![777i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        let mut consumed = 0u64;
        let out = p.tick(0.01, consumed, &mut jitter);
        consumed += out.len() as u64;
        assert!(p.started);
        // Run the buffer dry: 12 frames = 60 ms; drain it all.
        for _ in 0..30 {
            let out = p.tick(0.01, consumed, &mut jitter);
            consumed += out.len() as u64;
        }
        assert!(p.underrun_ticks > 0);
        // New data arrives -> next ticks non-silent again.
        for i in 12..40u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![777i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        let out = p.tick(0.01, consumed, &mut jitter);
        assert!(out.iter().any(|&s| s != 0.0));
    }
}
