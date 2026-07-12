// SPDX-License-Identifier: AGPL-3.0-only
//! Persistent driver state: connector reservations + permanent-pool
//! config, serialized to an opaque, schema-versioned blob the shell stores
//! under the device's registry key (libvirtualdisplay keeps equivalent
//! state; identity retention across restarts depends on it).
//!
//! Format (little-endian, no padding):
//! ```text
//! magic  u32 = "LVGP"
//! schema u32 = 1
//! reservation_count u32
//! [display_id u64, connector u32] × count
//! pool_present u32 (0/1)
//! PermanentPoolConfig fields (u32×8 + name u16×32)  — if present
//! ```
//! Parsing is defensive: wrong magic, short buffers, or a newer schema
//! yield `None` and the driver starts fresh (never trust stored bytes).

use luminal_driver_proto::PermanentPoolConfig;

pub const PERSIST_MAGIC: u32 = 0x4C56_4750; // "PGVL" LE => "LVGP"
pub const PERSIST_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersistedState {
    pub reservations: Vec<(u64, u32)>,
    pub pool: Option<PermanentPoolConfig>,
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Option<u32> {
        let b = self.data.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        let b = self.data.get(self.at..self.at + 8)?;
        self.at += 8;
        Some(u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn u16(&mut self) -> Option<u16> {
        let b = self.data.get(self.at..self.at + 2)?;
        self.at += 2;
        Some(u16::from_le_bytes(b.try_into().unwrap()))
    }
}

pub fn serialize(state: &PersistedState) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + state.reservations.len() * 12 + 100);
    put_u32(&mut out, PERSIST_MAGIC);
    put_u32(&mut out, PERSIST_SCHEMA);
    put_u32(&mut out, state.reservations.len() as u32);
    for &(display_id, connector) in &state.reservations {
        put_u64(&mut out, display_id);
        put_u32(&mut out, connector);
    }
    put_u32(&mut out, u32::from(state.pool.is_some()));
    if let Some(p) = &state.pool {
        for v in [
            p.count,
            p.width,
            p.height,
            p.refresh_millihz,
            p.bit_depth,
            p.hdr,
            p.physical_width_mm,
            p.physical_height_mm,
        ] {
            put_u32(&mut out, v);
        }
        for ch in p.name {
            out.extend_from_slice(&ch.to_le_bytes());
        }
    }
    out
}

pub fn parse(blob: &[u8]) -> Option<PersistedState> {
    let mut r = Reader { data: blob, at: 0 };
    if r.u32()? != PERSIST_MAGIC {
        return None;
    }
    if r.u32()? != PERSIST_SCHEMA {
        // Unknown schema (older driver reading newer state): start fresh.
        return None;
    }
    let count = r.u32()?;
    // Reservation count is bounded by connectors; anything huge is garbage.
    if count > 1024 {
        return None;
    }
    let mut reservations = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let display_id = r.u64()?;
        let connector = r.u32()?;
        reservations.push((display_id, connector));
    }
    let pool = if r.u32()? != 0 {
        let mut vals = [0u32; 8];
        for v in &mut vals {
            *v = r.u32()?;
        }
        let mut name = [0u16; 32];
        for ch in &mut name {
            *ch = r.u16()?;
        }
        Some(PermanentPoolConfig {
            count: vals[0],
            width: vals[1],
            height: vals[2],
            refresh_millihz: vals[3],
            bit_depth: vals[4],
            hdr: vals[5],
            physical_width_mm: vals[6],
            physical_height_mm: vals[7],
            name,
        })
    } else {
        None
    };
    Some(PersistedState { reservations, pool })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> PermanentPoolConfig {
        let mut name = [0u16; 32];
        for (i, c) in "Desk".encode_utf16().enumerate() {
            name[i] = c;
        }
        PermanentPoolConfig {
            count: 2,
            width: 2560,
            height: 1440,
            refresh_millihz: 120_000,
            bit_depth: 8,
            hdr: 0,
            physical_width_mm: 700,
            physical_height_mm: 390,
            name,
        }
    }

    #[test]
    fn round_trips_full_state() {
        let state = PersistedState {
            reservations: vec![(0xCAFE, 0), (0xBEEF, 3)],
            pool: Some(pool()),
        };
        assert_eq!(parse(&serialize(&state)), Some(state));
    }

    #[test]
    fn round_trips_empty_state() {
        let state = PersistedState::default();
        assert_eq!(parse(&serialize(&state)), Some(state));
    }

    #[test]
    fn rejects_garbage_wrong_magic_future_schema_and_truncation() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0xFF; 64]), None);

        let good = serialize(&PersistedState {
            reservations: vec![(1, 0)],
            pool: Some(pool()),
        });
        // Wrong magic.
        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        assert_eq!(parse(&bad), None);
        // Future schema.
        let mut future = good.clone();
        future[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(parse(&future), None);
        // Truncated at every point: never panics, returns None.
        for len in 0..good.len() {
            assert_eq!(parse(&good[..len]), None, "truncated at {len}");
        }
        // Absurd reservation count.
        let mut huge = good;
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&huge), None);
    }

    #[test]
    fn trailing_bytes_are_tolerated() {
        // A future minor writer may append fields; same-schema readers
        // must not choke on extra tail.
        let mut blob = serialize(&PersistedState::default());
        blob.extend_from_slice(&[1, 2, 3]);
        assert_eq!(parse(&blob), Some(PersistedState::default()));
    }
}
