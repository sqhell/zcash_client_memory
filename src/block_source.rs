use async_trait::async_trait;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use wasm_sync::RwLock;
use zcash_client_backend::data_api::chain::{BlockCache, BlockSource};
use zcash_client_backend::data_api::scanning::ScanRange;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_protocol::consensus::BlockHeight;

/// A block cache that just holds blocks in a map in memory
#[derive(Default)]
pub struct MemBlockCache(pub(crate) RwLock<BTreeMap<BlockHeight, CompactBlock>>);

impl MemBlockCache {
    /// Constructs a new empty [`MemBlockCache`].
    pub fn new() -> Self {
        Default::default()
    }

    /// Returns the [`CompactBlock`] at the given height, if it exists in the cache.
    pub fn find_block(&self, block_height: BlockHeight) -> Option<CompactBlock> {
        self.0.read().unwrap().get(&block_height).cloned()
    }
}

impl BlockSource for MemBlockCache {
    type Error = Infallible;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<zcash_protocol::consensus::BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), zcash_client_backend::data_api::chain::error::Error<WalletErrT, Self::Error>>
    where
        F: FnMut(
            CompactBlock,
        ) -> Result<
            (),
            zcash_client_backend::data_api::chain::error::Error<WalletErrT, Self::Error>,
        >,
    {
        let inner = self.0.read().unwrap();
        let block_iter = inner
            .iter()
            .filter(|(_, cb)| {
                if let Some(from_height) = from_height {
                    cb.height() >= from_height
                } else {
                    true
                }
            })
            .take(limit.unwrap_or(usize::MAX));

        for (_, cb) in block_iter {
            with_block(cb.clone())?;
        }
        Ok(())
    }
}

#[async_trait]
impl BlockCache for MemBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let inner = self.0.read().unwrap();
        if let Some(range) = range {
            let range = range.block_range();
            for h in (u32::from(range.start)..u32::from(range.end)).rev() {
                if let Some(cb) = inner.get(&h.into()) {
                    return Ok(Some(cb.height()));
                }
            }
        } else {
            return Ok(inner.last_key_value().map(|(h, _)| *h));
        }

        Ok(None)
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let inner = self.0.read().unwrap();
        let mut ret = Vec::with_capacity(range.len());
        let range = range.block_range();
        for height in u32::from(range.start)..u32::from(range.end) {
            if let Some(cb) = inner.get(&height.into()) {
                ret.push(cb.clone());
            }
        }

        Ok(ret)
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let mut inner = self.0.write().unwrap();
        compact_blocks.into_iter().for_each(|compact_block| {
            inner.insert(compact_block.height(), compact_block);
        });
        Ok(())
    }

    // Not an `async fn`: `zcash_client_backend::sync::run` collects the futures returned by
    // `delete` and only awaits them once the whole sync has finished. A lazy `async fn` body would
    // therefore keep every downloaded block in memory for the entire sync (gigabytes when syncing
    // from an old birthday), so the blocks are removed eagerly when `delete` is called.
    fn delete<'life0, 'async_trait>(
        &'life0 self,
        range: ScanRange,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let mut inner = self.0.write().unwrap();
        let range = range.block_range();
        for height in u32::from(range.start)..u32::from(range.end) {
            inner.remove(&height.into());
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_client_backend::data_api::scanning::ScanPriority;

    #[test]
    fn delete_removes_blocks_without_awaiting() {
        let cache = MemBlockCache::new();
        for h in 100u32..110 {
            let block = CompactBlock {
                height: h.into(),
                ..Default::default()
            };
            cache.0.write().unwrap().insert(h.into(), block);
        }
        assert!(cache.find_block(105.into()).is_some());

        // `sync::run` collects these futures and only awaits them after the whole sync,
        // so the removal must happen when `delete` is called, not when it is polled.
        let _unpolled = cache.delete(ScanRange::from_parts(
            BlockHeight::from(100)..BlockHeight::from(105),
            ScanPriority::Scanned,
        ));

        for h in 100u32..105 {
            assert!(
                cache.find_block(h.into()).is_none(),
                "block {h} still cached"
            );
        }
        assert!(cache.find_block(105.into()).is_some());
    }
}
