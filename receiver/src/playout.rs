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

    /// Produce `out_frames` frames of interleaved f32 samples, plus the
    /// count of those frames that came from real input (the rest, if any,
    /// is underrun silence — never a repeated sample).
    pub fn render(&mut self, out_frames: usize, step: f64) -> (Vec<f32>, usize) {
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
                return (out, i);
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
        (out, out_frames)
    }
}

pub struct Playout {
    pub cfg: PlayoutConfig,
    drift: DriftController,
    interp: FrameInterpolator,
    started: bool,
    /// Frames of REAL audio handed to the render ring, and the device-
    /// consumed count captured when the first real audio went in.
    /// Prefill/startup silence is deliberately excluded from the level
    /// the drift controller sees: counting it (produced-consumed) would
    /// treat the stream-open handshake latency (~100 ms of dead air) as
    /// buffered audio and bias both the level estimate and the drift
    /// correction.
    produced_real: u64,
    /// Total frames handed to the ring (for session stats).
    produced_frames: u64,
    /// Fractional out-frame accumulator so rounding never biases the pace.
    out_accum: f64,
    pub underrun_ticks: u64,
    /// Audio frames discarded to recover from big level overshoots.
    pub flushed_frames: u64,
    /// Device consumption seen at the previous tick (pull-pace master).
    last_consumed: u64,
    /// Whether the one-time ring seed SILENCE has been prepended.
    seed_prepended: bool,
    seed_frames: usize,
}

/// Fixed render-ring cushion, ms. The playout thread mirrors whatever the
/// device consumed; this seed is the one-time head-start so callbacks
/// never find an empty ring mid-stream. It is part of the delay budget and
/// of the level target (the drift controller aims at
/// jitter-target + extra-delay + this).
pub const RING_SEED_MS: f64 = 48.0;

impl Playout {
    pub fn new(cfg: PlayoutConfig) -> Self {
        // channels pulled out before cfg moves into the struct.
        let channels = cfg.channels;
        let seed_frames = (RING_SEED_MS * cfg.sample_rate as f64 / 1_000.0).ceil() as usize;
        Self {
            cfg,
            drift: DriftController::new(crate::drift::DriftConfig::default()),
            interp: FrameInterpolator::new(channels),
            started: false,
            produced_real: 0,
            produced_frames: 0,
            out_accum: 0.0,
            underrun_ticks: 0,
            flushed_frames: 0,
            last_consumed: 0,
            seed_prepended: false,
            seed_frames,
        }
    }

    /// Audio currently ahead of the DAC, in ms. The device pops the seed
    /// silence first (FIFO), so of everything consumed, the first
    /// `seed_frames` were silence: real audio still in the ring is
    /// produced_real minus the real part of consumption.
    pub fn level_ms(&self, consumed_frames: u64, jitter: &JitterBuffer) -> f64 {
        let real_popped = consumed_frames.saturating_sub(self.seed_frames as u64);
        let ring_real = self.produced_real.saturating_sub(real_popped);
        let ring_ms = ring_real as f64 * 1000.0 / self.cfg.sample_rate as f64;
        ring_ms + jitter.buffered_ms()
    }

    pub fn target_ms(&self, jitter: &JitterBuffer) -> f64 {
        (jitter.target_ms() + self.cfg.extra_delay_ms).min(200.0 + self.cfg.extra_delay_ms)
    }

