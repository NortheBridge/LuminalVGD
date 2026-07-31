// SPDX-License-Identifier: AGPL-3.0-only
//! The decisions behind an `UPDATE_MODES` push, below the shell line.
//!
//! `shell::monitors::push_targets` is the only caller of
//! `IddCxMonitorUpdateModes2`, and it cannot be unit-tested: it needs the
//! eWDK, a live IddCx adapter and an arrived monitor. So everything it
//! *decides* lives here instead — which stage a request stops at, whether
//! an outcome commits the durable list, and whether a proposed target list
//! would evict the mode the OS has committed — exactly as
//! [`crate::tdr`] does for the TDR poller's recovery discriminator.
//!
//! Three questions are answered here, and each of them was a defect first:
//!
//! 1. **Does this outcome commit?** [`PushOutcome::settle_result`] is the
//!    single mapping from "what the push did" to
//!    [`ModeUpdateResult`]. Before it existed, the TDR-parked arm mutated
//!    the parked target list and then settled `NotApplied`, so the durable
//!    list and the list the re-arrival would publish disagreed forever —
//!    see [`parked_push`].
//! 2. **What has the OS committed?** [`CommittedPaths`] is a lock-free
//!    record of the paths `EvtIddCxAdapterCommitModes2` last handed us.
//!    Before it existed the driver threw `IDDCX_PATH2` away and could
//!    neither trace nor avoid publishing a subset that excluded the mode
//!    the only active display was running.
//! 3. **Which monitor mode is preferred?** [`preferred_monitor_mode_idx`] —
//!    the answer stopped being a constant 0 the moment the published
//!    target subset could exclude `monitor_modes[0]`.
//! 4. **Is this refusal worth retrying?** [`PushOutcome::retryable`]. Every
//!    other way a push can fail to apply is transient; a refusal to evict
//!    the committed mode is not, and reporting the two the same way put a
//!    retrying host in a loop it could never leave. The permanent one
//!    carries the blocking mode's index in the monitor's create-time list
//!    all the way out to `UpdateModesReply` (`update_status::BLOCKED` +
//!    `err::MODE_COMMITTED`), so the host can pick a different subset
//!    rather than guess.

use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use luminal_vgd_core::modes::Mode;
use luminal_vgd_core::session::ModeUpdateResult;

/// The paths the OS last committed, per IddCx monitor object.
///
/// Lives here rather than in `shell::monitors` — where it was born —
/// because two layers now read it: the shell writes it from
/// `EvtIddCxAdapterCommitModes2` and consults it before a push, and
/// `dispatch` reads [`committed_token`] on every `UPDATE_MODES` to decide
/// whether a remembered refusal is still evidence of anything. A process
/// global like the rest of the shell's state (one root-enumerated devnode),
/// const-constructible so the modeset callback that writes it never depends
/// on any initialisation having run. On a non-Windows build nothing writes
/// it, so every reader sees "nothing committed" — the fail-open answer.
pub static COMMITTED: CommittedPaths = CommittedPaths::new();

/// The current "what has the OS committed" version stamp.
///
/// Handed to `SessionTable::update_modes` as the validity token for a
/// remembered [`stage::GATES_COMMITTED`] refusal: the refusal is an answer
/// about the mode the OS is running, so it stays good exactly as long as no
/// commit has happened since. Every modeset bumps it.
pub fn committed_token() -> u64 {
    COMMITTED.generation()
}

// ---------------------------------------------------------------------
// Stages
// ---------------------------------------------------------------------

/// Stages for the `UpdateModes*` ETW events (constraint 6: every deny/fail
/// path carries stage + code). Numbering is append-only, like `STAGE_*` in
/// `shell::control`; 6 and 7 belong to the dispatch layer
/// (`trace_update_modes_result`) and are deliberately skipped here.
pub mod stage {
    /// The session has neither a live nor a parked monitor.
    pub const NO_MONITOR: u32 = 0;
    /// Empty list — `TargetModeCount` cannot be zero (IddCx.h:3594).
    pub const EMPTY: u32 = 1;
    /// Applied to a TDR-parked spec; no OS call (there is no monitor
    /// object to push at until the re-arrival creates one).
    pub const PARKED: u32 = 2;
    /// Stored, OS push skipped: a TDR duck is in flight.
    pub const DUCK_PENDING: u32 = 3;
    /// `IddCxMonitorUpdateModes2` returned failure.
    pub const OS_CALL: u32 = 4;
    /// The monitor changed under a failed push, so the rollback was
    /// declined rather than written onto a stranger.
    pub const ROLLBACK_RACED: u32 = 5;
    /// The adapter was already torn down when the push was attempted.
    pub const NO_ADAPTER: u32 = 8;
    /// A requested target has no counterpart in the LIVE monitor's frozen
    /// description.
    pub const NOT_IN_SUPERSET: u32 = 9;
    /// The push would have gated out the mode the OS has COMMITTED on this
    /// monitor. Refused: see [`live_gate`].
    pub const GATES_COMMITTED: u32 = 10;
    /// A D3Final teardown landed between publishing the new runtime list
    /// and the OS call, so the call was never made and the runtime list was
    /// put back.
    pub const ADAPTER_TORN_DOWN: u32 = 11;
}

// ---------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------

/// What a push did with a pending target list. Every variant is reported
/// to `SessionTable::settle_modes` exactly once, through
/// [`settle_result`](Self::settle_result).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushOutcome {
    /// `IddCxMonitorUpdateModes2` returned success: the list is in force.
    Applied { published: u32, superset: u32 },
    /// The session was parked by a TDR duck-out, so there was no monitor
    /// object to push at and the PARKED SPEC was patched instead. The
    /// re-arrival publishes it — through `IddCxMonitorCreate` plus the
    /// OS's own `QueryTargetModes2` solicitation, which is that monitor's
    /// only publication mechanism — so this commits, exactly like
    /// [`Applied`](Self::Applied).
    ///
    /// It is a separate variant rather than `Applied` because no OS call
    /// happened: `UpdateModesApplied(modes, superset)` is the
    /// replace-vs-append measurement (CLAUDE.md build 17), and folding a
    /// call-less application into it would corrupt that reading.
    AppliedParked { published: u32, superset: u32 },
    /// No OS call was made and NOTHING was left changed. Retryable — the
    /// host's next identical request resolves and pushes for real.
    Deferred { stage: u32, count: u32 },
    /// The push was refused before it was made because the list would have
    /// gated out the mode the OS has COMMITTED (see [`live_gate`]).
    ///
    /// A separate variant from [`Failed`](Self::Failed) because it is the
    /// one refusal a RETRY CANNOT FIX. Everything else that lands in
    /// `Failed` — the OS refusing the DDI, a rollback race, an empty list —
    /// is either transient or a caller bug the caller can see; this one is
    /// a statement about what the display is currently running, and it
    /// stands until that changes. Folding it into `Failed` is what made it
    /// indistinguishable from a transient failure on the wire: both settled
    /// `NotApplied`, both left the sticky `UPDATE_FAILED` that means "send
    /// it again", and the host obligingly did, forever.
    ///
    /// It carries what the host needs to act instead of retry:
    /// `superset_idx` names the committed mode inside the monitor's
    /// create-time list, and `token` says how long that answer is good for.
    Blocked { committed: CommittedMode, superset_idx: u32, token: u64, count: u32 },
    /// The OS refused, or there was nothing to push at, or the request
    /// named modes this monitor was never created with.
    Failed { stage: u32, code: i32, count: u32, rolled_back: bool },
}

impl PushOutcome {
    /// THE mapping from a push outcome to the durable commit. There is
    /// exactly one, and it is the reason a caller cannot forget half of a
    /// state change: any outcome that CHANGED a published list (the OS's
    /// or a parked spec's) commits, and any outcome that changed nothing
    /// does not.
    pub fn settle_result(self) -> ModeUpdateResult {
        match self {
            _ if self.commits() => ModeUpdateResult::Applied,
            // The one not-applied outcome that must not be reported as an
            // ordinary not-applied: it is permanent, and the table carries
            // that fact (plus the blocking mode) into the answer the host's
            // next request gets.
            PushOutcome::Blocked { superset_idx, token, .. } => {
                ModeUpdateResult::Blocked { superset_idx, token }
            }
            _ => ModeUpdateResult::NotApplied,
        }
    }

