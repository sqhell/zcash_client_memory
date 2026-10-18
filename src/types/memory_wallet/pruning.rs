//! Pruning of block records.
//!
//! `put_blocks` records every scanned block, but only a handful of those records are ever read
//! again: the blocks a rewind or a resumed scan connects to, the fully scanned block, the
//! birthday block, and the blocks the wallet's transactions were mined in. A wallet with a
//! million-block history would otherwise carry a million mostly empty records for the life of
//! the wallet, which is more than a browser can decode. The per-block output counts that scan
//! progress needs are folded into [`ScanCounters`] before a record is dropped.

use std::{cmp::Ordering, collections::BTreeSet};

use zcash_client_backend::data_api::scanning::ScanPriority;
use zcash_protocol::consensus::{self, BlockHeight};

use super::MemoryWalletDb;
use crate::{
    PRUNING_DEPTH,
    error::Error,
    types::{ScanCounters, ScanCounts, scanning::ScanQueue},
};

/// Why a block record must be kept: every reader of the block table and the heights it looks
/// up. A record at a height outside all of these sets is never read again.
///
/// [`Self::retains`] is the predicate; [`MemoryWalletDb::prune_blocks`] gathers the sets from
/// the wallet state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockRetention {
    /// The highest scanned height. The `PRUNING_DEPTH` blocks below it are kept so that a
    /// rewind within reorg depth, as `update_chain_tip` assumes, finds its target block.
    pub(crate) max_scanned: BlockHeight,
    /// The heights a rewind can land on: every tree has a checkpoint there. After a rewind the
    /// target's record is the fully scanned block and the continuity anchor of the next scan.
    pub(crate) rewind_targets: BTreeSet<BlockHeight>,
    /// The fully scanned height, whose metadata `block_fully_scanned` returns.
    pub(crate) fully_scanned: Option<BlockHeight>,
    /// The wallet birthday. The scan-progress denominators fall back to the tree sizes of the
    /// block at or below it, so nothing at or below the birthday is dropped.
    pub(crate) birthday: BlockHeight,
    /// The mined heights of the wallet's transactions: `get_block_time` reads their timestamps
    /// for the transaction history, and `put_transparent_output` their records.
    pub(crate) tx_heights: BTreeSet<BlockHeight>,
    /// The heights a resumed scan checks continuity against; see [`scan_boundaries`].
    pub(crate) scan_boundaries: BTreeSet<BlockHeight>,
}

impl BlockRetention {
    /// Whether the block record at `height` may still be read.
    pub(crate) fn retains(&self, height: BlockHeight) -> bool {
        height + PRUNING_DEPTH >= self.max_scanned
            || self.rewind_targets.contains(&height)
            || Some(height) == self.fully_scanned
            || height <= self.birthday
            || self.tx_heights.contains(&height)
            || self.scan_boundaries.contains(&height)
    }
}

/// The heights a scan that starts at a range boundary looks up to check chain continuity:
/// `start - 1` of every range still to be scanned (any priority above `Scanned`), and `end - 1`
/// of every scanned range, which is the block the range after it connects to.
pub(crate) fn scan_boundaries(queue: &ScanQueue) -> BTreeSet<BlockHeight> {
    let mut boundaries = BTreeSet::new();
    for (start, end, priority) in queue.iter() {
        match priority.cmp(&ScanPriority::Scanned) {
            Ordering::Greater => {
                if *start > BlockHeight::from(0) {
                    boundaries.insert(*start - 1);
                }
            }
            Ordering::Equal => {
                boundaries.insert(*end - 1);
            }
            Ordering::Less => {}
        }
    }
    boundaries
}

