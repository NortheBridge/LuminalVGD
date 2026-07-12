// SPDX-License-Identifier: AGPL-3.0-only
//! Display identity — stable "which monitor is this" separate from lease
//! lifetime (behavior folded in from libvirtualdisplay, MIT; see
//! THIRD-PARTY-NOTICES.md).
//!
//! Windows keys remembered display settings (mode, HDR, position, scaling)
//! on the EDID (manufacturer, product code, serial) and the connector a
//! monitor appears on. Give a returning client the same tuple and Windows
//! restores its setup; give it a fresh tuple and it's a brand-new display.
//! This module derives all identity-bearing values deterministically from
//! a `display_id`, and manages the connector reservations that keep a
//! given identity on the same IddCx connector across reconnects and driver
//! restarts.

use std::collections::BTreeMap;

/// Permanent-pool identities live in this range: index 0..MAX maps to
/// `PERMANENT_DISPLAY_ID_BASE | index`.
pub const PERMANENT_DISPLAY_ID_BASE: u64 = 0x7000_0000_0000_0000;
/// Ephemeral identities (throwaway, per-session) live in this range so
/// they can never collide with host-chosen stable ids by accident.
pub const EPHEMERAL_DISPLAY_ID_BASE: u64 = 0xE000_0000_0000_0000;

/// EDID product-code bases: permanent displays are `0x4000 + pool index`,
/// temporary (leased) displays are `0x5000 + connector`, so device-manager
/// archaeology can always tell the two kinds apart.
pub const PERMANENT_PRODUCT_CODE_BASE: u16 = 0x4000;
pub const TEMPORARY_PRODUCT_CODE_BASE: u16 = 0x5000;

pub const fn permanent_display_id(index: u32) -> u64 {
    PERMANENT_DISPLAY_ID_BASE | index as u64
}

pub const fn is_permanent_display_id(display_id: u64) -> bool {
    display_id & 0xF000_0000_0000_0000 == PERMANENT_DISPLAY_ID_BASE
}

/// Derive a throwaway identity from a session id.
pub const fn ephemeral_display_id(session_id: u64) -> u64 {
    EPHEMERAL_DISPLAY_ID_BASE | (session_id & 0x0FFF_FFFF_FFFF_FFFF)
}

pub const fn permanent_product_code(index: u32) -> u16 {
    PERMANENT_PRODUCT_CODE_BASE.wrapping_add(index as u16)
}

pub const fn temporary_product_code(connector_index: u32) -> u16 {
    TEMPORARY_PRODUCT_CODE_BASE.wrapping_add(connector_index as u16)
}

/// EDID serial derived from the identity (32-bit fold keeps distinct ids
/// distinct in practice while fitting the EDID field).
pub const fn serial_from_display_id(display_id: u64) -> u32 {
    (display_id ^ (display_id >> 32)) as u32
}

/// Deterministic container GUID for the monitor devnode, derived from the
/// LuminalVGD namespace GUID + display id. Same identity => same container
/// => Windows groups the device consistently in Settings.
pub fn container_guid_from_display_id(display_id: u64) -> (u32, u16, u16, [u8; 8]) {
    let (d1, d2, d3, d4) = luminal_driver_proto::LUMINAL_VGD_INTERFACE_GUID;
    let lo = display_id as u32;
    let hi = (display_id >> 32) as u32;
    let mut tail = d4;
    let bytes = display_id.to_le_bytes();
    let mut i = 0;
    while i < 8 {
        tail[i] ^= bytes[i];
        i += 1;
    }
    // Set the RFC 4122 "version 8 (custom)" and variant bits so the result
    // is a well-formed GUID distinct from the namespace itself.
    (
        d1 ^ lo,
        ((d2 ^ (hi as u16)) & 0x0FFF) | 0x8000,
        ((d3 ^ ((hi >> 16) as u16)) & 0x0FFF) | 0x8000,
        tail,
    )
}

/// Connector reservations: `display_id` → connector index. Reservations
/// outlive leases (and, via the persistence blob, driver restarts), which
/// is what keeps a returning identity on the same connector.
#[derive(Clone, Debug, Default)]
pub struct ConnectorTable {
    max_connectors: u32,
    /// display_id -> connector
    reservations: BTreeMap<u64, u32>,
    /// Monotonic age counter for least-recently-used eviction.
    next_age: u64,
    /// display_id -> age of last use.
    last_used: BTreeMap<u64, u64>,
}

impl ConnectorTable {
    pub fn new(max_connectors: u32) -> Self {
        Self { max_connectors, ..Self::default() }
    }

    pub fn reservations(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.reservations.iter().map(|(&d, &c)| (d, c))
    }