    /// Is the host's identical retry worth anything? False only for
    /// [`Blocked`](Self::Blocked) — see that variant. This is the single
    /// definition behind both the `retryable` field on the ETW events and
    /// the wire's `update_status::BLOCKED`, so a trace and a host can never
    /// disagree about whether a refusal was worth repeating.
    pub fn retryable(self) -> bool {
        !matches!(self, PushOutcome::Blocked { .. })
    }

    /// True when this outcome means "the published list really changed",
    /// which is both the durable-commit condition and — for
    /// [`AppliedParked`](Self::AppliedParked) — the caller's instruction to
    /// write the parked spec.
    pub fn commits(self) -> bool {
        matches!(self, PushOutcome::Applied { .. } | PushOutcome::AppliedParked { .. })
    }
}

/// Decide what a push that lands on a TDR-PARKED session does.
///
/// A parked monitor was departed by the duck-out and its re-arrival
/// creates a NEW monitor object from the SAME EDID, so there is nothing to
/// call `IddCxMonitorUpdateModes2` on and the only way to make the update
/// survive the recovery is to patch the parked spec the replug plugs with.
///
/// # Why this returns an APPLICATION and not a deferral
///
/// Build 17 patched the parked spec and then settled `NotApplied`. Both
/// halves are defensible alone and together they are a permanent split
/// brain:
///
/// * the parked spec — and therefore the monitor the replug re-arrives —
///   published the NEW subset, while
/// * the durable `Monitor.target_modes` still held the OLD one.
///
/// `SessionTable::update_modes` short-circuits a request that matches the
/// durable list ("already published, nothing to do", no effect, `OK`), so
/// the host could never get back to the wider list: it would ask for
/// exactly the list the table thought was in force, be told yes, and go on
/// streaming to a monitor that offered the gated subset. The rescind
/// direction was unreachable for the life of the session.
///
/// So a patch is an application: it changes what the monitor publishes, so
/// it commits. A push that changes nothing (a duck in flight, a torn-down
/// adapter) stays a deferral — that distinction is the whole discipline.
pub fn parked_push(parked_superset: &[Mode], targets: &[Mode]) -> PushOutcome {
    let count = targets.len() as u32;
    if count == 0 {
        return PushOutcome::Failed {
            stage: stage::EMPTY,
            code: luminal_driver_proto::err::BAD_MODE,
            count: 0,
            rolled_back: false,
        };
    }
    // The parked superset is the same one the request was validated
    // against (the replug recreates from the same EDID), so this holds
    // already; re-checked anyway, because it is one scan of at most four
    // entries and the alternative is an unactivatable parked spec.
    if !targets.iter().all(|m| parked_superset.contains(m)) {
        return PushOutcome::Failed {
            stage: stage::NOT_IN_SUPERSET,
            code: luminal_driver_proto::err::BAD_MODE,
            count,
            rolled_back: false,
        };
    }
    PushOutcome::AppliedParked { published: count, superset: parked_superset.len() as u32 }
}

// ---------------------------------------------------------------------
// The committed path
// ---------------------------------------------------------------------

/// One mode the OS has COMMITTED on a path, as recorded by
/// `EvtIddCxAdapterCommitModes2`.
///
/// Deliberately not a [`Mode`]: `IDDCX_PATH2` carries a
/// `DISPLAYCONFIG_VIDEO_SIGNAL_INFO`, which has no bit depth and no HDR
/// flag, and inventing those to manufacture a `Mode` would let a
/// comparison succeed or fail on fields the OS never told us about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommittedMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

impl CommittedMode {
    /// Rebuild the committed mode from the path's signal block.
    ///
    /// `vSyncFreq` is a rational, and the OS is free to hand back a
    /// REDUCED fraction of what `signal_info` reported (we always send
    /// `refresh_millihz / 1000`), so the millihertz value is recomputed
    /// rather than read out of the numerator. A zero denominator or a zero
    /// dimension means the path carries nothing we can reason about —
    /// `None`, which every caller treats as "no committed mode", i.e. as
    /// today's behaviour.
    pub fn from_signal(width: u32, height: u32, vsync_num: u64, vsync_den: u64) -> Option<Self> {
        if width == 0 || height == 0 || vsync_den == 0 {
            return None;
        }
        let millihz = vsync_num.checked_mul(1000)? / vsync_den;
        if millihz == 0 || millihz > u64::from(u32::MAX) {
            return None;
        }
        Some(Self { width, height, refresh_millihz: millihz as u32 })
    }

    /// Is `mode` the mode the OS committed? Compared on the three fields a
    /// signal block actually carries.
    pub fn is(&self, mode: &Mode) -> bool {
        mode.width == self.width
            && mode.height == self.height
            && mode.refresh_millihz == self.refresh_millihz
    }
}

/// Slots in [`CommittedPaths`]. One per monitor the adapter can have
/// (`proto::ABI_MAX_MONITORS`); a commit carrying more paths than this
/// records the first `MAX_COMMITTED_PATHS` and is traced, which is
/// unreachable for a driver whose own session cap is the same number.
pub const MAX_COMMITTED_PATHS: usize = luminal_driver_proto::ABI_MAX_MONITORS as usize;

/// Bounded retries for one slot read. A read that keeps losing to a
/// concurrent commit reports "no committed mode", which degrades to the
/// pre-build-17 behaviour (push anyway) rather than to a spin — nothing in
/// a driver may retry without a bound, least of all on an effects worker.
const READ_RETRIES: u32 = 4;

/// The paths the OS last committed, keyed by IddCx monitor object.
///
/// # Why this is lock-free rather than a `Mutex<HashMap<..>>`
///
/// It is written from `EvtIddCxAdapterCommitModes2`, which is a CALLBACK
/// FRAME (DESIGN.md §3.3): the OS is inside a modeset transaction on our
/// thread, and anything that can block there can wedge a display change.
/// So the writer does a fixed number of atomic stores into a fixed array —
/// no allocation, no lock, no syscall — and the reader (the effects
/// worker, immediately before an `IddCxMonitorUpdateModes2`) does a
/// bounded seqlock read.
///
/// # The two races, and why neither is allowed to produce a phantom mode
///
/// What this record decides is whether to REFUSE a host's push, so a value
/// that never existed is worse than no value at all: "no committed mode"
/// fails open (push, as the driver always did), while a phantom mode can
/// refuse a legitimate request or — the other direction — wave through the
/// mid-stream modeset the gate exists to prevent.
///
/// 1. **Reader vs writer, within one slot.** Handled by a real seqlock:
///    [`CommittedSlot::version`], even/odd, written around every payload
///    change and validated by the reader before and after. The version is a
///    counter and NOT the monitor handle, which is what the first cut used:
///    a slot rewritten with the same handle (the same display committing
///    again — the commonest case there is) passes a handle-based
///    before/after check while the payload changes underneath it, pairing a
///    size from one commit with a refresh rate from the next.
/// 2. **Writer vs writer.** [`apply`](Self::apply) and
///    [`forget`](Self::forget) both rewrite the array, and they run on
///    unrelated threads: modeset callbacks, `evt_d0_exit`'s power callback,
///    the effects worker's plug/unplug. Two of them interleaving would
///    leave a mixture of two path sets that no commit ever produced. A
///    lock is not available here (callback frame), so they take a
///    non-blocking writer token instead, and a writer that cannot get it
///    does not wait, does not write, and marks the record CONTENDED — the
///    holder then clears everything rather than publishing a maybe-mixture.
///    Clearing is the fail-open direction, so the worst case of losing this
///    race is a push that goes through exactly as it did in build 16.
///
///    **The token and the contention flag are ONE atomic word**
///    (`state`), and the check is part of the release rather
///    than a step before it. Two separate flags left a window between "was
///    anybody turned away?" and "the token is free" (the guard's drop, one
///    statement later): a writer arriving inside it was turned away by a
///    token nobody would look at again, so its update was dropped AND the
///    record was left stale, AND the flag it set survived to erase the next
///    commit — a complete, uncontended one — instead. One word makes those
///    two events mutually exclusive: either the writer is turned away and
///    the holder is still there to clear, or it takes the token and writes.
pub struct CommittedPaths {
    slots: [CommittedSlot; MAX_COMMITTED_PATHS],
    /// Active paths in the last commit, which MAY exceed the number of
    /// slots recorded — `evt_commit_modes2` counts every active path it was
    /// handed and passes that total, so a commit wider than the array is
    /// reported honestly instead of silently truncated.
    active: AtomicU32,
    /// Commits observed. Two jobs: diagnostics (a trace showing a push
    /// refused at [`stage::GATES_COMMITTED`] can be read against the commit
    /// that set it), and the validity token behind
    /// [`committed_token`] — a remembered refusal is only evidence while
    /// this has not moved.
    generation: AtomicU64,
    /// Writer token and contention flag in ONE word — [`WRITING`] and
    /// [`CONTENDED`].
    ///
    /// One word rather than two booleans because the release has to answer
    /// "did anyone lose while I held this?" and hand the token back in a
    /// single atomic step. As two flags the answer was read first and the
    /// token released afterwards, and a writer arriving between them was
    /// turned away for nothing: its update dropped, the record left stale,
    /// and a `contended` flag left set with no holder to honour it — so the
    /// NEXT commit, complete and uncontended, was the one that got erased.
    state: AtomicU32,
}