impl<P: consensus::Parameters> MemoryWalletDb<P> {
    /// Drops the block records that nothing will read again and returns how many were dropped.
    /// Called at the end of every `put_blocks`, and once when a wallet saved before block
    /// pruning is decoded.
    ///
    /// # Retention rule
    ///
    /// A record at height `h` is kept if any of the following holds (see [`BlockRetention`]):
    ///
    /// - `h + PRUNING_DEPTH >= max_scanned`: the rewind window;
    /// - every tree has a checkpoint at `h`, so a rewind can land on it;
    /// - `h` is the fully scanned height;
    /// - `h` is at or below the wallet birthday;
    /// - `h` is the mined height of a transaction in the transaction table;
    /// - `h == start - 1` of a scan-queue range with priority above `Scanned`, or `h == end - 1`
    ///   of a `Scanned` range;
    /// - `h` is outside the run of heights the scan counters cover, so that no count is lost.
    ///
    /// # Scan counters
    ///
    /// The counters are extended first, over `(birthday, through]` where `through` is the
    /// least of the fully scanned height, the last height below the rewind window and the last
    /// height below the oldest rewind target. Every height in that run has been scanned (the
    /// fully scanned height is contiguous from the birthday) and is either in the previous run
    /// or still has its record, so the totals are exact. The run only grows, and a rewind
    /// cannot reach into it: a rewind lands on a checkpoint, and the run ends below the oldest
    /// one. The exception is a forced rescan (adding an account whose birthday is below the
    /// run's end), which checkpoints the block before its first batch; a rewind to that height
    /// leaves the counts of the cut heights in place, over-reported until they are scanned again
    /// and exact after that.
    ///
    /// Only the heights above the fully scanned height can accumulate uncounted records, when a
    /// range near the chain tip is scanned before the historic range below it; they are dropped
    /// once the historic scan reaches them.
    pub(crate) fn prune_blocks(&mut self) -> Result<usize, Error> {
        let (Some(birthday), Some(max_scanned)) =
            (self.wallet_birthday(), self.max_scanned_height())
        else {
            return Ok(0);
        };
        let fully_scanned = self.fully_scanned_height()?;
        let rewind_targets = self.rewindable_checkpoint_heights()?;

        // The highest height whose record may be dropped: below the rewind window, below every
        // rewind target, and no higher than the fully scanned height.
        let window_floor = max_scanned.saturating_sub(PRUNING_DEPTH);
        let through = fully_scanned
            .filter(|_| window_floor > BlockHeight::from(0))
            .map(|fully_scanned| fully_scanned.min(window_floor - 1))
            .map(|through| match rewind_targets.first() {
                Some(oldest) if *oldest > BlockHeight::from(0) => through.min(*oldest - 1),
                Some(_) => BlockHeight::from(0),
                None => through,
            });

        let extended = self.extend_scan_counters(birthday, fully_scanned, through);
        self.scan_counters = extended.or(self.scan_counters);
        let Some(counters) = self.scan_counters else {
            return Ok(0);
        };

        let retention = BlockRetention {
            max_scanned,
            rewind_targets,
            fully_scanned,
            birthday,
            tx_heights: self
                .tx_table
                .0
                .values()
                .filter_map(|tx| tx.mined_height())
                .collect(),
            scan_boundaries: scan_boundaries(&self.scan_queue),
        };

        let before = self.blocks.len();
        self.blocks
            .retain(|height, _| !counters.covers(*height) || retention.retains(*height));
        Ok(before - self.blocks.len())
    }

    /// The scan counters extended to cover `(birthday, through]` in addition to the heights the
    /// current counters cover, or `None` if that adds nothing. The lower bound only moves down
    /// to `birthday` once the fully scanned height has reached the current lower bound, so that
    /// every newly covered height has been scanned and still has its record.
    fn extend_scan_counters(
        &self,
        birthday: BlockHeight,
        fully_scanned: Option<BlockHeight>,
        through: Option<BlockHeight>,
    ) -> Option<ScanCounters> {
        let (above, through, counts) = match self.scan_counters {
            Some(current) => {
                let above = if fully_scanned.is_some_and(|f| f >= current.counted_above()) {
                    birthday.min(current.counted_above())
                } else {
                    current.counted_above()
                };
                let through = through.map_or(current.counted_through(), |through| {
                    through.max(current.counted_through())
                });
                (above, through, current.counts())
            }
            None => (birthday, through?, ScanCounts::default()),
        };
        if through <= above {
            return None;
        }
        if self.scan_counters.is_some_and(|current| {
            current.counted_above() == above && current.counted_through() == through
        }) {
            return None;
        }

        let mut counts = counts;
        for (height, block) in self.blocks.range(above + 1..=through) {
            let already_counted = self
                .scan_counters
                .is_some_and(|current| current.covers(*height));
            if !already_counted {
                counts = counts + ScanCounts::of_block(block);
            }
        }
        ScanCounters::from_parts(above, through, counts)
    }
}

