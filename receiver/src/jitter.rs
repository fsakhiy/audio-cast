//! Frame-level jitter buffer between the network thread and the playout
//! thread.
//!
//! Design notes:
//! - The sender is real-time paced, so *time* is what we adapt on, not
//!   packet counts: every frame carries its capture timestamp and a
//!   duration in µs.
//! - The queue is a `VecDeque<Frame>`; `push`/`pop` run on two threads
//!   behind one `Mutex` — but never on the audio callback (the callback
//!   only sees the lock-free render ring that the playout thread fills).
//!   A mutex here is acceptable because both sides are ordinary threads
//!   with milliseconds of slack; the realtime thread keeps its no-lock
//!   contract.
//! - Lost packets are replaced by silence frames so the media timeline
//!   advances honestly (playout never "skips ahead" past lost audio, which
//!   would break lip-sync for the rest of the session).

use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub sequence: u32,
    /// Sender-side monotonic capture clock, microseconds.
    pub capture_ts_us: u64,
    /// PCM: interleaved s16, exactly `duration_us * rate / 1e6 * channels`
    /// long. Opus frames are decoded to PCM before entering here.
    pub samples: Vec<i16>,
    pub duration_us: u64,
    /// True for gap-fill silence (no jitter-estimator input from these).
    pub synthetic: bool,
}

impl Frame {
    /// Build a real PCM frame; duration is derived from its length.
    pub fn pcm(
        sequence: u32,
        capture_ts_us: u64,
        samples: Vec<i16>,
        channels: usize,
        sample_rate: u32,
    ) -> Self {
        let frames = samples.len() / channels.max(1);
        Self {
            sequence,
            capture_ts_us,
            samples,
            duration_us: (frames as u64) * 1_000_000 / (sample_rate.max(1)) as u64,
            synthetic: false,
        }
    }

    fn silence(
        sequence: u32,
        capture_ts_us: u64,
        duration_us: u64,
        channels: usize,
        sample_rate: u32,
    ) -> Self {
        let samples = (duration_us * sample_rate as u64 / 1_000_000).max(1) as usize * channels;
        Self {
            sequence,
            capture_ts_us,
            samples: vec![0i16; samples],
            duration_us,
            synthetic: true,
        }
    }
}

/// Adaptive jitter target from inter-arrival deviation.
///
/// Wide-band-voice style (RFC 3550 family): an exponential moving average
/// of |arrival_delta - send_delta| feeds a linear target
/// `base + k * ema`, plus an event bump (loss/eviction/late) that decays
/// over time. Clamped to the configured 10–200 ms band around the base.
pub struct JitterEstimator {
    ema_ms: f64,
    bump_ms: f64,
    last_send_ts_us: Option<u64>,
    last_arrival_ms: Option<f64>,
    base_ms: f64,
    min_ms: f64,
    max_ms: f64,
    k: f64,
}

impl JitterEstimator {
    pub fn new(base_ms: f64) -> Self {
        let base = base_ms.clamp(10.0, 200.0);
        Self {
            ema_ms: 0.0,
            bump_ms: 0.0,
            last_send_ts_us: None,
            last_arrival_ms: None,
            base_ms: base,
            min_ms: 10.0,
            max_ms: 200.0,
            k: 4.0,
        }
    }

    /// Feed one real (non-synthetic) arrival. `arrival_ms` is any
    /// monotonic clock (e.g. `Instant::elapsed` in ms).
    pub fn observe_arrival(&mut self, send_ts_us: u64, arrival_ms: f64) {
        if let (Some(prev_send), Some(prev_arrival)) = (self.last_send_ts_us, self.last_arrival_ms)
        {
            let send_delta_ms = send_ts_us.saturating_sub(prev_send) as f64 / 1_000.0;
            let arrival_delta_ms = arrival_ms - prev_arrival;
            // Negative arrival deltas mean the sender's clock moved
            // backwards relative to our clock — impossible in reality, so
            // treat as uninformative rather than huge jitter.
            if send_delta_ms > 0.0 && arrival_delta_ms >= 0.0 {
                let dev = (arrival_delta_ms - send_delta_ms).clamp(0.0, 200.0);
                // alpha = 1/8: smooths spikes, reacts within ~1 s at 200 pps.
                self.ema_ms = self.ema_ms * 0.875 + dev * 0.125;
            }
            // Decay the event bump ~5 % per observation.
            self.bump_ms *= 0.95;
            if self.bump_ms < 0.5 {
                self.bump_ms = 0.0;
            }
        }
        self.last_send_ts_us = Some(send_ts_us);
        self.last_arrival_ms = Some(arrival_ms);
    }