/// [`CommittedPaths::state`]: nobody holds the writer token.
const FREE: u32 = 0;
/// [`CommittedPaths::state`]: a writer owns the array.
const WRITING: u32 = 1 << 0;
/// [`CommittedPaths::state`]: a writer was turned away while the token was
/// held, so its update was dropped and the record may be missing a write.
/// The HOLDER honours this on release by clearing the record — the
/// fail-open direction.
const CONTENDED: u32 = 1 << 1;

/// Bounded attempts to take the writer token. A retry exists only for the
/// case worth retrying: the holder released between our failed acquire and
/// our attempt to flag it, which means the token is free RIGHT NOW and one
/// more try records the commit properly instead of dropping it. Bounded
/// because [`CommittedPaths::apply`] runs on a modeset callback frame.
const ACQUIRE_ATTEMPTS: u32 = 4;

/// Bounded attempts to release the writer token. Each retry means another
/// writer was turned away while we were clearing; the record is cleared
/// again and the token re-offered. On exhaustion the token is handed back
/// unconditionally with the record CLEARED, which is the fail-open state.
const RELEASE_ATTEMPTS: u32 = 4;

struct CommittedSlot {
    /// Seqlock version: even = stable, odd = a write in progress. Read by
    /// [`CommittedPaths::get`] before and after the payload.
    version: AtomicU32,
    /// The `IDDCX_MONITOR` handle as an integer. 0 = empty.
    monitor: AtomicU64,
    /// `(width << 32) | height`.
    size: AtomicU64,
    refresh_millihz: AtomicU32,
}

impl CommittedSlot {
    const fn new() -> Self {
        Self {
            version: AtomicU32::new(0),
            monitor: AtomicU64::new(0),
            size: AtomicU64::new(0),
            refresh_millihz: AtomicU32::new(0),
        }
    }

    /// Publish one (monitor, mode) pairing, or clear the slot when
    /// `monitor` is 0. Caller holds the writer token, so this is the only
    /// writer; the odd/even fence pair is what a concurrent READER needs.
    fn publish(&self, monitor: u64, mode: CommittedMode) {
        let v = self.version.load(Ordering::Relaxed);
        // Odd: readers that see this discard whatever they read.
        self.version.store(v.wrapping_add(1), Ordering::Relaxed);
        // Release fence: the odd version is visible before any payload
        // store that follows, so a reader can never see new payload under
        // an even version.
        fence(Ordering::Release);
        self.monitor.store(monitor, Ordering::Relaxed);
        self.size.store((u64::from(mode.width) << 32) | u64::from(mode.height), Ordering::Relaxed);
        self.refresh_millihz.store(mode.refresh_millihz, Ordering::Relaxed);
        // Even again, with release ordering so the payload above
        // happens-before any reader that observes this version.
        self.version.store(v.wrapping_add(2), Ordering::Release);
    }

    fn clear(&self) {
        self.publish(0, CommittedMode { width: 0, height: 0, refresh_millihz: 0 });
    }

    /// One bounded seqlock read. `None` means "empty, or being written" —
    /// both of which the caller treats as no committed mode.
    fn read(&self) -> Option<(u64, CommittedMode)> {
        for _ in 0..READ_RETRIES {
            let before = self.version.load(Ordering::Acquire);
            if before & 1 != 0 {
                // A write is in progress. Try again rather than reading a
                // half-written slot; bounded, because nothing in a driver
                // spins without a limit.
                continue;
            }
            let monitor = self.monitor.load(Ordering::Relaxed);
            let size = self.size.load(Ordering::Relaxed);
            let refresh_millihz = self.refresh_millihz.load(Ordering::Relaxed);
            // Acquire fence: the payload loads above are ordered before the
            // version load below, so an equal version really does mean
            // nothing changed while they happened.
            fence(Ordering::Acquire);
            if self.version.load(Ordering::Relaxed) != before {
                continue;
            }
            if monitor == 0 {
                return None;
            }
            return Some((
                monitor,
                CommittedMode {
                    width: (size >> 32) as u32,
                    height: size as u32,
                    refresh_millihz,
                },
            ));
        }
        None
    }
}

impl Default for CommittedPaths {
    fn default() -> Self {
        Self::new()
    }
}

/// The writer token of a [`CommittedPaths`].
///
/// The release — including honouring anyone turned away while it was
/// held — happens in `Drop`, and ONLY there. That is the fix for the
/// window this type used to open: the contention check and the release
/// were two separate stores with the guard's drop in between, so a writer
/// arriving between them was refused by a token nobody would look at
/// again, and the flag it set outlived the write it described.
struct WriteToken<'a>(&'a CommittedPaths);

impl Drop for WriteToken<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl CommittedPaths {
    pub const fn new() -> Self {
        Self {
            slots: [const { CommittedSlot::new() }; MAX_COMMITTED_PATHS],
            active: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            state: AtomicU32::new(FREE),
        }
    }

