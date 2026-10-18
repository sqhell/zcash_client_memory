use std::ops::Add;

use zcash_protocol::consensus::BlockHeight;

use crate::types::block::MemoryWalletBlock;

/// Totals of the per-block Sapling output count and Orchard and Ironwood action counts over a
/// set of scanned blocks. Scan progress is reported as the share of the chain's outputs that
/// has been scanned, so these totals are its numerator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanCounts {
    pub(crate) sapling_outputs: u64,
    pub(crate) orchard_actions: u64,
    pub(crate) ironwood_actions: u64,
}

impl ScanCounts {
    /// The counts of a single scanned block. A block record written without a count contributes
    /// nothing for that pool, and a build without the `orchard` feature records no Orchard or
    /// Ironwood counts at all.
    pub(crate) fn of_block(block: &MemoryWalletBlock) -> Self {
        Self {
            sapling_outputs: u64::from(block.sapling_output_count.unwrap_or(0)),
            #[cfg(feature = "orchard")]
            orchard_actions: u64::from(block.orchard_action_count.unwrap_or(0)),
            #[cfg(not(feature = "orchard"))]
            orchard_actions: 0,
            #[cfg(feature = "orchard")]
            ironwood_actions: u64::from(block.ironwood_action_count.unwrap_or(0)),
            #[cfg(not(feature = "orchard"))]
            ironwood_actions: 0,
        }
    }
}

impl Add for ScanCounts {
    type Output = Self;

    /// Sums the counts. The totals are bounded by the number of outputs that can exist on
    /// chain, far below `u64::MAX`, so saturation is a formality.
    fn add(self, other: Self) -> Self {
        Self {
            sapling_outputs: self.sapling_outputs.saturating_add(other.sapling_outputs),
            orchard_actions: self.orchard_actions.saturating_add(other.orchard_actions),
            ironwood_actions: self.ironwood_actions.saturating_add(other.ironwood_actions),
        }
    }
}

/// The scan counts of a contiguous run of fully scanned heights whose block records may have
/// been pruned (see `MemoryWalletDb::prune_blocks`): every height `h` with
/// `counted_above < h <= counted_through`. Progress reporting adds these counts to those of the
/// retained block records outside the run, so a block record inside the run is redundant for it
/// and a block scanned again at a covered height is not counted twice.
///
/// Persisted, so a wallet keeps reporting progress after its block records are gone. Wallets
/// saved before block pruning have no counters; theirs are computed from the full block set
/// when they are decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanCounters {
    counted_above: BlockHeight,
    counted_through: BlockHeight,
    counts: ScanCounts,
}

impl ScanCounters {
    /// Counters holding `counts` for the heights `counted_above + 1 ..= counted_through`, or
    /// `None` if that run is empty.
    pub(crate) fn from_parts(
        counted_above: BlockHeight,
        counted_through: BlockHeight,
        counts: ScanCounts,
    ) -> Option<Self> {
        (counted_above < counted_through).then_some(Self {
            counted_above,
            counted_through,
            counts,
        })
    }

    /// The exclusive lower bound of the counted heights.
    pub(crate) fn counted_above(&self) -> BlockHeight {
        self.counted_above
    }

    /// The inclusive upper bound of the counted heights.
    pub(crate) fn counted_through(&self) -> BlockHeight {
        self.counted_through
    }

    /// The totals over the counted heights.
    pub(crate) fn counts(&self) -> ScanCounts {
        self.counts
    }

    /// Whether `height` is one of the counted heights.
    pub(crate) fn covers(&self, height: BlockHeight) -> bool {
        self.counted_above < height && height <= self.counted_through
    }
}

mod serialization {
    use super::*;
    use crate::error::{Error, Result};
    use crate::proto::memwallet as proto;

    impl From<&ScanCounters> for proto::ScanCounters {
        fn from(counters: &ScanCounters) -> Self {
            Self {
                counted_above: counters.counted_above.into(),
                counted_through: counters.counted_through.into(),
                sapling_outputs: counters.counts.sapling_outputs,
                orchard_actions: counters.counts.orchard_actions,
                ironwood_actions: counters.counts.ironwood_actions,
            }
        }
    }

    impl TryFrom<proto::ScanCounters> for ScanCounters {
        type Error = Error;

        /// Fails with [`Error::CorruptedData`] if the counted run of heights is empty, which
        /// this crate never writes.
        fn try_from(counters: proto::ScanCounters) -> Result<Self> {
            ScanCounters::from_parts(
                counters.counted_above.into(),
                counters.counted_through.into(),
                ScanCounts {
                    sapling_outputs: counters.sapling_outputs,
                    orchard_actions: counters.orchard_actions,
                    ironwood_actions: counters.ironwood_actions,
                },
            )
            .ok_or_else(|| {
                Error::CorruptedData(format!(
                    "scan counters cover no heights: counted above {} through {}",
                    counters.counted_above, counters.counted_through
                ))
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn scan_counters_roundtrip() {
            let counters = ScanCounters::from_parts(
                BlockHeight::from(10),
                BlockHeight::from(500),
                ScanCounts {
                    sapling_outputs: 1,
                    orchard_actions: 2,
                    ironwood_actions: 3,
                },
            )
            .unwrap();
            let proto = proto::ScanCounters::from(&counters);
            assert_eq!(ScanCounters::try_from(proto).unwrap(), counters);
        }

        #[test]
        fn an_empty_counted_run_is_rejected() {
            for (above, through) in [(10, 10), (11, 10)] {
                let proto = proto::ScanCounters {
                    counted_above: above,
                    counted_through: through,
                    sapling_outputs: 0,
                    orchard_actions: 0,
                    ironwood_actions: 0,
                };
                assert!(matches!(
                    ScanCounters::try_from(proto),
                    Err(Error::CorruptedData(_))
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_is_exclusive_below_and_inclusive_above() {
        let counters = ScanCounters::from_parts(
            BlockHeight::from(10),
            BlockHeight::from(20),
            ScanCounts::default(),
        )
        .unwrap();
        assert!(!counters.covers(BlockHeight::from(10)));
        assert!(counters.covers(BlockHeight::from(11)));
        assert!(counters.covers(BlockHeight::from(20)));
        assert!(!counters.covers(BlockHeight::from(21)));
    }

    #[test]
    fn from_parts_rejects_an_empty_run() {
        assert!(
            ScanCounters::from_parts(
                BlockHeight::from(10),
                BlockHeight::from(10),
                ScanCounts::default()
            )
            .is_none()
        );
    }
}
