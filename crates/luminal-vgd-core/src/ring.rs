// SPDX-License-Identifier: AGPL-3.0-only
//! Drop-oldest ring policy (DESIGN.md §3.1).
//!
//! This models the slot state machine the driver enforces with keyed
//! mutexes on Windows. The invariants the whole transport hangs on:
//!
//! 1. The writer (driver) NEVER waits for the reader (host). If no slot is
//!    free it overwrites the oldest published frame; if the host has
//!    checked out everything (pathological), the frame is dropped and
//!    counted. A hung host must never hang the driver (§3.3 rule 1).
//! 2. Sequences are monotonic across drops AND across generation bumps, so
//!    the host detects gaps by arithmetic, not guesswork.
//! 3. A slot the host holds (`Reading`) is never touched by the writer.

/// Slot states, mirroring `proto::slot_state` semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Free,
    Writing,
    /// Published and unconsumed; payload is the frame's sequence number.
    Published(u64),
    Reading,
}

/// Outcome of `writer_acquire`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterSlot {
    pub index: usize,
    /// Sequence of the published-but-unread frame this write overwrote,
    /// if any (telemetry: `frames_dropped`).
    pub overwrote: Option<u64>,
}

#[derive(Debug)]
pub struct RingPolicy {
    slots: Vec<Slot>,
    next_sequence: u64,
    pub generation: u32,
    pub frames_published: u64,
    pub frames_dropped: u64,
}

impl RingPolicy {
    pub fn new(slot_count: u32) -> Self {
        let slot_count = slot_count.clamp(2, luminal_driver_proto::ABI_MAX_RING_SLOTS) as usize;
        Self {
            slots: vec![Slot::Free; slot_count],
            next_sequence: 1,
            generation: 1,
            frames_published: 0,
            frames_dropped: 0,
        }
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn slot(&self, index: usize) -> Slot {
        self.slots[index]
    }

    /// Driver has a new frame: pick a slot to write into. Prefers a free
    /// slot; otherwise overwrites the OLDEST published frame; if the host
    /// holds every slot, returns `None` (frame dropped entirely).
    pub fn writer_acquire(&mut self) -> Option<WriterSlot> {
        if let Some(i) = self.slots.iter().position(|s| *s == Slot::Free) {
            self.slots[i] = Slot::Writing;
            return Some(WriterSlot { index: i, overwrote: None });
        }
        let oldest = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s {
                Slot::Published(seq) => Some((i, *seq)),
                _ => None,
            })
            .min_by_key(|&(_, seq)| seq);
        match oldest {
            Some((i, seq)) => {
                self.slots[i] = Slot::Writing;
                self.frames_dropped += 1;
                Some(WriterSlot { index: i, overwrote: Some(seq) })
            }
            None => {
                // Reader holds everything it could; writer never blocks.
                self.frames_dropped += 1;
                None
            }
        }
    }

    /// Writer finished copying: publish the slot. Returns the frame's
    /// sequence number (monotonic, gap-free on the writer side).
    pub fn publish(&mut self, index: usize) -> u64 {
        debug_assert_eq!(self.slots[index], Slot::Writing);
        let seq = self.next_sequence;
        self.next_sequence += 1;
        self.frames_published += 1;
        self.slots[index] = Slot::Published(seq);
        seq
    }