    /// Take the writer token, or record that we could not and give up.
    ///
    /// Never waits: the callers include a modeset callback frame, where a
    /// wait of any length is a display change we might wedge. A writer that
    /// loses simply does not write — its update is dropped, and the flag it
    /// leaves makes the winner clear the record instead of publishing
    /// something that might be half of each.
    ///
    /// The flag is set with a CAS against `WRITING`, not an unconditional
    /// `fetch_or`, so it can only ever be attached to a holder that is
    /// still there to honour it. When that CAS fails the holder has just
    /// released, which means the token is free and one more attempt records
    /// this commit properly — so the loop retries rather than dropping a
    /// write it could have made. Bounded ([`ACQUIRE_ATTEMPTS`]): on
    /// exhaustion the flag is set unconditionally, which costs the next
    /// writer its record but keeps the guarantee that losing this race can
    /// only ever CLEAR the record — never leave a stale one behind.
    fn begin_write(&self) -> Option<WriteToken<'_>> {
        for _ in 0..ACQUIRE_ATTEMPTS {
            // Sets a bit that is already set when someone holds the token,
            // so the failure case is a no-op rather than a state change.
            let prev = self.state.fetch_or(WRITING, Ordering::AcqRel);
            if prev & WRITING == 0 {
                return Some(WriteToken(self));
            }
            if self
                .state
                .compare_exchange(
                    WRITING,
                    WRITING | CONTENDED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                // Bump: the record is now known to be losing a write, and
                // the generation is what expires cached refusals built on
                // it.
                self.generation.fetch_add(1, Ordering::AcqRel);
                return None;
            }
            // Either the holder released between the two atomics (retry
            // takes the token) or CONTENDED was already set by someone else
            // (the clear is already arranged, and a retry may still win the
            // token from the holder's release).
        }
        self.state.fetch_or(CONTENDED, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::AcqRel);
        None
    }

    /// What this write will be worth once the token goes back: `recorded`,
    /// or 0 if a writer has already been turned away and the record is
    /// therefore going to be cleared.
    ///
    /// It does NOT release — [`WriteToken::drop`] does, and it re-reads the
    /// flag there. This is a REPORT (it feeds the `CommitModes2` counts),
    /// not a promise: a writer turned away in the instant after this
    /// returns still causes the clear, and `get`/`active` are the authority
    /// on what the record holds.
    fn end_write(&self, recorded: u32) -> u32 {
        if self.state.load(Ordering::Acquire) & CONTENDED != 0 {
            0
        } else {
            recorded
        }
    }

    /// Hand the writer token back, honouring anyone turned away while it
    /// was held.
    ///
    /// The check and the release are ONE compare-exchange, which is the
    /// whole point: there is no instant in which the token is held by a
    /// writer that has stopped listening, so a `CONTENDED` flag can never
    /// outlive the write it describes and erase the next one instead.
    ///
    /// A writer WAS turned away ⇒ the record may be missing that write, so
    /// it is cleared. Clearing is the fail-open direction — "nothing
    /// committed" is the pre-gate behaviour — so losing this race can
    /// allow a push, never invent a refusal.
    fn release(&self) {
        for _ in 0..RELEASE_ATTEMPTS {
            self.generation.fetch_add(1, Ordering::AcqRel);
            if self
                .state
                .compare_exchange(WRITING, FREE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
            // Take the flag down but KEEP the token, so the clear below
            // cannot interleave with a new writer's publish.
            self.state.fetch_and(!CONTENDED, Ordering::AcqRel);
            for slot in &self.slots {
                slot.clear();
            }
            self.active.store(0, Ordering::Release);
        }
        // Bounded, because this runs on a modeset callback frame. The
        // record is cleared by the loop above, so handing the token back
        // unconditionally costs at most one more dropped update and still
        // leaves the fail-open state.
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.state.store(FREE, Ordering::Release);
    }

    /// Replace the whole record with the ACTIVE paths of one commit.
    ///
    /// Replace, not merge: `EvtIddCxAdapterCommitModes2` is handed the
    /// complete path set for the adapter, so a monitor absent from it is a
    /// monitor with no committed mode, and remembering a stale one would
    /// refuse pushes forever after a display was turned off.
    ///
    /// `active_total` is how many ACTIVE paths that commit carried, which
    /// may be more than fits: the count is reported (and traced) honestly
    /// while only `MAX_COMMITTED_PATHS` are recorded.
    ///
    /// Every slot is visited exactly once, whether it is being written or
    /// cleared. The first cut cleared all the keys and then filled the
    /// front of the array, which meant a slot could be written twice per
    /// commit and left a window in which a monitor that WAS committed read
    /// as uncommitted for no reason.
    ///
    /// Returns the number of paths recorded (0 if the write was abandoned).
    pub fn apply(&self, paths: &[(u64, CommittedMode)], active_total: u32) -> u32 {
        let Some(_token) = self.begin_write() else {
            return 0;
        };
        let mut recorded = 0u32;
        for (i, slot) in self.slots.iter().enumerate() {
            match paths.get(i) {
                Some(&(monitor, mode)) if monitor != 0 => {
                    slot.publish(monitor, mode);
                    recorded += 1;
                }
                _ => slot.clear(),
            }
        }
        self.active.store(active_total, Ordering::Release);
        self.end_write(recorded)
    }

    /// Forget one monitor — it departed, so whatever it had committed is
    /// gone with it and the handle may be reissued to a different monitor.
    ///
    /// Takes the writer token like [`apply`](Self::apply): it is a
    /// read-modify-write of the same array, and interleaving it with a
    /// commit could clear a slot the commit had just refilled with a
    /// DIFFERENT monitor's mode.
    pub fn forget(&self, monitor: u64) {
        if monitor == 0 {
            return;
        }
        let Some(_token) = self.begin_write() else {
            return;
        };
        let mut recorded = 0u32;
        for slot in &self.slots {
            match slot.read() {
                Some((m, _)) if m == monitor => slot.clear(),
                Some(_) => recorded += 1,
                None => {}
            }
        }
        self.end_write(recorded);
    }

    /// The mode the OS has committed on `monitor`, if any.
    pub fn get(&self, monitor: u64) -> Option<CommittedMode> {
        if monitor == 0 {
            return None;
        }
        self.slots.iter().find_map(|slot| match slot.read() {
            Some((m, mode)) if m == monitor => Some(mode),
            _ => None,
        })
    }

    /// The committed mode plus the token that says how long the answer is
    /// good for. The generation is read FIRST, so a commit racing this read
    /// can only make the token look OLDER than the mode it accompanies —
    /// which expires a refusal built on it early (one extra push, and the
    /// truth is re-derived) instead of keeping a stale refusal alive.
    pub fn get_with_token(&self, monitor: u64) -> (Option<CommittedMode>, u64) {
        let token = self.generation();
        (self.get(monitor), token)
    }

    /// Active paths in the last commit, which may exceed the number of
    /// slots recorded. Diagnostics.
    pub fn active(&self) -> u32 {
        self.active.load(Ordering::Acquire)
    }

    /// Commits observed, and the validity token for a remembered refusal.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------

/// What a proposed target list may do to a LIVE monitor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LiveGate {
    /// Publish it.
    Push,
    /// Publish it — but the OS has committed a mode this monitor's superset
    /// does not describe, so the committed-mode rule could not be applied.
    /// This is the deliberate FAIL-OPEN case (see [`live_gate`]); it is a
    /// distinct variant purely so the push site can say so in a trace,
    /// because a gate silently declining to fire looks exactly like a gate
    /// that had nothing to do.
    PushCommittedUnrecognised(CommittedMode),
    /// A target has no counterpart in this monitor's frozen description.
    /// The OS could never surface it (Windows offers the INTERSECTION), and
    /// publishing it would be invisible to the user and inexplicable in a
    /// trace.
    NotInSuperset,
    /// The list would gate out the mode the OS has COMMITTED on this
    /// monitor. `superset_idx` is that mode's index in the monitor's frozen
    /// description — the same list, in the same order, the host sent at
    /// `CREATE_MONITOR` — which is how the refusal tells the host WHICH
    /// mode is in the way instead of only that something is.
    EvictsCommitted { committed: CommittedMode, superset_idx: u32 },
}

impl LiveGate {
    /// Does this verdict allow the push? True for both push variants, so a
    /// call site cannot accidentally treat the fail-open case as a refusal
    /// (or forget it exists when a new variant is added).
    pub fn pushes(self) -> bool {
        matches!(self, LiveGate::Push | LiveGate::PushCommittedUnrecognised(_))
    }
}