    /// One tick of `dt_secs`. `consumed_frames` is the audio callback's
    /// running total of frames POPPED from the ring. Returns interleaved
    /// f32 samples for the render ring.
    ///
    /// Pacing is DEVICE-DRIVEN, not wall-clock-driven: we replace exactly
    /// what the device consumed since the last tick (times the drift
    /// ratio), plus a one-time ring seed. A wall-clock-paced producer
    /// permanently accumulates whatever lead the device takes to start
    /// (~150 ms measured on WASAPI) — the ring depth then exceeds the
    /// drift controller's ±0.05 % reach and sticks. With the device clock
    /// as master, ring depth is by construction ~= the seed, and the
    /// jitter buffer holds the adaptive target.
    ///
    /// The jitter buffer is passed by &mut borrow (guarded by the session
    /// Mutex in net.rs): the UDP thread hands it frames, we take them out.
    pub fn tick(
        &mut self,
        dt_secs: f64,
        consumed_frames: u64,
        jitter: &mut JitterBuffer,
    ) -> Vec<f32> {
        let delta_consumed = consumed_frames.saturating_sub(self.last_consumed);
        self.last_consumed = consumed_frames;

        let level = self.level_ms(consumed_frames, jitter);
        let target = self.target_ms(jitter) + RING_SEED_MS;

        // Prefill: hold real audio in the jitter buffer until the adaptive
        // target cushion is there. Nothing is pushed meanwhile; the device
        // plays its own underrun silence until then.
        if !self.started {
            if jitter.buffered_ms() >= self.target_ms(jitter) {
                self.started = true;
            } else {
                return Vec::new();
            }
        }

        // Big transient (a bursty sender, a resume after a stall, or an
        // adaptive target that just grew): the drift ratio is capped at
        // 0.05 % precisely so it can never pitch-shift noticeably — which
        // means it also can't chew through hundreds of ms of excess audio
        // in reasonable time. When the level sits >100 ms over target,
        // discard whole frames down to target+30. This trades a chunk of
        // audio (a skip, like aggressive fast-forward) for latency — the
        // right call for a live stream, where stale audio is worthless.
        // Ordinary jitter never reaches the trigger: the adaptive target
        // IS the headroom for it.
        if level - target > 100.0 {
            let mut flush_target_us = ((level - target - 30.0) * 1_000.0) as u64;
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

        // Replace exactly what the device consumed, corrected only by the
        // pitch-safe drift ratio. Ring depth is NOT level-controlled on
        // purpose: it is a fixed seed of silence (prepended once below),
        // and the jitter buffer — fed by the gate, bounded by flush —
        // holds the adaptive target. (An earlier ±1 % proportional term
        // tried to steer total level through the output volume; it just
        // robbed the jitter buffer to grow the ring, and starved.)
        self.out_accum += delta_consumed as f64 * ratio;
        let out_frames = self.out_accum as usize;
        self.out_accum -= out_frames as f64;
        let seeding = !self.seed_prepended;
        if out_frames == 0 && !seeding {
            return Vec::new();
        }

        // Make sure the interpolator has enough input frames: pull whole
        // frames from the jitter buffer until there's headroom, or pad
        // silence on underrun (once counted per starved tick).
        let mut rendered: Vec<f32> = Vec::new();
        if out_frames > 0 {
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
                // Tell the estimator: this link needs more cushion. Bounded
                // to the 10-200 ms band and decayed by calm seconds, so a
                // one-off hiccup doesn't stick (observed live: bursty
                // lossless link oscillating 5..95 ms stuttered forever
                // because only loss events raised the target).
                jitter.report_underrun();
                // render() zero-pads whatever the buffer can't cover, which
                // is silence, never a repeat of stale samples.
            }
            let step = 1.0 / ratio;
            let (samples, real_frames) = self.interp.render(out_frames, step);
            self.produced_real += real_frames as u64;
            rendered = samples;
        }

        // One-time seed: silence ahead of the first real audio, so the
        // device never starves between ticks and ring depth is a known
        // constant in the level math.
        let mut out = if seeding {
            self.seed_prepended = true;
            vec![0.0f32; self.seed_frames * self.cfg.channels]
        } else {
            Vec::with_capacity(rendered.len())
        };
        out.append(&mut rendered);
        self.produced_frames += out.len() as u64 / self.cfg.channels as u64;
        out
    }

    /// Exposed for stats logging by the playout thread.
    pub fn produced_frames(&self) -> u64 {
        self.produced_frames
    }

    /// (produced_real, consumed, ring_real_frames) for diagnostics.
    pub fn ring_diag(&self, consumed_frames: u64) -> (u64, u64, u64) {
        let real_popped = consumed_frames.saturating_sub(self.seed_frames as u64);
        (
            self.produced_real,
            consumed_frames,
            self.produced_real.saturating_sub(real_popped),
        )
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
        let (out, _) = it.render(478, 1.0);
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
        let (out, _) = it.render(49, 2.0);
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
        let (out, _) = it.render(10, 1.0);
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
        assert!(
            p.flushed_frames > 70,
            "should chew through the burst: {}",
            p.flushed_frames
        );
        // With the pull model the ring only ever holds the seed (~48 ms),
        // so the total level stays bounded near target + cushion.
        let level = p.level_ms(0, &jitter);
        assert!(level < 200.0, "level after flush: {level}");
    }