    /// Restore reservations from persisted state (invalid entries are
    /// dropped, not trusted).
    pub fn restore(&mut self, entries: impl IntoIterator<Item = (u64, u32)>) {
        for (display_id, connector) in entries {
            if connector < self.max_connectors
                && !self.reservations.values().any(|&c| c == connector)
            {
                self.reservations.insert(display_id, connector);
                let age = self.next_age;
                self.next_age += 1;
                self.last_used.insert(display_id, age);
            }
        }
    }

    /// Get the connector for an identity, reusing its reservation when one
    /// exists, else claiming the lowest free connector, else evicting the
    /// least-recently-used reservation NOT in `active` (identities with a
    /// live monitor are never evicted). `None` = genuinely full.
    pub fn acquire(&mut self, display_id: u64, active: &[u64]) -> Option<u32> {
        let age = self.next_age;
        self.next_age += 1;

        if let Some(&c) = self.reservations.get(&display_id) {
            self.last_used.insert(display_id, age);
            return Some(c);
        }

        // Lowest free connector.
        let used: Vec<u32> = self.reservations.values().copied().collect();
        let free = (0..self.max_connectors).find(|c| !used.contains(c));
        let connector = match free {
            Some(c) => c,
            None => {
                // Evict the LRU reservation whose identity is not live.
                let victim = self
                    .reservations
                    .keys()
                    .filter(|d| !active.contains(d))
                    .min_by_key(|d| self.last_used.get(d).copied().unwrap_or(0))
                    .copied()?;
                let c = self.reservations.remove(&victim).expect("victim exists");
                self.last_used.remove(&victim);
                c
            }
        };
        self.reservations.insert(display_id, connector);
        self.last_used.insert(display_id, age);
        Some(connector)
    }

    /// Drop a reservation entirely (ephemeral identities on destroy).
    pub fn release(&mut self, display_id: u64) {
        self.reservations.remove(&display_id);
        self.last_used.remove(&display_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_derivations_are_stable_and_partitioned() {
        assert_eq!(permanent_display_id(2), 0x7000_0000_0000_0002);
        assert!(is_permanent_display_id(permanent_display_id(0)));
        assert!(!is_permanent_display_id(0x1234));
        assert!(!is_permanent_display_id(ephemeral_display_id(7)));

        assert_eq!(permanent_product_code(1), 0x4001);
        assert_eq!(temporary_product_code(3), 0x5003);

        // Serial folds are deterministic and differ across ids.
        let a = serial_from_display_id(0xAAAA_0000_0000_0001);
        let b = serial_from_display_id(0xAAAA_0000_0000_0002);
        assert_eq!(a, serial_from_display_id(0xAAAA_0000_0000_0001));
        assert_ne!(a, b);
    }

    #[test]
    fn container_guid_is_deterministic_well_formed_and_distinct() {
        let g1 = container_guid_from_display_id(1);
        let g2 = container_guid_from_display_id(2);
        assert_eq!(g1, container_guid_from_display_id(1));
        assert_ne!(g1, g2);
        assert_ne!(g1, luminal_driver_proto::LUMINAL_VGD_INTERFACE_GUID);
    }

    #[test]
    fn returning_identity_gets_its_connector_back() {
        let mut t = ConnectorTable::new(4);
        assert_eq!(t.acquire(0xA, &[]), Some(0));
        assert_eq!(t.acquire(0xB, &[]), Some(1));
        // A reconnects (lease died, identity persists): same connector.
        assert_eq!(t.acquire(0xA, &[]), Some(0));
    }

    #[test]
    fn full_table_evicts_lru_unreserved_but_never_active() {
        let mut t = ConnectorTable::new(2);
        t.acquire(0xA, &[]);
        t.acquire(0xB, &[]);
        // Touch A so B is LRU.
        t.acquire(0xA, &[]);

        // C arrives; B (LRU, not active) is evicted.
        assert_eq!(t.acquire(0xC, &[0xA]), Some(1));
        // B lost its reservation; returning B now must evict someone.
        // With both A and C active, there is no victim: full.
        assert_eq!(t.acquire(0xB, &[0xA, 0xC]), None);
    }

    #[test]
    fn restore_ignores_garbage_and_duplicate_connectors() {
        let mut t = ConnectorTable::new(2);
        t.restore([
            (0xA, 0),
            (0xB, 0),  // duplicate connector: dropped
            (0xC, 99), // out of range: dropped
        ]);
        let all: Vec<_> = t.reservations().collect();
        assert_eq!(all, vec![(0xA, 0)]);
        // Restored reservation honored on reconnect.
        assert_eq!(t.acquire(0xA, &[]), Some(0));
    }

    #[test]
    fn release_frees_the_connector() {
        let mut t = ConnectorTable::new(1);
        assert_eq!(t.acquire(0xA, &[]), Some(0));
        t.release(0xA);
        assert_eq!(t.acquire(0xB, &[]), Some(0));
    }
}