/// Decide whether a validated target list may be pushed at a live monitor.
///
/// # The committed-mode rule
///
/// `UPDATE_MODES` steers what the OS MAY select. It never evicts what the
/// OS HAS selected.
///
/// Publishing a subset that excludes the committed mode forces Windows to
/// re-select on a display that is, under `virtual_display_layout=exclusive`,
/// the only active one — a modeset in the middle of the stream the feature
/// exists to avoid disturbing. Since the driver now knows what is
/// committed (`EvtIddCxAdapterCommitModes2` → [`CommittedPaths`]), it
/// refuses that one push rather than performing it blind. A refused PUSH is
/// not a refused session (constraint 1): the previously published list
/// stays in force, the monitor keeps working, the stream keeps running, and
/// the sticky `err::UPDATE_FAILED` tells the host to re-ask.
///
/// A host that wants the display on a DIFFERENT mode does the modeset
/// itself — `SetDisplayConfig`, which is what LuminalShine's display helper
/// already does for topology — and then gates, at which point the committed
/// mode is one it is keeping and this returns [`Push`](LiveGate::Push).
///
/// # Fail-open, deliberately
///
/// The refusal fires only when the committed mode is one this monitor's
/// superset actually describes. If the OS committed something we cannot
/// recognise — a decoding mismatch, a mode from a monitor object we are
/// not tracking — the push proceeds exactly as it did before this gate
/// existed, and the trace says so
/// ([`PushCommittedUnrecognised`](LiveGate::PushCommittedUnrecognised),
/// which the push site emits: the claim that it is traced was in this
/// comment one review pass before the event was). A gate that fired on an
/// unrecognised commit would silently turn the whole feature off.
pub fn live_gate(
    superset: &[Mode],
    targets: &[Mode],
    committed: Option<CommittedMode>,
) -> LiveGate {
    // THE containment check, against the LIVE monitor's own description
    // rather than the session table's copy: the two could in principle
    // have been reconciled by different paths (a replug rebuilding the
    // runtime, a session id reused), and this is the last statement before
    // the OS call.
    if !targets.iter().all(|m| superset.contains(m)) {
        return LiveGate::NotInSuperset;
    }
    if let Some(committed) = committed {
        match superset_index_of(superset, committed) {
            Some(superset_idx) => {
                if !targets.iter().any(|m| committed.is(m)) {
                    return LiveGate::EvictsCommitted { committed, superset_idx };
                }
            }
            // Fail open, and say so.
            None => return LiveGate::PushCommittedUnrecognised(committed),
        }
    }
    LiveGate::Push
}

/// Where `committed` sits in the monitor's frozen description, if it is one
/// of its modes.
///
/// The index — not the mode — is what travels to the host, because
/// `UpdateModesReply` may never grow and has exactly one spare word. It
/// costs nothing in precision: the superset IS the list the host sent at
/// `CREATE_MONITOR`, in that order, so an index names a mode the host
/// already has in hand.
pub fn superset_index_of(superset: &[Mode], committed: CommittedMode) -> Option<u32> {
    superset.iter().position(|m| committed.is(m)).map(|i| i as u32)
}