    /// A loss/eviction/late-drop happened: demand headroom now.
    pub fn report_event(&mut self) {
        self.bump_ms = (self.bump_ms * 2.0 + 5.0).min(self.max_ms - self.base_ms);
    }

    pub fn target_ms(&self) -> f64 {
        (self.base_ms + self.k * self.ema_ms + self.bump_ms).clamp(self.min_ms, self.max_ms)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u64,
    pub lost: u64,
    pub late_dropped: u64,
    pub evicted: u64,
    pub evicted_samples: u64,
}

pub struct JitterBuffer {
    queue: VecDeque<Frame>,
    channels: usize,
    sample_rate: u32,
    next_seq: Option<u32>,
    stats: JitterStats,
    estimator: JitterEstimator,
    /// Typical real-frame duration, learned from the first frames; used to
    /// size gap-fill silence (Opus frames may vary, PCM ones don't).
    typical_frame_us: Option<u64>,
    /// Max queue length in ms before oldest frames are evicted.
    max_queued_ms: f64,
    /// Playback watermark: all audio ending before this has been consumed.
    last_played_end_us: u64,
}

impl JitterBuffer {
    pub fn new(channels: usize, sample_rate: u32, base_target_ms: f64) -> Self {
        Self {
            queue: VecDeque::new(),
            channels: channels.max(1),
            sample_rate: sample_rate.max(8_000),
            next_seq: None,
            stats: JitterStats::default(),
            estimator: JitterEstimator::new(base_target_ms),
            typical_frame_us: None,
            // Eviction means playout has stalled for ~0.8 s: a real
            // anomaly, not normal operation (targets top out at 200 ms).
            max_queued_ms: 800.0,
            last_played_end_us: 0,
        }
    }

    pub fn target_ms(&self) -> f64 {
        self.estimator.target_ms()
    }