#[cfg(test)]
mod tests {
    use zcash_client_backend::data_api::scanning::ScanPriority;

    use super::*;

    fn retention() -> BlockRetention {
        BlockRetention {
            max_scanned: BlockHeight::from(1_000),
            rewind_targets: BTreeSet::new(),
            fully_scanned: None,
            birthday: BlockHeight::from(100),
            tx_heights: BTreeSet::new(),
            scan_boundaries: BTreeSet::new(),
        }
    }

    #[test]
    fn the_rewind_window_keeps_pruning_depth_blocks_below_the_max_scanned_height() {
        let retention = retention();
        let window_start = BlockHeight::from(1_000 - PRUNING_DEPTH);
        assert!(!retention.retains(window_start - 1));
        assert!(retention.retains(window_start));
        assert!(retention.retains(BlockHeight::from(1_000)));
        assert!(retention.retains(BlockHeight::from(1_001)));
    }

    #[test]
    fn rewind_targets_and_the_fully_scanned_height_are_kept() {
        let mut retention = retention();
        retention.rewind_targets.insert(BlockHeight::from(400));
        retention.fully_scanned = Some(BlockHeight::from(500));
        assert!(retention.retains(BlockHeight::from(400)));
        assert!(retention.retains(BlockHeight::from(500)));
        assert!(!retention.retains(BlockHeight::from(401)));
        assert!(!retention.retains(BlockHeight::from(499)));
    }

    #[test]
    fn transaction_heights_are_kept() {
        let mut retention = retention();
        retention.tx_heights.insert(BlockHeight::from(250));
        assert!(retention.retains(BlockHeight::from(250)));
        assert!(!retention.retains(BlockHeight::from(249)));
        assert!(!retention.retains(BlockHeight::from(251)));
    }

    #[test]
    fn scan_range_boundaries_are_kept() {
        let mut retention = retention();
        retention.scan_boundaries.insert(BlockHeight::from(300));
        assert!(retention.retains(BlockHeight::from(300)));
        assert!(!retention.retains(BlockHeight::from(301)));
    }

    #[test]
    fn the_birthday_block_and_everything_below_it_are_kept() {
        let retention = retention();
        assert!(retention.retains(BlockHeight::from(100)));
        assert!(retention.retains(BlockHeight::from(99)));
        assert!(!retention.retains(BlockHeight::from(101)));
    }

    #[test]
    fn scan_boundaries_come_from_unscanned_range_starts_and_scanned_range_ends() {
        let queue = ScanQueue(vec![
            (
                BlockHeight::from(0),
                BlockHeight::from(100),
                ScanPriority::Ignored,
            ),
            (
                BlockHeight::from(100),
                BlockHeight::from(300),
                ScanPriority::Scanned,
            ),
            (
                BlockHeight::from(300),
                BlockHeight::from(400),
                ScanPriority::Historic,
            ),
            (
                BlockHeight::from(400),
                BlockHeight::from(450),
                ScanPriority::Scanned,
            ),
            (
                BlockHeight::from(450),
                BlockHeight::from(500),
                ScanPriority::ChainTip,
            ),
            (
                BlockHeight::from(500),
                BlockHeight::from(600),
                ScanPriority::Verify,
            ),
        ]);
        // 299 is both the end of the first scanned range and the block before the historic
        // range; 399, the block before the second scanned range, is unscanned and not a
        // boundary; 449 ends the second scanned range; 499 precedes the `Verify` range.
        assert_eq!(
            scan_boundaries(&queue),
            [299u32, 449, 499]
                .into_iter()
                .map(BlockHeight::from)
                .collect()
        );
    }

    #[test]
    fn an_unscanned_range_starting_at_zero_has_no_boundary_below_it() {
        let queue = ScanQueue(vec![(
            BlockHeight::from(0),
            BlockHeight::from(10),
            ScanPriority::Historic,
        )]);
        assert!(scan_boundaries(&queue).is_empty());
    }
}
