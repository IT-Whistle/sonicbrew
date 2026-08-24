//! RTP receive-side jitter buffer.
//!
//! Reorders packets by sequence number (`u16`, wrapping at 65536), suppresses
//! duplicates, handles loss (explicit gap-skip), and bounds memory with a
//! capacity. Wrap-aware via signed (`i16`) sequence arithmetic, so it stays
//! correct across the 65535→0 boundary.
//!
//! The buffer is decoupled from `RtpPacket` (it takes a raw `u16` seq) because
//! `UdpTransport::recv_rtp` returns payload bytes only; the caller captures the
//! seq upstream (a future `recv_rtp_with_seq` path feeds this buffer).
//!
//! # Out of scope
//! Time-based playout delay (this buffer is event-driven: `pop`/`skip_gap`),
//! SSRC validation, and recv-loop integration.

use std::collections::HashMap;

/// Signed forward distance `from → to` over the `u16` RTP sequence space
/// (wraps at 65536). Positive ⇒ `to` is ahead of `from` within a half-window of
/// 32768; negative ⇒ behind; zero ⇒ equal.
#[must_use]
pub fn forward_distance(from: u16, to: u16) -> i16 {
    to.wrapping_sub(from) as i16
}

/// Outcome of [`JitterBuffer::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome<T> {
    /// The packet was buffered (or will be emitted by the next `pop` run).
    Accepted,
    /// The seq was already buffered or has already been emitted (a duplicate / late).
    Duplicate,
    /// The buffer is at capacity; the incoming packet is dropped (newest-loses).
    /// Carries the payload back so the caller can trace/counter an xrun.
    Rejected(T),
}

/// A reorder buffer that emits packets in ascending (wrap-aware) RTP sequence
/// order. The first pushed packet establishes the emission baseline; later
/// packets are buffered if ahead, dropped as duplicates if already seen/emitted,
/// and dropped (returned via [`PushOutcome::Rejected`]) when the buffer is full.
pub struct JitterBuffer<T> {
    /// Next seq to emit. `None` until the first push.
    next_seq: Option<u16>,
    /// Buffered packets keyed by seq (for O(1) dedup/lookup).
    buf: HashMap<u16, T>,
    /// Max simultaneously-buffered (out-of-order) packets.
    capacity: usize,
}

