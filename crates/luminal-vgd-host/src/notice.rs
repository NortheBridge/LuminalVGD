// SPDX-License-Identifier: AGPL-3.0-only
//! User-facing status notices for capture-mode changes.
//!
//! Product requirement (2026-07, supersedes the original "recovery at next
//! session" rule in DESIGN.md §2): a fallback from direct encoding to WGC
//! is **seamless** — the stream keeps running, and **no Windows toast is
//! ever raised**. LuminalShine surfaces the state change in its own UI
//! (web dashboard / status endpoint) and logs, nothing OS-level.
//!
//! That is enforced structurally here: [`NoticeChannel`] has no OS-toast
//! variant, so no code path in the controller — present or future — can
//! ask for one without changing this file and tripping its tests.

/// Where a notice may be delivered. Deliberately a single variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeChannel {
    /// LuminalShine's own surfaces only: structured log line + status
    /// visible in the web UI. Never an OS notification.
    HostUiOnly,
}

/// Why the controller left (or failed to enter) direct encoding.
/// Reason codes are the Insider-regression early-warning telemetry
/// (WGC-RELIABILITY.md §4) — always logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    /// No LuminalVGD control device present.
    DriverAbsent,
    /// Handshake failed or proto major mismatch.
    HandshakeFailed(i32),
    /// CREATE_MONITOR refused (carries the proto error code).
    CreateFailed(i32),
    /// `driver_heartbeat_qpc` went stale: driver gone or wedged.
    HeartbeatStale,
    /// Ring reported `REBUILDING` (TDR / device reset in progress).
    RingRebuilding,
    /// Ring reported `DEAD`.
    RingDead,
    /// Frame-sequence watchdog starved while direct was active.
    DirectStarvation,
    /// WGC itself starved / trip while in fallback (ladder input).
    WgcStarvation,
    /// Mode/topology change while in fallback (immediate R1 per
    /// WGC-RELIABILITY.md §5).
    ModeChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// Direct encoding lost; WGC carrying the stream.
    FellBackToWgc,
    /// Direct encoding restored mid-session.
    DirectRestored,
    /// Last-resort DDA in use.
    FellBackToDda,
    /// Session failed loudly (ladder exhausted, R6).
    SessionFailed,
}

/// A structured status notice. `text` is the exact copy LuminalShine's UI
/// shows; the stream itself is never interrupted by delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub channel: NoticeChannel,
    pub reason: Option<FallbackReason>,
    pub text: &'static str,
}

/// UI copy, per the product requirement wording.
pub const FELL_BACK_TEXT: &str = "Direct encoding is temporarily unavailable. LuminalShine has \
     fallen back to Windows Graphics Capture and will try restoring \
     direct encoding as soon as possible.";
pub const RESTORED_TEXT: &str = "Direct encoding restored.";
pub const DDA_TEXT: &str = "Windows Graphics Capture is unavailable. LuminalShine is using \
     Desktop Duplication until capture can be restored.";
pub const FAILED_TEXT: &str = "Capture failed and could not be recovered. The session was \
     ended; a log bundle has been written.";

impl Notice {
    pub fn fell_back(reason: FallbackReason) -> Self {
        Self {
            kind: NoticeKind::FellBackToWgc,
            channel: NoticeChannel::HostUiOnly,
            reason: Some(reason),
            text: FELL_BACK_TEXT,
        }
    }

    pub fn restored() -> Self {
        Self {
            kind: NoticeKind::DirectRestored,
            channel: NoticeChannel::HostUiOnly,
            reason: None,
            text: RESTORED_TEXT,
        }
    }

    pub fn dda(reason: FallbackReason) -> Self {
        Self {
            kind: NoticeKind::FellBackToDda,
            channel: NoticeChannel::HostUiOnly,
            reason: Some(reason),
            text: DDA_TEXT,
        }
    }

    pub fn failed(reason: FallbackReason) -> Self {
        Self {
            kind: NoticeKind::SessionFailed,
            channel: NoticeChannel::HostUiOnly,
            reason: Some(reason),
            text: FAILED_TEXT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_notice_is_host_ui_only() {
        // The channel type has one variant; this test exists so that
        // adding an OS-toast variant forces a deliberate decision here.
        for n in [
            Notice::fell_back(FallbackReason::HeartbeatStale),
            Notice::restored(),
            Notice::dda(FallbackReason::WgcStarvation),
            Notice::failed(FallbackReason::WgcStarvation),
        ] {
            assert_eq!(n.channel, NoticeChannel::HostUiOnly);
        }
    }

    #[test]
    fn fallback_copy_promises_restore() {
        let n = Notice::fell_back(FallbackReason::RingDead);
        assert!(n.text.contains("Windows Graphics Capture"));
        assert!(n.text.contains("as soon as possible"));
    }
}
