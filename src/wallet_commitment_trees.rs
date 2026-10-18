use std::convert::Infallible;

use incrementalmerkletree::Address;
use shardtree::{ShardTree, error::ShardTreeError, store::memory::MemoryShardStore};
#[cfg(feature = "orchard")]
use zcash_client_backend::data_api::ORCHARD_SHARD_HEIGHT;
use zcash_client_backend::data_api::{
    SAPLING_SHARD_HEIGHT, WalletCommitmentTrees, chain::CommitmentTreeRoot,
};
use zcash_protocol::consensus::{self, BlockHeight};

use crate::MemoryWalletDb;

impl<P: consensus::Parameters> WalletCommitmentTrees for MemoryWalletDb<P> {
    type Error = Infallible;
    type SaplingShardStore<'a> = MemoryShardStore<sapling::Node, BlockHeight>;

    fn with_sapling_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<A, E>
    where
        for<'a> F: FnMut(
            &'a mut ShardTree<
                Self::SaplingShardStore<'a>,
                { sapling::NOTE_COMMITMENT_TREE_DEPTH },
                SAPLING_SHARD_HEIGHT,
            >,
        ) -> Result<A, E>,
        E: From<ShardTreeError<Infallible>>,
    {
        tracing::debug!("with_sapling_tree_mut");
        callback(&mut self.sapling_tree)
    }

    fn put_sapling_subtree_roots(
        &mut self,
        start_index: u64,
        roots: &[CommitmentTreeRoot<sapling::Node>],
    ) -> Result<(), ShardTreeError<Self::Error>> {
        tracing::debug!("put_sapling_subtree_roots");
        self.with_sapling_tree_mut(|t| {
            for (root, i) in roots.iter().zip(0u64..) {
                let root_addr = Address::from_parts(SAPLING_SHARD_HEIGHT.into(), start_index + i);
                t.insert(root_addr, *root.root_hash())?;
            }
            Ok::<_, ShardTreeError<Self::Error>>(())
        })?;

        // store the end block heights for each shard as well
        for (root, i) in roots.iter().zip(0u64..) {
            let root_addr = Address::from_parts(SAPLING_SHARD_HEIGHT.into(), start_index + i);
            self.sapling_tree_shard_end_heights
                .insert(root_addr, root.subtree_end_height());
        }

        Ok(())
    }

    #[cfg(feature = "orchard")]
    type OrchardShardStore<'a> = MemoryShardStore<orchard::tree::MerkleHashOrchard, BlockHeight>;

    #[cfg(feature = "orchard")]
    fn with_orchard_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<A, E>
    where
        for<'a> F: FnMut(
            &'a mut ShardTree<
                Self::OrchardShardStore<'a>,
                { ORCHARD_SHARD_HEIGHT * 2 },
                ORCHARD_SHARD_HEIGHT,
            >,
        ) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        tracing::debug!("with_orchard_tree_mut");
        callback(&mut self.orchard_tree)
    }

    /// Adds a sequence of note commitment tree subtree roots to the data store.
    #[cfg(feature = "orchard")]
    fn put_orchard_subtree_roots(
        &mut self,
        start_index: u64,
        roots: &[CommitmentTreeRoot<orchard::tree::MerkleHashOrchard>],
    ) -> Result<(), ShardTreeError<Self::Error>> {
        tracing::debug!("put_orchard_subtree_roots");
        self.with_orchard_tree_mut(|t| {
            for (root, i) in roots.iter().zip(0u64..) {
                let root_addr = Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), start_index + i);
                t.insert(root_addr, *root.root_hash())?;
            }
            Ok::<_, ShardTreeError<Self::Error>>(())
        })?;

        // store the end block heights for each shard as well
        for (root, i) in roots.iter().zip(0u64..) {
            let root_addr = Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), start_index + i);
            self.orchard_tree_shard_end_heights
                .insert(root_addr, root.subtree_end_height());
        }

        Ok(())
    }

    /// The Ironwood (NU6.3) note commitment tree. It is Orchard-shaped, so it shares the
    /// Orchard shard store type and shard height, but it is a separate tree: Ironwood notes
    /// are committed to it, and Ironwood spends are proven against its anchors.
    #[cfg(feature = "orchard")]
    fn with_ironwood_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<Option<A>, E>
    where
        for<'a> F: FnMut(
            &'a mut ShardTree<
                Self::OrchardShardStore<'a>,
                { ORCHARD_SHARD_HEIGHT * 2 },
                ORCHARD_SHARD_HEIGHT,
            >,
        ) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        tracing::debug!("with_ironwood_tree_mut");
        callback(&mut self.ironwood_tree).map(Some)
    }

    /// Adds a sequence of Ironwood note commitment tree subtree roots to the data store.
    #[cfg(feature = "orchard")]
    fn put_ironwood_subtree_roots(
        &mut self,
        start_index: u64,
        roots: &[CommitmentTreeRoot<orchard::tree::MerkleHashOrchard>],
    ) -> Result<(), ShardTreeError<Self::Error>> {
        tracing::debug!("put_ironwood_subtree_roots");
        self.with_ironwood_tree_mut(|t| {
            for (root, i) in roots.iter().zip(0u64..) {
                let root_addr = Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), start_index + i);
                t.insert(root_addr, *root.root_hash())?;
            }
            Ok::<_, ShardTreeError<Self::Error>>(())
        })?;

        // store the end block heights for each shard as well
        for (root, i) in roots.iter().zip(0u64..) {
            let root_addr = Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), start_index + i);
            self.ironwood_tree_shard_end_heights
                .insert(root_addr, root.subtree_end_height());
        }

        Ok(())
    }
}

