// SPDX-License-Identifier: AGPL-3.0-only
//! Core error type, one-to-one with the wire codes in `proto::err` so the
//! IOCTL dispatcher can translate without judgment calls.

use luminal_driver_proto::err;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreError {
    ProtoMismatch,
    MaxMonitors,
    BadMode,
    BadBitDepth,
    HdrUnsupported,
    NoAdapter,
    DuplicateSession,
    NoSuchSession,
    RingAlloc,
    NotHandshaken,
    Internal,
}

impl CoreError {
    /// The wire code carried in reply `result` fields.
    pub const fn code(self) -> i32 {
        match self {
            Self::ProtoMismatch => err::PROTO_MISMATCH,
            Self::MaxMonitors => err::MAX_MONITORS,
            Self::BadMode => err::BAD_MODE,
            Self::BadBitDepth => err::BAD_BIT_DEPTH,
            Self::HdrUnsupported => err::HDR_UNSUPPORTED,
            Self::NoAdapter => err::NO_ADAPTER,
            Self::DuplicateSession => err::DUPLICATE_SESSION,
            Self::NoSuchSession => err::NO_SUCH_SESSION,
            Self::RingAlloc => err::RING_ALLOC,
            Self::NotHandshaken => err::NOT_HANDSHAKEN,
            Self::Internal => err::INTERNAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_proto() {
        assert_eq!(CoreError::MaxMonitors.code(), err::MAX_MONITORS);
        assert_eq!(CoreError::NoSuchSession.code(), err::NO_SUCH_SESSION);
        assert_eq!(CoreError::Internal.code(), err::INTERNAL);
    }
}
