//! Ironwood (NU6.3) pool tests for the in-memory store.
//!
//! The backend's `ShieldedPoolTester` harness has no Ironwood tester, so these cases drive the
//! store directly with the harness's [`IronwoodFvk`], which places version 3 notes in a block's
//! Ironwood actions, and with the Orchard tester's keys, which are the keys Ironwood notes are
//! decrypted and spent with.

use std::convert::Infallible;

use shardtree::{
    ShardTree,
    store::{ShardStore, memory::MemoryShardStore},
};
use zcash_client_backend::{
    data_api::{
        Account, NullifierQuery, WalletRead, WalletWrite,
        testing::{
            AddressType, IronwoodFvk, TestBuilder, orchard::OrchardPoolTester,
            pool::ShieldedPoolTester,
        },
        wallet::{ConfirmationsPolicy, input_selection::GreedyInputSelector},
    },
    fees::{DustOutputPolicy, StandardFeeRule, standard},
    wallet::OvkPolicy,
};
use zcash_primitives::block::BlockHash;
use zcash_protocol::{
    PoolType, ShieldedPool, consensus::BlockHeight, local_consensus::LocalNetwork, value::Zatoshis,
};
use zip321::{Payment, TransactionRequest};

use crate::{
    MemoryWalletDb,
    testing::{MemBlockCache, TestMemDbFactory},
};

/// A network on which Ironwood (NU6.3) is active from the Sapling activation height, so received
/// Ironwood notes are offered by input selection (which gates on NU6.3 activation).
fn ironwood_active_network() -> LocalNetwork {
    let activation = BlockHeight::from_u32(100_000);
    LocalNetwork {
        nu6: Some(activation),
        nu6_1: Some(activation),
        nu6_2: Some(activation),
        nu6_3: Some(activation),
        ..TestBuilder::<(), ()>::DEFAULT_NETWORK
    }
}

fn ironwood_checkpoint_heights(wallet: &MemoryWalletDb<LocalNetwork>) -> Vec<BlockHeight> {
    let store = wallet.ironwood_tree.store();
    let mut heights = vec![];
    store
        .for_each_checkpoint(store.checkpoint_count().unwrap(), |height, _| {
            heights.push(*height);
            Ok(())
        })
        .unwrap();
    heights
}