    /// Host wants the newest frame: checks out the LATEST published slot
    /// (streams want freshness, not backlog). Older published frames stay
    /// eligible for overwrite.
    pub fn reader_acquire_latest(&mut self) -> Option<(usize, u64)> {
        let newest = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s {
                Slot::Published(seq) => Some((i, *seq)),
                _ => None,
            })
            .max_by_key(|&(_, seq)| seq)?;
        self.slots[newest.0] = Slot::Reading;
        Some(newest)
    }

    /// Host released a checked-out slot.
    pub fn reader_release(&mut self, index: usize) {
        debug_assert_eq!(self.slots[index], Slot::Reading);
        self.slots[index] = Slot::Free;
    }

    /// TDR/device-reset rebuild (§3.3 rule 2): all slots reset, generation
    /// bumps (host re-opens textures by name — the generation is baked into
    /// the name), but sequences CONTINUE so the host's gap detection spans
    /// the rebuild.
    pub fn rebuild(&mut self) -> u32 {
        for s in &mut self.slots {
            *s = Slot::Free;
        }
        self.generation += 1;
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_publish_consume() {
        let mut r = RingPolicy::new(3);
        let w = r.writer_acquire().unwrap();
        assert_eq!(w.overwrote, None);
        let seq = r.publish(w.index);
        assert_eq!(seq, 1);

        let (i, got) = r.reader_acquire_latest().unwrap();
        assert_eq!(got, 1);
        r.reader_release(i);
        assert_eq!(r.slot(i), Slot::Free);
        assert_eq!(r.frames_published, 1);
        assert_eq!(r.frames_dropped, 0);
    }

    #[test]
    fn writer_prefers_free_then_drops_oldest() {
        let mut r = RingPolicy::new(3);
        // Fill all three slots with published frames 1,2,3.
        for _ in 0..3 {
            let w = r.writer_acquire().unwrap();
            assert_eq!(w.overwrote, None);
            r.publish(w.index);
        }
        // Fourth frame: no free slot => overwrite oldest (seq 1).
        let w = r.writer_acquire().unwrap();
        assert_eq!(w.overwrote, Some(1));
        let seq = r.publish(w.index);
        assert_eq!(seq, 4, "sequence keeps counting through drops");
        assert_eq!(r.frames_dropped, 1);

        // Reader sees the newest (4), and the gap is detectable: 2,3,4
        // remain => latest is 4.
        let (_, got) = r.reader_acquire_latest().unwrap();
        assert_eq!(got, 4);
    }

    #[test]
    fn reader_takes_newest_not_backlog() {
        let mut r = RingPolicy::new(3);
        for _ in 0..2 {
            let w = r.writer_acquire().unwrap();
            r.publish(w.index);
        }
        let (_, seq) = r.reader_acquire_latest().unwrap();
        assert_eq!(seq, 2, "stream reads freshest frame");
    }

    #[test]
    fn writer_never_touches_slot_host_holds() {
        let mut r = RingPolicy::new(2);
        let w = r.writer_acquire().unwrap();
        r.publish(w.index);
        let (held, _) = r.reader_acquire_latest().unwrap();

        // Fill the other slot, then keep writing: only the non-held slot
        // may ever be overwritten.
        let w = r.writer_acquire().unwrap();
        assert_ne!(w.index, held);
        r.publish(w.index);
        for _ in 0..5 {
            let w = r.writer_acquire().unwrap();
            assert_ne!(w.index, held, "held slot is untouchable");
            r.publish(w.index);
        }
    }

    #[test]
    fn all_slots_held_drops_frame_without_blocking() {
        let mut r = RingPolicy::new(2);
        // Host checks out both slots (pathological but must not wedge us).
        for _ in 0..2 {
            let w = r.writer_acquire().unwrap();
            r.publish(w.index);
            r.reader_acquire_latest().unwrap();
        }
        assert_eq!(r.writer_acquire(), None, "no slot: drop, don't block");
        assert_eq!(r.frames_dropped, 1);
    }

    #[test]
    fn rebuild_bumps_generation_but_sequences_continue() {
        let mut r = RingPolicy::new(3);
        let w = r.writer_acquire().unwrap();
        let last = r.publish(w.index);
        assert_eq!(r.generation, 1);

        let gen = r.rebuild();
        assert_eq!(gen, 2);
        assert!(r.reader_acquire_latest().is_none(), "slots reset");

        let w = r.writer_acquire().unwrap();
        let seq = r.publish(w.index);
        assert_eq!(seq, last + 1, "sequence spans the rebuild");
    }

    #[test]
    fn slot_count_clamped_to_abi() {
        assert_eq!(RingPolicy::new(0).slot_count(), 2);
        assert_eq!(RingPolicy::new(1).slot_count(), 2);
        assert_eq!(
            RingPolicy::new(99).slot_count(),
            luminal_driver_proto::ABI_MAX_RING_SLOTS as usize
        );
    }
}