    /// Playout starved (buffer ran dry): demand more headroom immediately.
    /// Without this, bursty-but-lossless links never raise the target and
    /// stutter forever (observed live: arrivals in 90 ms bursts, buffer
    /// oscillating 5..95 ms, loss ~0).
    pub fn report_underrun(&mut self) {
        self.estimator.report_event();
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    pub fn buffered_ms(&self) -> f64 {
        let us: u64 = self.queue.iter().map(|f| f.duration_us).sum();
        us as f64 / 1_000.0
    }

    #[allow(dead_code)] // read-side accessor, exercised by tests
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Insert a received frame. Returns false if it was dropped as late.
    /// Detected sequence gaps are filled with synthetic silence.
    pub fn push(&mut self, frame: Frame, arrival_ms: f64) -> bool {
        let end = frame.capture_ts_us + frame.duration_us;
        // Already played past this frame's whole span: useless now.
        if end <= self.last_played_end_us {
            self.stats.received += 1;
            self.stats.late_dropped += 1;
            self.estimator.report_event();
            return false;
        }

        self.stats.received += 1;
        self.typical_frame_us.get_or_insert(frame.duration_us);

        // Already-accounted sequence numbers (late or reordered arrivals):
        // their slot either played already or holds gap-fill silence.
        if let Some(next) = self.next_seq
            && frame.sequence < next
        {
            self.stats.late_dropped += 1;
            self.estimator.report_event();
            // If the placeholder is still unplayed silence, upgrade it to
            // the real audio that just turned up. Otherwise drop.
            if let Some(pos) = self
                .queue
                .iter()
                .position(|f| f.sequence == frame.sequence && f.synthetic)
            {
                self.queue[pos] = frame;
                return true;
            }
            return false;
        }

        // Is this the newest thing we've seen? Gap logic only makes sense
        // against the leading edge.
        let at_leading_edge = self
            .queue
            .back()
            .is_none_or(|b| frame.capture_ts_us >= b.capture_ts_us);

        if at_leading_edge {
            if let Some(next) = self.next_seq {
                if frame.sequence > next {
                    let gap = frame.sequence - next;
                    self.stats.lost += gap as u64;
                    self.estimator.report_event();
                    let unit_us = self.typical_frame_us.unwrap_or(frame.duration_us);
                    // Place the silence right after the current tail so it
                    // sits *before* this frame in time.
                    let mut ts = self
                        .queue
                        .back()
                        .map(|b| b.capture_ts_us + b.duration_us)
                        .unwrap_or(frame.capture_ts_us.saturating_sub(gap as u64 * unit_us));
                    for lost_seq in next..frame.sequence {
                        let s =
                            Frame::silence(lost_seq, ts, unit_us, self.channels, self.sample_rate);
                        self.queue.push_back(s);
                        ts += unit_us;
                    }
                }
                self.next_seq = Some(frame.sequence + 1);
            } else {
                self.next_seq = Some(frame.sequence + 1);
            }
            self.estimator
                .observe_arrival(frame.capture_ts_us, arrival_ms);
        } else {
            // Older timestamp but a sequence we have not accounted for:
            // genuine reorder inside the buffering window. Keep it (sorted
            // insert below) and count the reorder.
            self.stats.late_dropped += 1;
            self.estimator.report_event();
        }

        // Keep the queue sorted by capture timestamp (small backward scan;
        // queues are short).
        let mut idx = self.queue.len();
        while idx > 0 && self.queue[idx - 1].capture_ts_us > frame.capture_ts_us {
            idx -= 1;
        }
        self.queue.insert(idx, frame);

        // Overrun policy: evict OLDEST (project rule), never the newest.
        while self.buffered_ms() > self.max_queued_ms {
            match self.queue.pop_front() {
                Some(old) => {
                    self.stats.evicted += 1;
                    self.stats.evicted_samples += old.samples.len() as u64;
                    self.last_played_end_us = self
                        .last_played_end_us
                        .max(old.capture_ts_us + old.duration_us);
                    self.estimator.report_event();
                }
                None => break,
            }
        }
        true
    }

    /// Take the oldest frame for rendering. The prefill/water-level policy
    /// lives in the playout thread, not here.
    pub fn pop(&mut self) -> Option<Frame> {
        let frame = self.queue.pop_front()?;
        self.last_played_end_us = self
            .last_played_end_us
            .max(frame.capture_ts_us + frame.duration_us);
        Some(frame)
    }

    /// Drain everything (session teardown / stream restart).
    #[allow(dead_code)] // sessions currently drop the whole buffer instead
    pub fn clear(&mut self) {
        self.queue.clear();
        self.next_seq = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32, ts_us: u64) -> Frame {
        // 240 samples @ 48 kHz stereo = 5 ms
        Frame::pcm(seq, ts_us, vec![1i16; 480], 2, 48_000)
    }

    #[test]
    fn pcm_frame_duration_derived_from_length() {
        let f = frame(0, 0);
        assert_eq!(f.duration_us, 5_000);
    }

    #[test]
    fn push_then_pop_in_order() {
        let mut jb = JitterBuffer::new(2, 48_000, 30.0);
        jb.push(frame(0, 0), 0.0);
        jb.push(frame(1, 5_000), 5.0);
        assert_eq!(jb.pop().unwrap().sequence, 0);
        assert_eq!(jb.pop().unwrap().sequence, 1);
        assert!(jb.pop().is_none());
    }

    #[test]
    fn sequence_gap_inserts_silence_of_matching_duration() {
        let mut jb = JitterBuffer::new(2, 48_000, 30.0);
        jb.push(frame(0, 0), 0.0);
        jb.push(frame(3, 15_000), 15.0); // seq 1 and 2 lost
        let s = jb.stats();
        assert_eq!(s.lost, 2);
        // queue: real 0, silence, silence, real 3
        let seqs: Vec<u32> = std::iter::from_fn(|| jb.pop())
            .map(|f| f.sequence)
            .collect();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
        assert_eq!(jb.buffered_ms(), 0.0);
    }

    #[test]
    fn reordered_real_frame_replaces_its_gap_fill_silence() {
        let mut jb = JitterBuffer::new(2, 48_000, 30.0);
        jb.push(frame(0, 0), 0.0);
        jb.push(frame(2, 10_000), 10.0); // gap -> silence placeholder for seq 1
        jb.push(frame(1, 5_000), 11.0); // the real audio shows up late
        let popped: Vec<Frame> = std::iter::from_fn(|| jb.pop()).collect();
        let seqs: Vec<u32> = popped.iter().map(|f| f.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
        // seq 1 must be the REAL audio now, not the placeholder.
        assert!(!popped[1].synthetic);
        assert!(popped[1].samples.iter().all(|&s| s == 1));
        let s = jb.stats();
        assert_eq!(s.lost, 1); // the gap was detected while it lasted
        assert_eq!(s.late_dropped, 1); // and the arrival counted as reorder
    }

    #[test]
    fn already_played_frame_is_dropped_as_late() {
        let mut jb = JitterBuffer::new(2, 48_000, 30.0);
        jb.push(frame(0, 0), 0.0);
        jb.pop(); // plays out 0..5ms, watermark moves
        let accepted = jb.push(frame(0, 0), 1.0); // duplicate of played audio
        assert!(!accepted);
        assert_eq!(jb.stats().late_dropped, 1);
        assert!(jb.is_empty());
    }

    #[test]
    fn overrun_evicts_oldest_not_newest() {
        let mut jb = JitterBuffer::new(2, 48_000, 30.0);
        jb.push(frame(0, 0), 0.0);
        // 0.8 s = 160 frames of 5 ms; flood past it.
        for i in 1..250u32 {
            jb.push(frame(i, i as u64 * 5_000), i as f64 * 5.0);
        }
        assert!(jb.buffered_ms() <= 800.0 + 5.0);
        assert!(jb.stats().evicted > 0);
        // The newest frame must still be present (we evicted the OLD ones).
        let mut last = 0;
        while let Some(f) = jb.pop() {
            last = f.sequence;
        }
        assert_eq!(last, 249);
    }

    #[test]
    fn estimator_reacts_to_jitter_and_events() {
        let mut est = JitterEstimator::new(30.0);
        assert_eq!(est.target_ms(), 30.0);
        // Zero-jitter arrivals: target stays at base.
        for i in 0..100u64 {
            est.observe_arrival(i * 5_000, i as f64 * 5.0);
        }
        assert!((est.target_ms() - 30.0).abs() < 1.0);
        // 10 ms of arrival jitter should push the target up.
        for i in 0..100u64 {
            let jittered = if i % 2 == 0 { 0.0 } else { 10.0 };
            est.observe_arrival(i * 5_000, i as f64 * 5.0 + jittered);
        }
        assert!(est.target_ms() > 45.0, "target {}", est.target_ms());
        // A loss event bumps it further.
        let before = est.target_ms();
        est.report_event();
        assert!(est.target_ms() > before, "event should raise the target");
        // Never exceeds the configured band.
        for _ in 0..50 {
            est.report_event();
        }
        assert!(est.target_ms() <= 200.0);
    }

    #[test]
    fn underruns_raise_target_and_calm_decays_it() {
        let mut jb = JitterBuffer::new(1, 48_000, 30.0);
        let base = jb.target_ms();
        // Clean arrivals keep the target at base.
        for i in 0..100u32 {
            jb.push(frame(i, u64::from(i) * 5_000), f64::from(i) * 5.0);
            jb.pop();
        }
        assert!(
            jb.target_ms() < base + 5.0,
            "quiet link crept: {}",
            jb.target_ms()
        );
        // A starvation burst must push the target up sharply...
        for _ in 0..10 {
            jb.report_underrun();
        }
        let raised = jb.target_ms();
        assert!(
            raised > base + 30.0,
            "underruns should raise target: {raised}"
        );
        // ...and a hundred calm arrivals must let it decay again.
        for i in 100..200u32 {
            jb.push(frame(i, u64::from(i) * 5_000), f64::from(i) * 5.0);
            jb.pop();
        }
        assert!(
            jb.target_ms() < raised,
            "target should decay in calm: {}",
            jb.target_ms()
        );
    }

    #[test]
    fn estimator_clamps_to_min() {
        let mut est = JitterEstimator::new(5.0); // below allowed floor
        est.observe_arrival(0, 0.0);
        est.observe_arrival(5_000, 5.0);
        assert!(est.target_ms() >= 10.0);
    }

    #[test]
    fn clear_resets_sequence_baseline() {
        let mut jb = JitterBuffer::new(2, 48_000, 30.0);
        jb.push(frame(0, 0), 0.0);
        jb.push(frame(9, 45_000), 10.0);
        jb.clear();
        assert!(jb.is_empty());
        jb.push(frame(0, 0), 20.0); // new stream starting from seq 0
        assert_eq!(jb.stats().received, 3);
        // No spurious gap counted after clear: queue holds only the one frame.
        assert_eq!(jb.buffered_ms(), 5.0);
    }
}