pub(crate) mod serialization {
    use std::io::Cursor;

    use incrementalmerkletree::{Address, Level, Position};
    use shardtree::{
        LocatedPrunableTree, ShardTree,
        store::{Checkpoint, ShardStore, TreeState, memory::MemoryShardStore},
    };
    use zcash_client_backend::serialization::shardtree::{read_shard, write_shard};
    use zcash_protocol::consensus::BlockHeight;

    use crate::{Error, proto::memwallet as proto};

    pub(crate) fn tree_to_protobuf<
        H: Clone
            + incrementalmerkletree::Hashable
            + PartialEq
            + zcash_primitives::merkle_tree::HashSer,
        const DEPTH: u8,
        const SHARD_HEIGHT: u8,
    >(
        tree: &ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD_HEIGHT>,
    ) -> Result<Option<crate::proto::memwallet::ShardTree>, Error> {
        use crate::proto::memwallet::{ShardTree, TreeCheckpoint, TreeShard};

        let mut cap_bytes = Vec::new();
        write_shard(&mut cap_bytes, &tree.store().get_cap()?)?;

        let shards = tree
            .store()
            .get_shard_roots()?
            .iter()
            .map(|shard_root| {
                let shard = tree.store().get_shard(*shard_root)?.ok_or_else(|| {
                    Error::Other(format!(
                        "Missing shard for root index {}",
                        shard_root.index()
                    ))
                })?;

                let mut shard_data = Vec::new();
                write_shard(&mut shard_data, shard.root())?;

                Ok(TreeShard {
                    shard_index: shard_root.index(),
                    shard_data,
                })
            })
            .collect::<Result<Vec<TreeShard>, Error>>()?;

        let mut checkpoints = Vec::new();
        tree.store()
            .for_each_checkpoint(usize::MAX, |id, checkpoint| {
                let (tree_empty, position) = match checkpoint.tree_state() {
                    TreeState::Empty => (true, 0),
                    TreeState::AtPosition(position) => (false, position.into()),
                };
                checkpoints.push(TreeCheckpoint {
                    checkpoint_id: (*id).into(),
                    position,
                    tree_empty,
                    marks_removed: checkpoint
                        .marks_removed()
                        .iter()
                        .map(|position| u64::from(*position))
                        .collect(),
                });
                Ok(())
            })?;

        Ok(Some(ShardTree {
            cap: cap_bytes,
            shards,
            checkpoints,
        }))
    }

    pub(crate) fn tree_from_protobuf<
        H: Clone
            + incrementalmerkletree::Hashable
            + PartialEq
            + zcash_primitives::merkle_tree::HashSer,
        const DEPTH: u8,
        const SHARD_HEIGHT: u8,
    >(
        proto_tree: proto::ShardTree,
        max_checkpoints: usize,
        shard_root_level: Level,
    ) -> Result<ShardTree<MemoryShardStore<H, BlockHeight>, DEPTH, SHARD_HEIGHT>, Error> {
        let mut tree = ShardTree::new(MemoryShardStore::empty(), max_checkpoints);

        let cap = read_shard(Cursor::new(&proto_tree.cap))?;
        tree.store_mut().put_cap(cap)?;

        for proto_shard in proto_tree.shards {
            let shard_root = Address::from_parts(shard_root_level, proto_shard.shard_index);
            let shard_tree = read_shard(&mut Cursor::new(proto_shard.shard_data))?;
            let shard = LocatedPrunableTree::from_parts(shard_root, shard_tree)
                .map_err(|_| Error::Other("shard persistence invalid".to_string()))?;
            tree.store_mut().put_shard(shard)?;
        }

        for proto_checkpoint in proto_tree.checkpoints {
            // A checkpoint of the empty tree used to be written as position 0 and read back as
            // a checkpoint at position 0, which is a different (one-leaf) tree state; the
            // `tree_empty` flag keeps the two apart. Blobs without the flag decode as before.
            let tree_state = if proto_checkpoint.tree_empty {
                TreeState::Empty
            } else {
                TreeState::AtPosition(proto_checkpoint.position.into())
            };
            let marks_removed = proto_checkpoint
                .marks_removed
                .into_iter()
                .map(Position::from)
                .collect();
            tree.store_mut().add_checkpoint(
                BlockHeight::from(proto_checkpoint.checkpoint_id),
                Checkpoint::from_parts(tree_state, marks_removed),
            )?;
        }

        Ok(tree)
    }

