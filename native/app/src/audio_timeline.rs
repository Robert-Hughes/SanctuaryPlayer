use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueResult {
    Queued,
    Dropped,
    Deferred,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FillStats {
    pub(crate) output_frames: u64,
    pub(crate) copied_frames: u64,
    pub(crate) silence_frames: u64,
    pub(crate) discarded_frames: u64,
}

struct SharedTimeline {
    initialised: AtomicBool,
    origin_pts: AtomicI64,
    ring_start_pts: AtomicI64,
    next_output_pts: AtomicI64,
}

impl SharedTimeline {
    fn new() -> Self {
        Self {
            initialised: AtomicBool::new(false),
            origin_pts: AtomicI64::new(0),
            ring_start_pts: AtomicI64::new(0),
            next_output_pts: AtomicI64::new(0),
        }
    }

    fn initialise(&self, pts: i64) {
        self.origin_pts.store(pts, Ordering::Release);
        self.ring_start_pts.store(pts, Ordering::Release);
        self.next_output_pts.store(pts, Ordering::Release);
        self.initialised.store(true, Ordering::Release);
    }

    fn is_initialised(&self) -> bool {
        self.initialised.load(Ordering::Acquire)
    }

    fn origin_pts(&self) -> Option<i64> {
        self.is_initialised()
            .then(|| self.origin_pts.load(Ordering::Acquire))
    }

    fn next_output_pts(&self) -> Option<i64> {
        self.is_initialised()
            .then(|| self.next_output_pts.load(Ordering::Acquire))
    }
}

pub(crate) struct PcmTimelineProducer {
    producer: HeapProd<f32>,
    shared: Arc<SharedTimeline>,
    channels: usize,
    capacity_frames: usize,
    write_end_pts: Option<i64>,
}

pub(crate) struct PcmTimelineConsumer {
    consumer: HeapCons<f32>,
    shared: Arc<SharedTimeline>,
    channels: usize,
}

pub(crate) fn pcm_timeline_ring(
    capacity_frames: usize,
    channels: u16,
) -> (PcmTimelineProducer, PcmTimelineConsumer) {
    let channels = usize::from(channels.max(1));
    let capacity_frames = capacity_frames.max(1);
    let ring = HeapRb::<f32>::new(capacity_frames.saturating_mul(channels));
    let (producer, consumer) = ring.split();
    let shared = Arc::new(SharedTimeline::new());
    (
        PcmTimelineProducer {
            producer,
            shared: Arc::clone(&shared),
            channels,
            capacity_frames,
            write_end_pts: None,
        },
        PcmTimelineConsumer {
            consumer,
            shared,
            channels,
        },
    )
}

impl PcmTimelineProducer {
    pub(crate) fn queue_interleaved(
        &mut self,
        frame_pts: Option<i64>,
        frame_samples: u64,
        interleaved: &[f32],
    ) -> Result<QueueResult, String> {
        let frame_samples_usize = usize::try_from(frame_samples)
            .map_err(|_| "decoded audio frame is too large for this platform".to_owned())?;
        let expected_scalars = frame_samples_usize
            .checked_mul(self.channels)
            .ok_or_else(|| "decoded audio frame size overflow".to_owned())?;
        if interleaved.len() != expected_scalars {
            return Err(format!(
                "decoded audio has {} interleaved samples, expected {expected_scalars}",
                interleaved.len()
            ));
        }

        let occupied_frames = self.queued_frames();
        let expected_end = if !self.shared.is_initialised() {
            let initial = frame_pts.unwrap_or(0);
            self.shared.initialise(initial);
            self.write_end_pts = Some(initial);
            initial
        } else if occupied_frames == 0 {
            let next = self
                .shared
                .next_output_pts()
                .expect("initialised timeline has next output PTS");
            self.write_end_pts = Some(next);
            self.shared.ring_start_pts.store(next, Ordering::Release);
            next
        } else {
            self.write_end_pts
                .expect("non-empty audio ring must have an end PTS")
        };

        let frame_start = frame_pts.unwrap_or(expected_end);
        let frame_end = frame_start
            .checked_add(
                i64::try_from(frame_samples)
                    .map_err(|_| "decoded audio frame duration exceeds PTS range".to_owned())?,
            )
            .ok_or_else(|| "decoded audio frame end PTS overflow".to_owned())?;

        if frame_end <= expected_end {
            return Ok(QueueResult::Dropped);
        }

        let trim_frames = if frame_start < expected_end {
            u64::try_from(expected_end - frame_start)
                .map_err(|_| "decoded audio overlap exceeds supported range".to_owned())?
        } else {
            0
        };
        let remaining_frame_samples = frame_samples.saturating_sub(trim_frames);
        if remaining_frame_samples > self.capacity_frames as u64 {
            return Err(format!(
                "decoded audio frame has {remaining_frame_samples} usable samples but ring capacity is {}",
                self.capacity_frames
            ));
        }

        if frame_start > expected_end {
            let gap_frames = usize::try_from(frame_start - expected_end)
                .map_err(|_| "decoded audio gap exceeds supported range".to_owned())?;
            let padded = self.pad_gap(gap_frames);
            if padded < gap_frames {
                return Ok(QueueResult::Deferred);
            }
        }

        let current_end = self
            .write_end_pts
            .expect("initialised audio timeline must have an end PTS");
        debug_assert_eq!(current_end, frame_start.max(expected_end));

        let remaining_usize = usize::try_from(remaining_frame_samples)
            .map_err(|_| "decoded audio suffix is too large for this platform".to_owned())?;
        if self.vacant_frames() < remaining_usize {
            return Ok(QueueResult::Deferred);
        }

        if remaining_usize == 0 {
            return Ok(QueueResult::Dropped);
        }

        let trim_scalars = usize::try_from(trim_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(self.channels))
            .ok_or_else(|| "decoded audio overlap offset overflow".to_owned())?;
        let suffix = &interleaved[trim_scalars..];
        if self.queued_frames() == 0 {
            self.shared
                .ring_start_pts
                .store(current_end, Ordering::Release);
        }
        let pushed = self.producer.push_slice(suffix);
        if pushed != suffix.len() {
            return Err(format!(
                "audio ring accepted only {pushed} of {} interleaved samples",
                suffix.len()
            ));
        }
        self.write_end_pts = current_end
            .checked_add(
                i64::try_from(remaining_frame_samples)
                    .map_err(|_| "audio ring end PTS exceeds supported range".to_owned())?,
            )
            .ok_or_else(|| "audio ring end PTS overflow".to_owned())
            .map(Some)?;

        Ok(QueueResult::Queued)
    }

    fn pad_gap(&mut self, gap_frames: usize) -> usize {
        let to_pad = gap_frames.min(self.vacant_frames());
        if to_pad == 0 {
            return 0;
        }
        let current_end = self
            .write_end_pts
            .expect("initialised audio timeline must have an end PTS");
        if self.queued_frames() == 0 {
            self.shared
                .ring_start_pts
                .store(current_end, Ordering::Release);
        }
        let scalar_count = to_pad.saturating_mul(self.channels);
        let pushed = self
            .producer
            .push_iter(std::iter::repeat_n(0.0, scalar_count));
        debug_assert_eq!(pushed, scalar_count);
        let padded_frames = pushed / self.channels;
        self.write_end_pts = Some(current_end.saturating_add(padded_frames as i64));
        padded_frames
    }

    pub(crate) fn queued_frames(&self) -> usize {
        self.producer.occupied_len() / self.channels
    }

    pub(crate) fn vacant_frames(&self) -> usize {
        self.producer.vacant_len() / self.channels
    }

    #[cfg(test)]
    pub(crate) fn ring_end_pts(&self) -> Option<i64> {
        self.write_end_pts
    }

    pub(crate) fn origin_pts(&self) -> Option<i64> {
        self.shared.origin_pts()
    }

    pub(crate) fn next_output_pts(&self) -> Option<i64> {
        self.shared.next_output_pts()
    }
}

impl PcmTimelineConsumer {
    pub(crate) fn fill(&mut self, out: &mut [f32]) -> FillStats {
        assert!(
            out.len().is_multiple_of(self.channels),
            "sysaudio output buffer must contain whole sample frames"
        );
        let requested_frames = out.len() / self.channels;
        if requested_frames == 0 {
            return FillStats::default();
        }

        if !self.shared.is_initialised() {
            out.fill(0.0);
            return FillStats {
                output_frames: requested_frames as u64,
                silence_frames: requested_frames as u64,
                ..FillStats::default()
            };
        }

        let mut stats = FillStats {
            output_frames: requested_frames as u64,
            ..FillStats::default()
        };
        let mut cursor = self
            .shared
            .next_output_pts()
            .expect("initialised timeline has next output PTS");
        let mut dst_frames = 0usize;

        while dst_frames < requested_frames {
            let occupied_frames = self.consumer.occupied_len() / self.channels;
            if occupied_frames == 0 {
                let silence = requested_frames - dst_frames;
                out[dst_frames * self.channels..].fill(0.0);
                stats.silence_frames = stats.silence_frames.saturating_add(silence as u64);
                cursor = cursor.saturating_add(silence as i64);
                break;
            }

            let ring_start = self.shared.ring_start_pts.load(Ordering::Acquire);
            if ring_start < cursor {
                let stale_frames = usize::try_from(cursor - ring_start)
                    .unwrap_or(usize::MAX)
                    .min(occupied_frames);
                let skipped_scalars = self
                    .consumer
                    .skip(stale_frames.saturating_mul(self.channels));
                let skipped_frames = skipped_scalars / self.channels;
                if skipped_frames == 0 {
                    break;
                }
                self.shared.ring_start_pts.store(
                    ring_start.saturating_add(skipped_frames as i64),
                    Ordering::Release,
                );
                stats.discarded_frames =
                    stats.discarded_frames.saturating_add(skipped_frames as u64);
                continue;
            }

            if ring_start > cursor {
                let silence = usize::try_from(ring_start - cursor)
                    .unwrap_or(usize::MAX)
                    .min(requested_frames - dst_frames);
                let start = dst_frames * self.channels;
                let end = (dst_frames + silence) * self.channels;
                out[start..end].fill(0.0);
                dst_frames += silence;
                cursor = cursor.saturating_add(silence as i64);
                stats.silence_frames = stats.silence_frames.saturating_add(silence as u64);
                continue;
            }

            let copy_frames = occupied_frames.min(requested_frames - dst_frames);
            let start = dst_frames * self.channels;
            let end = (dst_frames + copy_frames) * self.channels;
            let popped_scalars = self.consumer.pop_slice(&mut out[start..end]);
            let popped_frames = popped_scalars / self.channels;
            if popped_frames == 0 {
                continue;
            }
            dst_frames += popped_frames;
            cursor = cursor.saturating_add(popped_frames as i64);
            self.shared.ring_start_pts.store(cursor, Ordering::Release);
            stats.copied_frames = stats.copied_frames.saturating_add(popped_frames as u64);
        }

        self.shared.next_output_pts.store(cursor, Ordering::Release);
        stats
    }

    #[cfg(test)]
    fn queued_frames(&self) -> usize {
        self.consumer.occupied_len() / self.channels
    }

    #[cfg(test)]
    fn ring_start_pts(&self) -> Option<i64> {
        self.shared
            .is_initialised()
            .then(|| self.shared.ring_start_pts.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono_ring(capacity: usize) -> (PcmTimelineProducer, PcmTimelineConsumer) {
        pcm_timeline_ring(capacity, 1)
    }

    #[test]
    fn first_frame_initialises_integer_timeline_and_appends() {
        let (mut producer, consumer) = mono_ring(8);
        assert_eq!(
            producer.queue_interleaved(Some(100), 3, &[1.0, 2.0, 3.0]),
            Ok(QueueResult::Queued)
        );
        assert_eq!(producer.origin_pts(), Some(100));
        assert_eq!(producer.next_output_pts(), Some(100));
        assert_eq!(producer.ring_end_pts(), Some(103));
        assert_eq!(consumer.ring_start_pts(), Some(100));
        assert_eq!(producer.queued_frames(), 3);
    }

    #[test]
    fn contiguous_frame_appends_at_ring_end() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer
            .queue_interleaved(Some(10), 2, &[1.0, 2.0])
            .unwrap();
        assert_eq!(
            producer.queue_interleaved(Some(12), 2, &[3.0, 4.0]),
            Ok(QueueResult::Queued)
        );
        let mut out = [0.0; 4];
        let stats = consumer.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(stats.copied_frames, 4);
        assert_eq!(producer.next_output_pts(), Some(14));
    }

    #[test]
    fn missing_pts_is_assumed_contiguous() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer
            .queue_interleaved(Some(50), 2, &[1.0, 2.0])
            .unwrap();
        assert_eq!(
            producer.queue_interleaved(None, 2, &[3.0, 4.0]),
            Ok(QueueResult::Queued)
        );
        assert_eq!(producer.ring_end_pts(), Some(54));
        let mut out = [0.0; 4];
        consumer.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn small_forward_gap_is_padded_with_zeroes_before_frame() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer
            .queue_interleaved(Some(10), 2, &[1.0, 2.0])
            .unwrap();
        assert_eq!(
            producer.queue_interleaved(Some(14), 2, &[5.0, 6.0]),
            Ok(QueueResult::Queued)
        );
        assert_eq!(producer.ring_end_pts(), Some(16));
        let mut out = [9.0; 6];
        consumer.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 5.0, 6.0]);
    }

    #[test]
    fn too_large_gap_fills_available_ring_with_zeroes_and_defers_frame() {
        let (mut producer, mut consumer) = mono_ring(4);
        producer.queue_interleaved(Some(0), 1, &[1.0]).unwrap();

        assert_eq!(
            producer.queue_interleaved(Some(10), 1, &[9.0]),
            Ok(QueueResult::Deferred)
        );
        assert_eq!(producer.queued_frames(), 4);
        assert_eq!(producer.ring_end_pts(), Some(4));

        let mut first = [7.0; 4];
        consumer.fill(&mut first);
        assert_eq!(first, [1.0, 0.0, 0.0, 0.0]);

        assert_eq!(
            producer.queue_interleaved(Some(10), 1, &[9.0]),
            Ok(QueueResult::Deferred)
        );
        assert_eq!(producer.ring_end_pts(), Some(8));
        let mut second = [7.0; 4];
        consumer.fill(&mut second);
        assert_eq!(second, [0.0; 4]);

        assert_eq!(
            producer.queue_interleaved(Some(10), 1, &[9.0]),
            Ok(QueueResult::Queued)
        );
        assert_eq!(producer.ring_end_pts(), Some(11));
        let mut third = [7.0; 3];
        consumer.fill(&mut third);
        assert_eq!(third, [0.0, 0.0, 9.0]);
    }

    #[test]
    fn gap_can_fit_while_following_frame_is_deferred_without_partial_audio() {
        let (mut producer, mut consumer) = mono_ring(4);
        producer.queue_interleaved(Some(0), 1, &[1.0]).unwrap();

        assert_eq!(
            producer.queue_interleaved(Some(3), 2, &[3.0, 4.0]),
            Ok(QueueResult::Deferred)
        );
        assert_eq!(producer.ring_end_pts(), Some(3));
        assert_eq!(producer.queued_frames(), 3);

        let mut first = [9.0; 4];
        consumer.fill(&mut first);
        assert_eq!(first, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(producer.next_output_pts(), Some(4));

        assert_eq!(
            producer.queue_interleaved(Some(3), 2, &[3.0, 4.0]),
            Ok(QueueResult::Queued)
        );
        let mut second = [0.0; 1];
        consumer.fill(&mut second);
        assert_eq!(second, [4.0]);
    }

    #[test]
    fn future_frame_after_underflow_pads_from_next_output_cursor() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer.queue_interleaved(Some(10), 1, &[1.0]).unwrap();

        let mut first = [0.0; 3];
        consumer.fill(&mut first);
        assert_eq!(first, [1.0, 0.0, 0.0]);
        assert_eq!(producer.next_output_pts(), Some(13));

        assert_eq!(
            producer.queue_interleaved(Some(15), 1, &[6.0]),
            Ok(QueueResult::Queued)
        );
        assert_eq!(producer.ring_end_pts(), Some(16));

        let mut second = [9.0; 3];
        consumer.fill(&mut second);
        assert_eq!(second, [0.0, 0.0, 6.0]);
        assert_eq!(producer.next_output_pts(), Some(16));
    }

    #[test]
    fn wholly_overlapped_frame_is_dropped() {
        let (mut producer, _consumer) = mono_ring(8);
        producer
            .queue_interleaved(Some(10), 4, &[1.0, 2.0, 3.0, 4.0])
            .unwrap();
        assert_eq!(
            producer.queue_interleaved(Some(11), 2, &[8.0, 9.0]),
            Ok(QueueResult::Dropped)
        );
        assert_eq!(producer.ring_end_pts(), Some(14));
        assert_eq!(producer.queued_frames(), 4);
    }

    #[test]
    fn partial_overlap_trims_only_duplicate_prefix() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer
            .queue_interleaved(Some(10), 4, &[1.0, 2.0, 3.0, 4.0])
            .unwrap();
        assert_eq!(
            producer.queue_interleaved(Some(12), 4, &[30.0, 40.0, 5.0, 6.0]),
            Ok(QueueResult::Queued)
        );
        assert_eq!(producer.ring_end_pts(), Some(16));
        let mut out = [0.0; 6];
        consumer.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn contiguous_overflow_defers_whole_frame_without_partial_append() {
        let (mut producer, mut consumer) = mono_ring(4);
        producer
            .queue_interleaved(Some(0), 3, &[1.0, 2.0, 3.0])
            .unwrap();
        assert_eq!(
            producer.queue_interleaved(Some(3), 2, &[4.0, 5.0]),
            Ok(QueueResult::Deferred)
        );
        assert_eq!(producer.ring_end_pts(), Some(3));

        let mut first = [0.0; 2];
        consumer.fill(&mut first);
        assert_eq!(first, [1.0, 2.0]);

        assert_eq!(
            producer.queue_interleaved(Some(3), 2, &[4.0, 5.0]),
            Ok(QueueResult::Queued)
        );
        let mut rest = [0.0; 3];
        consumer.fill(&mut rest);
        assert_eq!(rest, [3.0, 4.0, 5.0]);
    }

    #[test]
    fn callback_discards_ring_data_that_is_behind_next_output_pts() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer
            .queue_interleaved(Some(0), 6, &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0])
            .unwrap();

        consumer.shared.next_output_pts.store(3, Ordering::Release);
        let mut out = [99.0; 2];
        let stats = consumer.fill(&mut out);
        assert_eq!(stats.discarded_frames, 3);
        assert_eq!(out, [3.0, 4.0]);
        assert_eq!(producer.next_output_pts(), Some(5));
    }

    #[test]
    fn callback_inserts_silence_when_ring_starts_ahead_then_copies_audio() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer.shared.initialise(0);
        producer.write_end_pts = Some(3);
        producer.shared.ring_start_pts.store(3, Ordering::Release);
        producer.producer.push_slice(&[7.0, 8.0]);

        let mut out = [1.0; 5];
        let stats = consumer.fill(&mut out);
        assert_eq!(out, [0.0, 0.0, 0.0, 7.0, 8.0]);
        assert_eq!(stats.silence_frames, 3);
        assert_eq!(stats.copied_frames, 2);
        assert_eq!(producer.next_output_pts(), Some(5));
    }

    #[test]
    fn empty_ring_outputs_silence_and_advances_next_output_pts() {
        let (mut producer, mut consumer) = mono_ring(4);
        producer.queue_interleaved(Some(100), 1, &[1.0]).unwrap();
        let mut first = [0.0; 1];
        consumer.fill(&mut first);
        assert_eq!(producer.next_output_pts(), Some(101));

        let mut out = [9.0; 3];
        let stats = consumer.fill(&mut out);
        assert_eq!(out, [0.0; 3]);
        assert_eq!(stats.silence_frames, 3);
        assert_eq!(producer.next_output_pts(), Some(104));
    }

    #[test]
    fn partial_underflow_copies_available_audio_then_zero_fills_remainder() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer
            .queue_interleaved(Some(20), 2, &[1.0, 2.0])
            .unwrap();
        let mut out = [9.0; 5];
        let stats = consumer.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 0.0]);
        assert_eq!(stats.copied_frames, 2);
        assert_eq!(stats.silence_frames, 3);
        assert_eq!(producer.next_output_pts(), Some(25));
    }

    #[test]
    fn late_frame_after_underflow_trims_missed_prefix_and_keeps_future_suffix() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer.queue_interleaved(Some(0), 1, &[1.0]).unwrap();

        let mut first = [9.0; 4];
        consumer.fill(&mut first);
        assert_eq!(first, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(producer.next_output_pts(), Some(4));

        assert_eq!(
            producer.queue_interleaved(Some(2), 4, &[2.0, 3.0, 4.0, 5.0]),
            Ok(QueueResult::Queued)
        );
        let mut second = [0.0; 2];
        consumer.fill(&mut second);
        assert_eq!(second, [4.0, 5.0]);
        assert_eq!(producer.next_output_pts(), Some(6));
    }

    #[test]
    fn wholly_late_frame_after_underflow_is_dropped() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer.queue_interleaved(Some(0), 1, &[1.0]).unwrap();
        let mut out = [0.0; 4];
        consumer.fill(&mut out);
        assert_eq!(producer.next_output_pts(), Some(4));

        assert_eq!(
            producer.queue_interleaved(Some(1), 2, &[8.0, 9.0]),
            Ok(QueueResult::Dropped)
        );
        assert_eq!(producer.queued_frames(), 0);
    }

    #[test]
    fn missing_pts_after_underflow_continues_from_next_output_pts() {
        let (mut producer, mut consumer) = mono_ring(8);
        producer.queue_interleaved(Some(10), 1, &[1.0]).unwrap();
        let mut first = [0.0; 3];
        consumer.fill(&mut first);
        assert_eq!(producer.next_output_pts(), Some(13));

        assert_eq!(
            producer.queue_interleaved(None, 2, &[4.0, 5.0]),
            Ok(QueueResult::Queued)
        );
        assert_eq!(producer.ring_end_pts(), Some(15));
        let mut second = [0.0; 2];
        consumer.fill(&mut second);
        assert_eq!(second, [4.0, 5.0]);
    }

    #[test]
    fn stereo_timeline_counts_sample_frames_not_scalar_samples() {
        let (mut producer, mut consumer) = pcm_timeline_ring(4, 2);
        producer
            .queue_interleaved(Some(10), 2, &[1.0, -1.0, 2.0, -2.0])
            .unwrap();
        assert_eq!(producer.queued_frames(), 2);
        assert_eq!(producer.ring_end_pts(), Some(12));
        let mut out = [0.0; 4];
        consumer.fill(&mut out);
        assert_eq!(out, [1.0, -1.0, 2.0, -2.0]);
        assert_eq!(producer.next_output_pts(), Some(12));
    }

    #[test]
    fn frame_larger_than_ring_capacity_is_rejected() {
        let (mut producer, _consumer) = mono_ring(2);
        let error = producer
            .queue_interleaved(Some(0), 3, &[1.0, 2.0, 3.0])
            .unwrap_err();
        assert!(error.contains("ring capacity"));
    }

    #[test]
    fn callback_before_timeline_initialisation_is_silent_without_advancing_pts() {
        let (producer, mut consumer) = mono_ring(4);
        let mut out = [1.0; 2];
        let stats = consumer.fill(&mut out);
        assert_eq!(out, [0.0; 2]);
        assert_eq!(stats.silence_frames, 2);
        assert_eq!(producer.next_output_pts(), None);
        assert_eq!(consumer.queued_frames(), 0);
    }
}