/// Scanning a block containing an Ironwood (version 3) note stores it as an Ironwood-pool
/// received note, counts it in the balance, and commits it to the Ironwood tree rather than the
/// Orchard tree.
#[test]
fn scan_block_stores_received_ironwood_note() {
    let mut st = TestBuilder::new()
        .with_network(ironwood_active_network())
        .with_data_store_factory(TestMemDbFactory::new())
        .with_block_cache(MemBlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();

    let account_id = st.test_account().unwrap().id();
    // The account's Ironwood outputs are trial-decrypted with its Orchard viewing key.
    let recipient = IronwoodFvk(OrchardPoolTester::test_account_fvk(&st));

    let value = Zatoshis::const_from_u64(60_000);
    let (h, _, _) = st.generate_next_block(&recipient, AddressType::DefaultExternal, value);
    st.scan_cached_blocks(h, 1);

    let pools: Vec<PoolType> = st
        .wallet()
        .received_notes
        .iter()
        .map(|n| n.pool())
        .collect();
    assert_eq!(pools, vec![PoolType::IRONWOOD]);
    assert_eq!(st.get_total_balance(account_id), value);

    assert_eq!(
        st.wallet()
            .get_ironwood_nullifiers(NullifierQuery::Unspent)
            .unwrap()
            .len(),
        1
    );
    assert!(
        st.wallet()
            .get_orchard_nullifiers(NullifierQuery::All)
            .unwrap()
            .is_empty(),
        "the note must not be recorded as an Orchard note"
    );

    // The commitment went into the Ironwood tree; the Orchard tree received no commitments.
    assert!(
        st.wallet()
            .ironwood_tree
            .store()
            .last_shard()
            .unwrap()
            .is_some()
    );
    assert!(
        st.wallet()
            .orchard_tree
            .store()
            .last_shard()
            .unwrap()
            .is_none()
    );
}

/// A received Ironwood note can be selected and spent. The spend is recorded when the transaction
/// is created, and again when the mined transaction is scanned, after which the note's nullifier
/// no longer counts as unspent and the change is back in the wallet as an Ironwood note.
#[test]
fn spend_received_ironwood_note() {
    let mut st = TestBuilder::new()
        .with_network(ironwood_active_network())
        .with_data_store_factory(TestMemDbFactory::new())
        .with_block_cache(MemBlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();

    let account = st.test_account().cloned().unwrap();
    let account_id = account.id();

    // Receive an Ironwood note, comfortably larger than the payment and fee.
    let received = IronwoodFvk(OrchardPoolTester::test_account_fvk(&st));
    let note_value = Zatoshis::const_from_u64(100_000);
    let (h, _, _) = st.generate_next_block(&received, AddressType::DefaultExternal, note_value);
    st.scan_cached_blocks(h, 1);
    assert_eq!(st.get_total_balance(account_id), note_value);
    let note_id = st.wallet().received_notes.iter().next().unwrap().note_id();
    assert_eq!(note_id.protocol(), ShieldedPool::Ironwood);

    // Advance the chain so the note's shard is fully scanned and an anchor is available.
    for _ in 0..5 {
        let (h, _) = st.generate_empty_block();
        st.scan_cached_blocks(h, 1);
    }

    // Propose and create a transfer that must spend the Ironwood note.
    let to_sk = OrchardPoolTester::sk(&[0xf5; 32]);
    let to = OrchardPoolTester::sk_default_address(&to_sk);
    let payment_value = Zatoshis::const_from_u64(10_000);
    let request = TransactionRequest::new(vec![Payment::without_memo(
        to.to_zcash_address(st.network()),
        payment_value,
    )])
    .unwrap();

    let change_strategy = standard::SingleOutputChangeStrategy::new(
        StandardFeeRule::Zip317,
        None,
        ShieldedPool::Orchard,
        DustOutputPolicy::default(),
    );
    let input_selector = GreedyInputSelector::new();

    let proposal = st
        .propose_transfer(
            account_id,
            &input_selector,
            &change_strategy,
            request,
            ConfirmationsPolicy::MIN,
        )
        .unwrap();

    // Change from spending Ironwood notes stays in the Ironwood pool, and the Orchard-receiver
    // payment is represented as an Ironwood output.
    let step = proposal.steps().last();
    assert!(
        step.balance()
            .proposed_change()
            .iter()
            .all(|c| c.output_pool() == PoolType::IRONWOOD),
        "change from an Ironwood spend must stay in the Ironwood pool"
    );
    assert!(
        step.payment_pools()
            .values()
            .all(|p| *p == PoolType::IRONWOOD),
        "the Orchard-receiver payment must be represented as an Ironwood output"
    );
    let fee = step.balance().fee_required();
    assert!(fee.into_u64() > 0, "the transaction must pay a fee");

    let created = st
        .create_proposed_transactions::<Infallible, _, Infallible, _>(
            account.usk(),
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap();
    assert_eq!(created.len(), 1);
    let sent_txid = created[0];

    // The created transaction spends into the Ironwood bundle, and storing it marked the
    // received note as spent by it.
    let tx = st
        .wallet()
        .get_transaction(sent_txid)
        .unwrap()
        .expect("The sent transaction was stored.");
    assert!(
        tx.ironwood_bundle().is_some(),
        "the transaction must contain an Ironwood bundle"
    );
    assert_eq!(
        st.wallet().received_note_spends.get(&note_id),
        Some(&sent_txid)
    );

    // Mine the transaction and scan it: the spend is detected from the chain, the spent note's
    // nullifier is no longer unspent, and the change note has been received in the Ironwood pool.
    let (mined_height, _) = st.generate_next_block_including(sent_txid);
    st.scan_cached_blocks(mined_height, 1);

    assert!(
        st.wallet()
            .get_ironwood_nullifiers(NullifierQuery::Unspent)
            .unwrap()
            .iter()
            .all(|(_, nf)| {
                st.wallet()
                    .received_notes
                    .iter()
                    .find(|n| n.nullifier() == Some(&crate::Nullifier::Ironwood(*nf)))
                    .map(|n| n.note_id())
                    != Some(note_id)
            }),
        "the spent note's nullifier must not be reported as unspent"
    );
    let change_pools: Vec<PoolType> = st
        .wallet()
        .received_notes
        .iter()
        .filter(|n| n.note_id() != note_id)
        .map(|n| n.pool())
        .collect();
    assert_eq!(change_pools, vec![PoolType::IRONWOOD]);
    assert_eq!(
        st.get_total_balance(account_id),
        (note_value - payment_value - fee).unwrap()
    );
}

/// `truncate_to_height` must roll the Ironwood tree back together with the Sapling and Orchard
/// trees. If it does not, checkpoints (and commitments) for the rolled-back blocks remain, and a
/// later re-scan appends onto a stale frontier, corrupting the Ironwood anchors.
#[test]
fn truncate_rolls_back_the_ironwood_tree() {
    let mut st = TestBuilder::new()
        .with_network(ironwood_active_network())
        .with_data_store_factory(TestMemDbFactory::new())
        .with_block_cache(MemBlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();

    // Receive Ironwood notes in three consecutive blocks and scan them, so the Ironwood tree
    // gains a checkpoint at each height.
    let fvk = IronwoodFvk(OrchardPoolTester::test_account_fvk(&st));
    let value = Zatoshis::const_from_u64(100_000);
    let (h0, _, _) = st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
    st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
    st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
    st.scan_cached_blocks(h0, 3);
    // `put_blocks` also checkpoints the chain state the batch starts from, at h0 - 1.
    assert_eq!(
        ironwood_checkpoint_heights(st.wallet()),
        vec![h0 - 1, h0, h0 + 1, h0 + 2]
    );

    // A reorg truncates the wallet back to h0; the Ironwood tree must roll back with the Sapling
    // and Orchard trees.
    assert_eq!(st.wallet_mut().truncate_to_height(h0).unwrap(), h0);
    assert_eq!(
        ironwood_checkpoint_heights(st.wallet()),
        vec![h0 - 1, h0],
        "truncation must remove Ironwood tree checkpoints above the truncation height"
    );
}

/// A wallet saved before Ironwood support and restored has an Ironwood tree with no checkpoints
/// until it next scans a block. Such a tree has nothing to roll back and must not make rewinds
/// fail, as they would otherwise do for every existing wallet on its first sync after upgrading
/// whenever that sync starts with a reorg.
#[test]
fn truncate_succeeds_when_ironwood_tree_has_no_checkpoints() {
    let mut st = TestBuilder::new()
        .with_network(ironwood_active_network())
        .with_data_store_factory(TestMemDbFactory::new())
        .with_block_cache(MemBlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();

    // Scan three blocks of Orchard notes so the Sapling and Orchard trees have checkpoints.
    let fvk = OrchardPoolTester::test_account_fvk(&st);
    let value = Zatoshis::const_from_u64(100_000);
    let (h0, _, _) = st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
    st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
    st.generate_next_block(&fvk, AddressType::DefaultExternal, value);
    st.scan_cached_blocks(h0, 3);

    // Put the wallet in the state a snapshot taken before Ironwood support restores to: the
    // Ironwood tree is the empty tree `MemoryWalletDb::new` creates, with no checkpoints.
    st.wallet_mut().ironwood_tree = ShardTree::new(MemoryShardStore::empty(), 100);
    st.wallet_mut().ironwood_tree_shard_end_heights.clear();
    assert!(ironwood_checkpoint_heights(st.wallet()).is_empty());

    // A rewind to a height the other trees are checkpointed at succeeds.
    assert_eq!(st.wallet_mut().truncate_to_height(h0).unwrap(), h0);

    // Rescanning the rolled-back blocks resumes normally and checkpoints the Ironwood tree at the
    // chain state the batch starts from and at every scanned block.
    st.scan_cached_blocks(h0, 3);
    assert_eq!(
        ironwood_checkpoint_heights(st.wallet()),
        vec![h0 - 1, h0, h0 + 1, h0 + 2]
    );
}