    #[cfg(test)]
    mod tests {
        use std::collections::BTreeSet;

        use incrementalmerkletree::{Marking, Retention};
        use zcash_client_backend::data_api::SAPLING_SHARD_HEIGHT;

        use super::*;

        type TestTree = ShardTree<
            MemoryShardStore<sapling::Node, BlockHeight>,
            { SAPLING_SHARD_HEIGHT * 2 },
            SAPLING_SHARD_HEIGHT,
        >;

        fn checkpoints(tree: &TestTree) -> Vec<(BlockHeight, TreeState, BTreeSet<Position>)> {
            let mut out = vec![];
            tree.store()
                .for_each_checkpoint(usize::MAX, |id, checkpoint| {
                    out.push((
                        *id,
                        checkpoint.tree_state(),
                        checkpoint.marks_removed().clone(),
                    ));
                    Ok(())
                })
                .unwrap();
            out
        }

        fn roundtrip(tree: &TestTree) -> TestTree {
            let proto_tree = tree_to_protobuf(tree).unwrap().unwrap();
            tree_from_protobuf(proto_tree, 100, SAPLING_SHARD_HEIGHT.into()).unwrap()
        }

        #[test]
        fn empty_tree_checkpoint_roundtrips_as_empty() {
            let mut tree = TestTree::new(MemoryShardStore::empty(), 100);
            tree.checkpoint(BlockHeight::from(10)).unwrap();
            assert_eq!(
                checkpoints(&tree),
                vec![(BlockHeight::from(10), TreeState::Empty, BTreeSet::new())]
            );

            let recovered = roundtrip(&tree);
            assert_eq!(checkpoints(&recovered), checkpoints(&tree));
        }

        #[test]
        fn checkpoint_with_removed_marks_roundtrips() {
            let mut tree = TestTree::new(MemoryShardStore::empty(), 100);
            let leaf = |b: u8| sapling::Node::from_bytes([b; 32]).unwrap();
            tree.append(leaf(1), Retention::Marked).unwrap();
            tree.append(
                leaf(2),
                Retention::Checkpoint {
                    id: BlockHeight::from(20),
                    marking: Marking::Marked,
                },
            )
            .unwrap();
            tree.append(
                leaf(3),
                Retention::Checkpoint {
                    id: BlockHeight::from(21),
                    marking: Marking::None,
                },
            )
            .unwrap();
            // Removing a mark as of a checkpoint records it in that checkpoint rather than
            // dropping it from the tree, so the removal must survive persistence.
            assert!(
                tree.remove_mark(Position::from(0), Some(&BlockHeight::from(21)))
                    .unwrap()
            );

            let expected = checkpoints(&tree);
            assert_eq!(
                expected,
                vec![
                    (
                        BlockHeight::from(20),
                        TreeState::AtPosition(Position::from(1)),
                        BTreeSet::new()
                    ),
                    (
                        BlockHeight::from(21),
                        TreeState::AtPosition(Position::from(2)),
                        BTreeSet::from([Position::from(0)])
                    ),
                ]
            );

            let recovered = roundtrip(&tree);
            assert_eq!(checkpoints(&recovered), expected);
            assert_eq!(
                recovered
                    .root_at_checkpoint_id(&BlockHeight::from(21))
                    .unwrap(),
                tree.root_at_checkpoint_id(&BlockHeight::from(21)).unwrap()
            );
        }

        #[test]
        fn checkpoints_without_the_empty_flag_decode_at_their_position() {
            // A blob written before `tree_empty` existed.
            let proto_tree = proto::ShardTree {
                cap: {
                    let mut bytes = vec![];
                    write_shard(
                        &mut bytes,
                        &shardtree::PrunableTree::<sapling::Node>::empty(),
                    )
                    .unwrap();
                    bytes
                },
                shards: vec![],
                checkpoints: vec![proto::TreeCheckpoint {
                    checkpoint_id: 5,
                    position: 0,
                    tree_empty: false,
                    marks_removed: vec![],
                }],
            };
            let tree: TestTree =
                tree_from_protobuf(proto_tree, 100, SAPLING_SHARD_HEIGHT.into()).unwrap();
            assert_eq!(
                checkpoints(&tree),
                vec![(
                    BlockHeight::from(5),
                    TreeState::AtPosition(Position::from(0)),
                    BTreeSet::new()
                )]
            );
        }
    }
}