    #[test]
    fn starved_tick_counts_one_underrun() {
        let mut p = playout();
        let mut jitter = jb();
        // Enough cushion to pass the prefill gate (12 frames = 60 ms).
        for i in 0..12u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![7i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        p.tick(0.01, 0, &mut jitter);
        assert!(p.started);
        jitter.clear();
        // The device consumed exactly the seed we just produced.
        p.tick(0.01, p.seed_frames as u64, &mut jitter);
        assert_eq!(
            p.underrun_ticks, 1,
            "one starved tick must count as one, not per pad frame"
        );
    }

    #[test]
    fn prefill_holds_back_until_target_cushion() {
        let mut p = playout();
        let mut jitter = jb();
        // Tick with an empty buffer: nothing is produced, not started.
        let out = p.tick(0.01, 0, &mut jitter);
        assert!(out.is_empty());
        assert!(!p.started);
        // Fill above the gate (0.7 x (30 + 48 seed) = 55 ms -> 12 frames).
        for i in 0..12u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![12_345i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        let out = p.tick(0.01, 0, &mut jitter);
        assert!(p.started, "level should pass the gate");
        assert_eq!(
            out.len() / p.cfg.channels,
            p.seed_frames,
            "first push is exactly the ring seed"
        );
        assert!(
            out.iter().all(|&s| s == 0.0),
            "the seed is silence by design"
        );
        // Once the device has consumed the seed, pushes carry real audio.
        let out = p.tick(0.01, p.seed_frames as u64, &mut jitter);
        assert!(!out.is_empty());
        assert!(
            out.iter().any(|&s| s != 0.0),
            "real audio flows after the seed"
        );
    }

    /// Simulated feedback device: pops at most `per_tick` frames per tick
    /// and only what the ring actually holds — exactly what a real sound
    /// card does. (A naive `min(wall_time, produced)` model lets the
    /// device retro-actively swallow bursts in one tick, which is how the
    /// first version of these tests produced phantom underruns.)
    struct FakeDevice {
        ring: u64,
        popped_total: u64,
        per_tick: u64,
    }

    impl FakeDevice {
        fn new() -> Self {
            Self {
                ring: 0,
                popped_total: 0,
                per_tick: 480,
            }
        }
        /// Call before each tick: applies device consumption since last tick.
        fn advance(&mut self) -> u64 {
            let pop = self.per_tick.min(self.ring);
            self.ring -= pop;
            self.popped_total += pop;
            self.popped_total
        }
        fn push(&mut self, frames: u64) {
            self.ring += frames;
        }
    }

    fn feed_frames(jitter: &mut JitterBuffer, seq: &mut u32, n: u64, t_ms: f64) {
        for i in 0..n {
            jitter.push(
                Frame::pcm(
                    *seq + i as u32,
                    u64::from(*seq + i as u32) * 5_000,
                    vec![500i16; 240],
                    1,
                    48_000,
                ),
                t_ms + i as f64 * 5.0,
            );
        }
        *seq += n as u32;
    }

    #[test]
    fn total_output_tracks_the_device_clock() {
        let mut p = playout();
        let mut jitter = jb();
        let mut dev = FakeDevice::new();
        let mut seq = 0u32;
        for tick in 0..300u64 {
            feed_frames(&mut jitter, &mut seq, 2, tick as f64 * 10.0); // 480/tick, real time
            let consumed = dev.advance();
            let out = p.tick(0.01, consumed, &mut jitter);
            dev.push((out.len() / p.cfg.channels) as u64);
        }
        // Ring holds seed + bounded slack; produced can't outrun the device.
        assert!(
            dev.ring < p.seed_frames as u64 * 4,
            "ring depth {} — producer ran away",
            dev.ring
        );
        assert!(
            dev.ring > 256,
            "ring depth {} — device will starve",
            dev.ring
        );
    }

    #[test]
    fn level_converges_to_target_and_stays() {
        // With real-time input and the FakeDevice feedback model, the
        // seed + prefill cushion must converge to ~target (78 ms) with
        // zero underruns.
        let mut p = playout();
        let mut jitter = jb();
        let mut dev = FakeDevice::new();
        let mut seq = 0u32;
        for tick in 0..400u64 {
            feed_frames(&mut jitter, &mut seq, 2, tick as f64 * 10.0);
            let consumed = dev.advance();
            let out = p.tick(0.01, consumed, &mut jitter);
            dev.push((out.len() / p.cfg.channels) as u64);
        }
        assert_eq!(p.underrun_ticks, 0, "underruns: {}", p.underrun_ticks);
        let level = p.level_ms(dev.popped_total, &jitter);
        assert!(
            level > 40.0 && level < 130.0,
            "level should sit near target (78), got {level}"
        );
    }

    #[test]
    #[ignore = "temporary debug trace"]
    fn debug_trace_convergence() {
        let mut p = playout();
        let mut jitter = jb();
        let mut dev = FakeDevice::new();
        let mut seq = 0u32;
        for tick in 0..40u64 {
            feed_frames(&mut jitter, &mut seq, 2, tick as f64 * 10.0);
            let consumed = dev.advance();
            let out = p.tick(0.01, consumed, &mut jitter);
            let o = out.len() / p.cfg.channels;
            dev.push(o as u64);
            if tick < 25 {
                eprintln!(
                    "t{tick} started={} jb_buf={:.0} dev_ring={} consumed={consumed} out={o} lvl={:.0} und={} flush={} seed_pre={}",
                    p.started,
                    jitter.buffered_ms(),
                    dev.ring,
                    p.level_ms(consumed, &jitter),
                    p.underrun_ticks,
                    p.flushed_frames,
                    p.seed_prepended,
                );
            }
        }
    }

    #[test]
    fn underrun_counts_and_recovers() {
        let mut p = playout();
        let mut jitter = jb();
        for i in 0..12u32 {
            jitter.push(
                Frame::pcm(i, i as u64 * 5_000, vec![777i16; 240], 1, 48_000),
                i as f64 * 5.0,
            );
        }
        let mut consumed = 0u64;
        let out = p.tick(0.01, consumed, &mut jitter);
        consumed += (out.len() / p.cfg.channels) as u64; // the seed
        assert!(p.started);
        // Keep ticking while the buffer runs dry.
        for _ in 0..30 {
            let out = p.tick(0.01, consumed, &mut jitter);
            consumed += (out.len() / p.cfg.channels) as u64;
        }
        assert!(p.underrun_ticks > 0);
        // New data arrives -> next tick is non-silent again.
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