impl<T> JitterBuffer<T> {
    /// Create a buffer holding up to `capacity` out-of-order packets.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            next_seq: None,
            buf: HashMap::with_capacity(capacity),
            capacity,
        }
    }

    /// The next seq that will be emitted, or `None` if nothing has been pushed.
    #[must_use]
    pub fn next_seq(&self) -> Option<u16> {
        self.next_seq
    }

    /// Number of packets currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether no packets are currently buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Push a packet.
    ///
    /// - The first push establishes `next_seq` (the emission baseline).
    /// - A seq already buffered, or already emitted (behind `next_seq`), ⇒ `Duplicate`.
    /// - At capacity ⇒ `Rejected(payload)` (newest loses; payload returned).
    /// - Otherwise ⇒ `Accepted` (buffered).
    pub fn push(&mut self, seq: u16, payload: T) -> PushOutcome<T> {
        match self.next_seq {
            None => {
                // First packet establishes the baseline.
                self.next_seq = Some(seq);
            }
            Some(base) => {
                // Behind the baseline ⇒ already emitted (late). Equal & present ⇒ dup.
                if forward_distance(base, seq) < 0 {
                    return PushOutcome::Duplicate;
                }
            }
        }
        if self.buf.contains_key(&seq) {
            return PushOutcome::Duplicate;
        }
        if self.buf.len() >= self.capacity {
            return PushOutcome::Rejected(payload);
        }
        self.buf.insert(seq, payload);
        PushOutcome::Accepted
    }

    /// Pop the next in-order packet (`seq == next_seq`), advancing `next_seq`.
    /// Returns `None` if the buffer is empty or the next seq isn't available yet
    /// (a gap — use [`Self::skip_gap`] to advance past it).
    pub fn pop(&mut self) -> Option<(u16, T)> {
        let next = self.next_seq?;
        let val = self.buf.remove(&next)?;
        self.next_seq = Some(next.wrapping_add(1));
        Some((next, val))
    }

    /// Declare any gap before the lowest buffered future seq as LOST, advancing
    /// `next_seq` to that lowest buffered seq. Returns the count of seqs skipped.
    /// No-op if `next_seq` is available (no gap) or the buffer is empty.
    pub fn skip_gap(&mut self) -> usize {
        let Some(base) = self.next_seq else {
            return 0;
        };
        if self.buf.contains_key(&base) {
            return 0; // next seq is present — no gap to skip
        }
        // Find the buffered seq with the smallest positive forward distance from base.
        let mut best: Option<(u16, i16)> = None;
        for &seq in self.buf.keys() {
            let d = forward_distance(base, seq);
            if d > 0 && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((seq, d));
            }
        }
        match best {
            Some((seq, d)) => {
                self.next_seq = Some(seq);
                // d packets were skipped (the gap), as an i16 count.
                usize::try_from(d).unwrap_or(0)
            }
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_distance_wrap_arithmetic() {
        assert_eq!(forward_distance(0, 1), 1);
        assert_eq!(forward_distance(0, 10), 10);
        assert_eq!(forward_distance(10, 0), -10);
        assert_eq!(forward_distance(0, 0), 0);
        // Wrap: 65535 → 0 is +1 (ahead).
        assert_eq!(forward_distance(65535, 0), 1);
        assert_eq!(forward_distance(65535, 1), 2);
        // 0 → 65535 is -1 (behind).
        assert_eq!(forward_distance(0, 65535), -1);
    }

    #[test]
    fn in_order_emits_ascending() {
        let mut jb = JitterBuffer::<&str>::new(16);
        assert_eq!(jb.push(0, "a"), PushOutcome::Accepted);
        assert_eq!(jb.push(1, "b"), PushOutcome::Accepted);
        assert_eq!(jb.push(2, "c"), PushOutcome::Accepted);
        assert_eq!(jb.pop(), Some((0, "a")));
        assert_eq!(jb.pop(), Some((1, "b")));
        assert_eq!(jb.pop(), Some((2, "c")));
        assert_eq!(jb.pop(), None);
    }

    #[test]
    fn out_of_order_is_reordered() {
        // Baseline 0 (first push); 2 then 1 arrive ahead, out of order.
        let mut jb = JitterBuffer::<&str>::new(16);
        jb.push(0, "a");
        jb.push(2, "c");
        jb.push(1, "b");
        assert_eq!(jb.pop(), Some((0, "a")));
        assert_eq!(jb.pop(), Some((1, "b")));
        assert_eq!(jb.pop(), Some((2, "c")));
        assert_eq!(jb.pop(), None);
    }

    #[test]
    fn duplicates_are_dropped() {
        let mut jb = JitterBuffer::<&str>::new(16);
        jb.push(0, "a");
        assert_eq!(jb.push(0, "dup"), PushOutcome::Duplicate); // already buffered
        assert_eq!(jb.pop(), Some((0, "a")));
        assert_eq!(jb.push(0, "late"), PushOutcome::Duplicate); // already emitted (behind next_seq=1)
        jb.push(1, "b");
        assert_eq!(jb.pop(), Some((1, "b")));
    }

    #[test]
    fn loss_gap_skipped() {
        let mut jb = JitterBuffer::<&str>::new(16);
        jb.push(0, "a");
        jb.push(2, "c"); // seq 1 missing
        assert_eq!(jb.pop(), Some((0, "a")));
        assert_eq!(jb.pop(), None); // waiting for seq 1 (gap)
        let skipped = jb.skip_gap();
        assert_eq!(skipped, 1); // one seq (1) declared lost
        assert_eq!(jb.pop(), Some((2, "c")));
        assert_eq!(jb.pop(), None);
    }

    #[test]
    fn wraparound_ordering() {
        // Baseline 65535 (first push); 0 then 1 are AHEAD across the wrap.
        let mut jb = JitterBuffer::<&str>::new(16);
        jb.push(65535, "x");
        jb.push(0, "y");
        jb.push(1, "z");
        assert_eq!(jb.pop(), Some((65535, "x")));
        assert_eq!(jb.pop(), Some((0, "y")));
        assert_eq!(jb.pop(), Some((1, "z")));
        assert_eq!(jb.pop(), None);
    }

    #[test]
    fn capacity_rejects_newest() {
        let mut jb = JitterBuffer::<&str>::new(2);
        jb.push(0, "a");
        jb.push(1, "b");
        assert_eq!(jb.push(2, "c"), PushOutcome::Rejected("c")); // full → newest loses
                                                                 // The two buffered still emit in order.
        assert_eq!(jb.pop(), Some((0, "a")));
        assert_eq!(jb.pop(), Some((1, "b")));
        assert_eq!(jb.pop(), None);
    }
}