/// The `PreferredMonitorModeIdx` to report from the parse-description DDIs.
///
/// The monitor-mode list is frozen and `monitor_modes[0]` is the EDID's
/// preferred detailed timing, so 0 was the answer until build 17 made the
/// published TARGET subset a moving thing. Windows offers the
/// INTERSECTION of the two lists, so once a gate excludes
/// `monitor_modes[0]` a preferred index of 0 names a mode that cannot be
/// activated: the OS's default choice is unreachable, and nothing in a
/// trace says why.
///
/// So: the first monitor mode that is actually being offered. Falling back
/// to 0 when nothing matches keeps the answer in range whatever happens —
/// `targets ⊆ superset` makes a match certain, and a preferred index out
/// of range would be a far worse failure than a stale one. `fill` is the
/// number of modes actually written into the OS's buffer, because the
/// index must name one of those.
pub fn preferred_monitor_mode_idx(monitor_modes: &[Mode], targets: &[Mode], fill: usize) -> u32 {
    let limit = fill.min(monitor_modes.len());
    for (i, mode) in monitor_modes.iter().take(limit).enumerate() {
        if targets.contains(mode) {
            return i as u32;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::{caps, BitDepth, CreateMonitorRequest, ModeSpec};
    use luminal_vgd_core::adapter::AdapterInfo;
    use luminal_vgd_core::session::SessionTable;

    const CAPS: u32 = caps::MULTI_MODE | caps::DYNAMIC_MODES;
    const BASE: u32 = 120_000;
    const DOUBLED: u32 = 240_000;
    /// A stand-in commit generation. These tests drive the table directly,
    /// so nothing is committing anything; what matters is that a request
    /// and the refusal it is measured against carry the SAME token, which
    /// is exactly the field's contract.
    const TOKEN: u64 = 7;

    fn mode(refresh_millihz: u32) -> Mode {
        Mode {
            width: 2560,
            height: 1440,
            refresh_millihz,
            bit_depth: BitDepth::Sdr8,
            hdr: false,
        }
    }

    fn spec(refresh_millihz: u32) -> ModeSpec {
        ModeSpec { width: 2560, height: 1440, refresh_millihz }
    }

    fn adapters() -> Vec<AdapterInfo> {
        vec![AdapterInfo { luid: 0x20, vram_bytes: 16 << 30, name: "dGPU".into(), software: false }]
    }

    /// A table holding one monitor whose frozen superset is
    /// `[BASE, DOUBLED]`, with the whole superset published (nothing gated
    /// yet) — the state every session starts in.
    fn table_with_both_rates() -> SessionTable {
        let mut modes = [ModeSpec::default(); 4];
        modes[0] = spec(BASE);
        modes[1] = spec(DOUBLED);
        let req = CreateMonitorRequest {
            session_id: 1,
            display_id: 0,
            adapter_luid: 0,
            lease_timeout_ms: luminal_driver_proto::LEASE_TIMEOUT_USE_DEFAULT,
            bit_depth: 8,
            hdr: 0,
            edid_serial: 0,
            flags: 0,
            mode_count: 2,
            modes,
            physical_width_mm: 0,
            physical_height_mm: 0,
            friendly_name: [0; 32],
            max_nits: 0,
            reserved0: 0,
        };
        let mut t = SessionTable::new(4, 10);
        t.create(0, &req, CAPS, &adapters(), 0).expect("create");
        t
    }

    // -----------------------------------------------------------------
    // MAJOR 1 — the TDR-parked patch must go through the same
    // pending/settle discipline as every other push.
    // -----------------------------------------------------------------

    /// THE REGRESSION TEST for the parked-patch split brain.
    ///
    /// `parked_push` tells its caller to write the parked spec, so the
    /// monitor the replug re-arrives WILL publish the gated subset. If that
    /// same outcome settles `NotApplied`, the durable list keeps the old
    /// one, the two disagree permanently, and — the part that cannot be
    /// recovered from — the host's rescind is short-circuited by
    /// `SessionTable::update_modes` as "already published", emits no
    /// effect, and returns success while the monitor keeps offering the
    /// subset the host just took back.
    #[test]
    fn a_parked_patch_commits_durably_so_a_rescind_still_re_pushes() {
        let mut t = table_with_both_rates();
        let superset: Vec<Mode> = t.get(1).unwrap().modes.clone();

        // The host gates down to the doubled rate while the session is
        // parked in a TDR duck-out.
        let update = t.update_modes(1, &[spec(DOUBLED)], CAPS, TOKEN).expect("accepted");
        let seq = update.queued.expect("a real change queues a push");

        // The shell's parked arm: decide, patch the parked spec, settle.
        // Build 17 patched it and settled `NotApplied`; the fix makes
        // `commits()` both the authorisation to patch and the settle, so
        // the two halves cannot drift apart. Either way the parked spec
        // ends up holding `targets`, so the assertions below are about the
        // OTHER copy — the durable one — agreeing with it.
        let outcome = parked_push(&superset, &update.targets);
        assert!(t
            .settle_modes(1, seq, outcome.settle_result(), &update.targets)
            .settled());

        // Durable now agrees with what the re-arrival will publish.
        assert_eq!(
            t.get(1).unwrap().target_modes,
            vec![mode(DOUBLED)],
            "the durable list must match the parked spec the replug plugs with"
        );

        // THE CONSEQUENCE. The host changes its mind and rescinds the gate.
        // With the durable list committed this is a real change, so it
        // queues a real push; with it left behind, `update_modes` matches
        // the stale durable list, returns "already published", emits no
        // effect — and the monitor offers the gated subset forever.
        let rescind =
            t.update_modes(1, &[spec(BASE), spec(DOUBLED)], CAPS, TOKEN).expect("accepted");
        assert!(
            rescind.queued.is_some(),
            "rescinding a gate the driver applied must push, not short-circuit"
        );
        assert_eq!(rescind.targets.len(), 2);

        // And the shape, last: a patched parked spec is an APPLICATION,
        // reported separately from `Applied` because no OS call happened.
        assert_eq!(outcome, PushOutcome::AppliedParked { published: 1, superset: 2 });
        assert!(outcome.commits(), "which is also what authorises the patch");
    }

    /// The other direction of the same discipline: an outcome that changed
    /// NOTHING must not commit, or a deferral becomes a silent lie about
    /// what the monitor publishes.
    #[test]
    fn outcomes_that_changed_nothing_do_not_commit() {
        let committed = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        for outcome in [
            PushOutcome::Deferred { stage: stage::DUCK_PENDING, count: 1 },
            PushOutcome::Deferred { stage: stage::NO_ADAPTER, count: 1 },
            PushOutcome::Failed {
                stage: stage::OS_CALL,
                code: -1,
                count: 1,
                rolled_back: true,
            },
            PushOutcome::Failed {
                stage: stage::NOT_IN_SUPERSET,
                code: luminal_driver_proto::err::BAD_MODE,
                count: 1,
                rolled_back: false,
            },
        ] {
            assert!(!outcome.commits(), "{outcome:?}");
            assert_eq!(outcome.settle_result(), ModeUpdateResult::NotApplied, "{outcome:?}");
            assert!(outcome.retryable(), "{outcome:?}");
        }
        // The refusal changes nothing either — but it is the one outcome
        // that must NOT settle as an ordinary not-applied, because that is
        // the state the host is told to retry out of.
        let blocked =
            PushOutcome::Blocked { committed, superset_idx: 3, token: 9, count: 1 };
        assert!(!blocked.commits());
        assert!(!blocked.retryable());
        assert_eq!(
            blocked.settle_result(),
            ModeUpdateResult::Blocked { superset_idx: 3, token: 9 }
        );
        for outcome in [
            PushOutcome::Applied { published: 1, superset: 2 },
            PushOutcome::AppliedParked { published: 1, superset: 2 },
        ] {
            assert!(outcome.commits(), "{outcome:?}");
            assert_eq!(outcome.settle_result(), ModeUpdateResult::Applied, "{outcome:?}");
        }
    }

    /// A parked spec is still checked against the parked SUPERSET, and a
    /// refusal must not commit anything — the parked monitor keeps the
    /// selection it was parked with (constraint 1: never a monitor left
    /// with no targets).
    #[test]
    fn a_parked_push_outside_the_parked_superset_is_refused_and_commits_nothing() {
        let superset = vec![mode(BASE), mode(DOUBLED)];
        let outcome = parked_push(&superset, &[mode(60_000)]);
        assert_eq!(
            outcome,
            PushOutcome::Failed {
                stage: stage::NOT_IN_SUPERSET,
                code: luminal_driver_proto::err::BAD_MODE,
                count: 1,
                rolled_back: false,
            }
        );
        assert!(!outcome.commits());
        // And the empty list never reaches the DDI's "cannot be zero".
        assert!(matches!(
            parked_push(&superset, &[]),
            PushOutcome::Failed { stage: stage::EMPTY, .. }
        ));
    }

    // -----------------------------------------------------------------
    // MAJOR 2 — the committed path.
    // -----------------------------------------------------------------

    /// THE REGRESSION TEST for publishing over the OS's committed mode.
    ///
    /// Under `virtual_display_layout=exclusive` the virtual display is the
    /// ONLY active one. A push that drops the committed rate forces Windows
    /// to re-select on it — a modeset in the middle of the stream this
    /// whole feature exists to avoid. Before the commit capture the driver
    /// could not even detect the case.
    #[test]
    fn a_push_that_would_evict_the_committed_mode_is_refused() {
        let superset = vec![mode(BASE), mode(DOUBLED)];
        let committed = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };

        // Gating down to the doubled rate alone would evict it — and the
        // verdict names WHERE the blocking mode sits in the create-time
        // list, which is what the host is told so it can pick a different
        // subset instead of retrying the same one.
        assert_eq!(
            live_gate(&superset, &[mode(DOUBLED)], Some(committed)),
            LiveGate::EvictsCommitted { committed, superset_idx: 0 },
            "gating out the committed mode must be refused, not pushed blind"
        );

        // Keeping it is fine, gated or not.
        assert_eq!(live_gate(&superset, &[mode(BASE)], Some(committed)), LiveGate::Push);
        assert_eq!(
            live_gate(&superset, &[mode(BASE), mode(DOUBLED)], Some(committed)),
            LiveGate::Push
        );
        // Nothing committed (never activated, or the path went inactive):
        // gating is unconstrained.
        assert_eq!(live_gate(&superset, &[mode(DOUBLED)], None), LiveGate::Push);
        // The index is of the committed mode, wherever it sits.
        let doubled_committed =
            CommittedMode { width: 2560, height: 1440, refresh_millihz: DOUBLED };
        assert_eq!(
            live_gate(&superset, &[mode(BASE)], Some(doubled_committed)),
            LiveGate::EvictsCommitted { committed: doubled_committed, superset_idx: 1 }
        );
    }

    /// Fail-open: a committed mode this monitor's superset does not
    /// describe cannot be reasoned about, so the push proceeds exactly as
    /// it did before the gate existed. A gate that fired here would turn
    /// the feature off system-wide on any decoding mismatch.
    ///
    /// It is REPORTED as its own verdict, though. "Gate declined to fire
    /// because it could not read the commit" and "gate had nothing to do"
    /// are the same push with completely different diagnoses, and the push
    /// site traces the difference.
    #[test]
    fn an_unrecognised_committed_mode_never_refuses_a_push_but_is_still_reported() {
        let superset = vec![mode(BASE), mode(DOUBLED)];
        let alien = CommittedMode { width: 1920, height: 1080, refresh_millihz: 60_000 };
        let gate = live_gate(&superset, &[mode(DOUBLED)], Some(alien));
        assert_eq!(gate, LiveGate::PushCommittedUnrecognised(alien));
        assert!(gate.pushes(), "fail-open: the push still goes ahead");
        assert!(LiveGate::Push.pushes());
        assert!(!LiveGate::NotInSuperset.pushes());
        assert!(!LiveGate::EvictsCommitted { committed: alien, superset_idx: 0 }.pushes());
        assert_eq!(superset_index_of(&superset, alien), None);
    }

    /// THE REGRESSION TEST for the retry loop.
    ///
    /// The refusal is asynchronous — the IRP completes long before the
    /// effects worker pushes — so the ONLY request that can ever be told
    /// about it is a later one. Build 17 settled it as an ordinary
    /// `NotApplied`: sticky `UPDATE_FAILED`, whose documented meaning is
    /// "the previous list is still in force and this request is fully
    /// retryable — resending it really does push again". A host doing
    /// exactly that got its request queued, refused for the same reason,
    /// and told the same thing, forever.
    ///
    /// What the fix owes the host is two facts the wire could not carry:
    /// that the refusal is PERMANENT while that mode stays committed, and
    /// WHICH mode it is, so a different subset can be chosen instead of
    /// guessed at.
    #[test]
    fn a_refused_push_tells_the_host_it_is_permanent_and_which_mode_blocked_it() {
        let mut t = table_with_both_rates();
        let superset: Vec<Mode> = t.get(1).unwrap().modes.clone();
        // The OS is running the base rate on this display.
        let committed = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };

        // The host gates down to the doubled rate. Accepted and queued —
        // the driver cannot know yet, and says so with `pending`.
        let first = t.update_modes(1, &[spec(DOUBLED)], CAPS, TOKEN).expect("accepted");
        let seq = first.queued.expect("a real change queues a push");
        assert_eq!(first.blocked, None, "nothing has been tried yet");

        // The effects worker gets there and the gate refuses: publishing
        // {240} would evict the committed 120 and make Windows re-select on
        // the only active display, mid-stream.
        let gate = live_gate(&superset, &first.targets, Some(committed));
        let LiveGate::EvictsCommitted { committed, superset_idx } = gate else {
            panic!("expected a refusal, got {gate:?}");
        };
        let outcome = PushOutcome::Blocked { committed, superset_idx, token: TOKEN, count: 1 };
        assert!(!outcome.retryable(), "THE property the wire has to carry");
        assert!(!outcome.commits(), "and a refusal still changes nothing");
        assert!(t.settle_modes(1, seq, outcome.settle_result(), &first.targets).settled());

        // Constraint 1: the PUSH was refused, not the session. The list in
        // force is untouched and the monitor carries on.
        let m = t.get(1).unwrap();
        assert_eq!(m.target_modes, vec![mode(BASE), mode(DOUBLED)]);
        // The sticky code a host polling GET_STATUS reads is distinct from
        // the retryable one — the first of the two channels.
        assert_eq!(m.last_error, luminal_driver_proto::err::MODE_COMMITTED);
        assert_ne!(
            luminal_driver_proto::err::MODE_COMMITTED,
            luminal_driver_proto::err::UPDATE_FAILED
        );

        // THE CONSEQUENCE. The host retries the identical request — the one
        // thing `UPDATE_FAILED` told it to do. Before the fix this queued a
        // second push, which was refused identically, which told it to
        // retry: the loop. Now it is answered from the refusal itself.
        let retry = t.update_modes(1, &[spec(DOUBLED)], CAPS, TOKEN).expect("answered");
        assert_eq!(retry.blocked, Some(0), "and names create-time mode 0 as the blocker");
        assert_eq!(
            superset[retry.blocked.unwrap() as usize],
            mode(BASE),
            "which is the mode the OS is running — the host can now ask for a subset with it"
        );
        assert!(retry.queued.is_none(), "no second push: it could only be refused again");
        assert_eq!(retry.targets, vec![mode(BASE), mode(DOUBLED)], "still what is in force");

        // And the way out is open: a subset that KEEPS the committed mode
        // is a different question, and pushes for real.
        let narrower = t.update_modes(1, &[spec(BASE)], CAPS, TOKEN).expect("accepted");
        assert_eq!(narrower.blocked, None);
        assert!(narrower.queued.is_some(), "gating is still possible — just not over the OS");
        assert_eq!(live_gate(&superset, &narrower.targets, Some(committed)), LiveGate::Push);
    }

    /// A remembered refusal is evidence about what the OS is running, so it
    /// expires the moment anything commits. Otherwise the loop-breaker
    /// becomes its own bug: a host that legitimately moved the display onto
    /// the mode it wanted (`SetDisplayConfig`, which is the documented way
    /// out) would be refused for a mode that is no longer committed.
    #[test]
    fn a_remembered_refusal_expires_when_anything_commits() {
        let mut t = table_with_both_rates();
        let seq = t
            .update_modes(1, &[spec(DOUBLED)], CAPS, TOKEN)
            .expect("accepted")
            .queued
            .expect("queued");
        assert!(t
            .settle_modes(
                1,
                seq,
                ModeUpdateResult::Blocked { superset_idx: 0, token: TOKEN },
                &[mode(DOUBLED)],
            )
            .settled());
        assert_eq!(
            t.update_modes(1, &[spec(DOUBLED)], CAPS, TOKEN).unwrap().blocked,
            Some(0),
            "same world, same answer"
        );

        // A modeset happened: the token moves, so the cached refusal is no
        // longer evidence of anything and the request pushes for real.
        let after = t.update_modes(1, &[spec(DOUBLED)], CAPS, TOKEN + 1).expect("accepted");
        assert_eq!(after.blocked, None);
        assert!(after.queued.is_some(), "re-derive the truth rather than trust a stale refusal");
    }

    /// The superset check still comes first: it is the invariant that keeps
    /// the design safe under BOTH replace and append semantics.
    #[test]
    fn the_superset_check_precedes_the_committed_check() {
        let superset = vec![mode(BASE)];
        let committed = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        assert_eq!(
            live_gate(&superset, &[mode(DOUBLED)], Some(committed)),
            LiveGate::NotInSuperset
        );
    }

    /// The OS may hand back a REDUCED `vSyncFreq` fraction, so the
    /// millihertz value is recomputed from the rational rather than read
    /// out of the numerator we happened to send.
    #[test]
    fn a_committed_mode_survives_a_reduced_vsync_fraction() {
        let sent = CommittedMode::from_signal(2560, 1440, 240_000, 1000).unwrap();
        assert_eq!(sent.refresh_millihz, 240_000);
        // 240000/1000 reduced to 240/1, and to 24000/100.
        assert_eq!(CommittedMode::from_signal(2560, 1440, 240, 1), Some(sent));
        assert_eq!(CommittedMode::from_signal(2560, 1440, 24_000, 100), Some(sent));
        assert!(sent.is(&mode(DOUBLED)));
        assert!(!sent.is(&mode(BASE)));
        // Degenerate blocks are "no committed mode", never a panic and
        // never a division by zero on a callback frame.
        assert_eq!(CommittedMode::from_signal(2560, 1440, 240, 0), None);
        assert_eq!(CommittedMode::from_signal(0, 1440, 240, 1), None);
        assert_eq!(CommittedMode::from_signal(2560, 1440, 0, 1), None);
    }

    /// The record is REPLACED by every commit, so a monitor that leaves the
    /// path set stops having a committed mode. Remembering a stale one
    /// would refuse every later push on that monitor.
    #[test]
    fn a_commit_replaces_the_whole_record() {
        let paths = CommittedPaths::new();
        let a = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        let b = CommittedMode { width: 3840, height: 2160, refresh_millihz: 60_000 };
        assert_eq!(paths.get(0x11), None, "nothing committed yet");

        assert_eq!(paths.apply(&[(0x11, a), (0x22, b)], 2), 2);
        assert_eq!(paths.get(0x11), Some(a));
        assert_eq!(paths.get(0x22), Some(b));
        assert_eq!(paths.active(), 2);

        // Second display turned off: only one path comes back.
        paths.apply(&[(0x11, b)], 1);
        assert_eq!(paths.get(0x11), Some(b), "same monitor, new mode");
        assert_eq!(paths.get(0x22), None, "a monitor that left the path set has nothing committed");

        // Every path inactive: the whole record clears.
        paths.apply(&[], 0);
        assert_eq!(paths.get(0x11), None);
        assert_eq!(paths.active(), 0);
        assert!(paths.generation() >= 3, "every commit moves the token");
    }

    /// A monitor re-committing the SAME handle with a DIFFERENT mode is the
    /// commonest write there is (a rate change on the one active display),
    /// and it is exactly the case the first cut's validation could not
    /// catch: the handle was both key and sequence, so a reader that saw it
    /// before and after a rewrite concluded nothing had changed while the
    /// payload had. Whatever a read returns must be a pairing some commit
    /// actually made.
    #[test]
    fn a_rewrite_that_restores_the_same_key_is_still_detected() {
        let paths = CommittedPaths::new();
        let a = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        let b = CommittedMode { width: 3840, height: 2160, refresh_millihz: DOUBLED };
        paths.apply(&[(0x11, a)], 1);
        for _ in 0..64 {
            paths.apply(&[(0x11, b)], 1);
            assert_eq!(paths.get(0x11), Some(b));
            paths.apply(&[(0x11, a)], 1);
            let read = paths.get(0x11).expect("committed");
            // Never a mixture: 2560x1440@240 or 3840x2160@120 would both be
            // modes nobody ever committed, and both would be compared
            // against the superset to decide whether to refuse a push.
            assert!(read == a || read == b, "torn read: {read:?}");
            assert_eq!(read, a);
        }
    }

    /// Two writers must never interleave into one record. They cannot be
    /// serialised by waiting (a commit is a callback frame), so the loser
    /// drops its update and the record is CLEARED rather than left as a
    /// possible mixture of two path sets — the fail-open direction, i.e.
    /// exactly the pre-gate behaviour.
    #[test]
    fn a_lost_writer_clears_the_record_instead_of_mixing_two_commits() {
        let paths = CommittedPaths::new();
        let a = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        let b = CommittedMode { width: 3840, height: 2160, refresh_millihz: 60_000 };
        paths.apply(&[(0x11, a)], 1);
        assert_eq!(paths.get(0x11), Some(a));

        // Simulate the interleave the token prevents: enter a write, and
        // have a second writer arrive while it is held.
        let token = paths.begin_write().expect("uncontended");
        assert_eq!(paths.apply(&[(0x22, b)], 1), 0, "the loser writes nothing");
        let recorded = paths.end_write(1);
        drop(token);
        assert_eq!(recorded, 0, "and the holder clears rather than publish a mixture");
        assert_eq!(paths.get(0x11), None, "no committed mode ⇒ pushes proceed as before");
        assert_eq!(paths.get(0x22), None, "and the loser's paths never landed either");
        assert_eq!(paths.active(), 0);

        // Recovery is the next commit, like every other clear.
        paths.apply(&[(0x11, b)], 1);
        assert_eq!(paths.get(0x11), Some(b));
    }

    /// THE REGRESSION TEST for the writer-token loser (review pass 3).
    ///
    /// The turned-away writer in the test above arrives while the holder is
    /// still listening. This one arrives an instant later — AFTER the holder
    /// has asked "was anyone turned away?" and BEFORE the token actually
    /// goes back, which used to be two separate stores with a whole
    /// statement between them. A writer landing in that window was refused
    /// by a token nobody would look at again, and three things went wrong at
    /// once:
    ///
    /// * its update was dropped, so
    /// * the record was left STALE — holding a path set the OS had already
    ///   replaced. That is the fail-CLOSED direction: a stale committed mode
    ///   is what makes `live_gate` refuse a push that should have gone
    ///   through, which is precisely the invariant the type's own doc
    ///   promises ("losing this race can only ever allow a push, never
    ///   invent a refusal"); and
    /// * the flag it set survived with no holder left to honour it, so the
    ///   clear landed on the NEXT commit instead — a complete, uncontended
    ///   one, erased for a race it had nothing to do with.
    ///
    /// The token and the flag are one word now and the check is PART of the
    /// release, so the window does not exist: whoever is turned away is
    /// turned away by a holder that is still going to act on it.
    #[test]
    fn a_writer_turned_away_after_the_release_check_is_still_honoured() {
        let paths = CommittedPaths::new();
        let a = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        let b = CommittedMode { width: 3840, height: 2160, refresh_millihz: 60_000 };
        let c = CommittedMode { width: 1920, height: 1080, refresh_millihz: DOUBLED };
        paths.apply(&[(0x11, a)], 1);
        assert_eq!(paths.get(0x11), Some(a));

        // A holder enters, writes, and asks what its write will be worth
        // with nobody yet waiting on it.
        let token = paths.begin_write().expect("uncontended");
        assert_eq!(paths.end_write(1), 1, "nobody had been turned away when it looked");

        // THE WINDOW. A second commit arrives before the token goes back.
        assert_eq!(paths.apply(&[(0x22, b)], 1), 0, "still held, so its update is dropped");

        drop(token);

        // Someone has to honour that. The holder is the only candidate, and
        // it is still there, so the record is CLEARED — the fail-open state
        // — rather than left asserting a path set that has been superseded.
        assert_eq!(
            paths.get(0x11),
            None,
            "a dropped commit must never leave a stale one standing"
        );
        assert_eq!(paths.get(0x22), None, "and the loser's paths never landed either");
        assert_eq!(paths.active(), 0);

        // ...and the flag does not outlive it: the NEXT commit is complete
        // and uncontended, so it keeps its own record.
        assert_eq!(paths.apply(&[(0x11, c)], 1), 1);
        assert_eq!(paths.get(0x11), Some(c), "a stale flag must not erase a good commit");
        assert_eq!(paths.active(), 1);

        // The reader-facing invariant across the whole sequence: whatever
        // `get` returns is a pairing some commit really made, or nothing.
        // Both ways of getting that wrong — a mixture, or a superseded set
        // left standing — reach `live_gate` as a phantom mode.
        assert_eq!(paths.get(0x22), None);
    }

    /// A departed monitor's handle can be reissued to a different monitor
    /// object, so the record must be forgettable independently of the next
    /// commit.
    #[test]
    fn forgetting_a_departed_monitor_leaves_the_others_alone() {
        let paths = CommittedPaths::new();
        let a = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        let b = CommittedMode { width: 3840, height: 2160, refresh_millihz: 60_000 };
        paths.apply(&[(0x11, a), (0x22, b)], 2);
        paths.forget(0x11);
        assert_eq!(paths.get(0x11), None);
        assert_eq!(paths.get(0x22), Some(b));
        // A null handle is never a key, and forgetting it must not clear
        // the empty slots' meaning.
        paths.forget(0);
        assert_eq!(paths.get(0x22), Some(b));
        assert_eq!(paths.get(0), None);
    }

    /// More paths than slots records what fits and never writes out of
    /// bounds — unreachable while the session cap equals the slot count,
    /// which is exactly why it is asserted here rather than assumed.
    ///
    /// The count of ACTIVE paths is reported in full even so. It is the
    /// caller's count, not the array's: `active()` is what
    /// `UpdateModesGatesCommitted` reports as `active_paths`, and the fact
    /// that a refusal happened while N displays were active is the whole
    /// point of the field. Deriving it from the truncated record — which is
    /// what the first cut did, making the documented "may exceed the slots
    /// recorded" unreachable — would understate exactly the case worth
    /// seeing.
    #[test]
    fn more_paths_than_slots_is_bounded_but_the_active_count_is_not_truncated() {
        let paths = CommittedPaths::new();
        let m = CommittedMode { width: 2560, height: 1440, refresh_millihz: BASE };
        let many: Vec<(u64, CommittedMode)> =
            (1..=(MAX_COMMITTED_PATHS as u64 + 4)).map(|i| (i, m)).collect();
        assert_eq!(paths.apply(&many, many.len() as u32), MAX_COMMITTED_PATHS as u32);
        assert_eq!(paths.active(), many.len() as u32);
        assert!(paths.active() > MAX_COMMITTED_PATHS as u32);
        assert_eq!(paths.get(1), Some(m));
        assert_eq!(paths.get(MAX_COMMITTED_PATHS as u64), Some(m));
        assert_eq!(paths.get(MAX_COMMITTED_PATHS as u64 + 1), None);
    }

    // -----------------------------------------------------------------
    // MINOR — the preferred monitor mode.
    // -----------------------------------------------------------------

    /// `PreferredMonitorModeIdx` names a mode the OS can actually pick.
    /// With `monitor_modes[0]` gated out, a constant 0 pointed the OS's
    /// default choice at a mode outside the published intersection.
    #[test]
    fn the_preferred_index_names_a_mode_that_is_actually_offered() {
        let superset = vec![mode(BASE), mode(DOUBLED)];
        // Nothing gated: the EDID's preferred detailed timing, as always.
        assert_eq!(preferred_monitor_mode_idx(&superset, &superset, 2), 0);
        // Gated to the doubled rate: entry 0 is unreachable, so name entry 1.
        assert_eq!(preferred_monitor_mode_idx(&superset, &[mode(DOUBLED)], 2), 1);
        // Gated to the base rate: entry 0 again.
        assert_eq!(preferred_monitor_mode_idx(&superset, &[mode(BASE)], 2), 0);
    }

    /// The index must name a mode inside the buffer the OS was given, and
    /// must stay in range for inputs the invariant says cannot happen.
    #[test]
    fn the_preferred_index_stays_inside_the_reported_buffer() {
        let superset = vec![mode(BASE), mode(DOUBLED)];
        // Truncated buffer: entry 1 was not reported, so it cannot be named.
        assert_eq!(preferred_monitor_mode_idx(&superset, &[mode(DOUBLED)], 1), 0);
        assert_eq!(preferred_monitor_mode_idx(&superset, &[mode(DOUBLED)], 0), 0);
        // targets ⊄ superset is impossible by invariant; still in range.
        assert_eq!(preferred_monitor_mode_idx(&superset, &[mode(60_000)], 2), 0);
        assert_eq!(preferred_monitor_mode_idx(&[], &[mode(BASE)], 2), 0);
    }
}
