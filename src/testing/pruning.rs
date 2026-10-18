//! Tests for pruning of the nullifier map during block scanning.
//!
//! These drive real (fake-compact-block) scans through `WalletWrite::put_blocks` so that the
//! pruning cutoff is derived from the wallet's actual scan queue, as it is in production.
//!
//! The scanner only records *unlinked* nullifiers: spends it could not match to a note the wallet
//! already knows. A spend scanned linearly after its note is linked on the spot and never enters
//! the map, so every test here scans the spending block *before* the receiving block.

use zcash_client_backend::data_api::{
    Account as _, WalletRead,
    scanning::{ScanPriority, ScanRange},
    testing::{
        AddressType, TestBuilder, TestState, pool::ShieldedPoolTester, sapling::SaplingPoolTester,
    },
};
use zcash_primitives::block::BlockHash;
use zcash_protocol::{
    TxId,
    consensus::{BlockHeight, TxIndex},
    local_consensus::LocalNetwork,
    value::Zatoshis,
};

use crate::{
    MemoryWalletDb, Nullifier, PRUNING_DEPTH,
    testing::{MemBlockCache, TestMemDbFactory},
};

type St = TestState<MemBlockCache, MemoryWalletDb<LocalNetwork>, LocalNetwork>;

const RECEIVED: u64 = 50_000;
const SENT_AWAY: u64 = 20_000;
const PER_BLOCK: u64 = 1_000;

/// Builds a wallet with one account whose birthday is Sapling activation, generates a block
/// receiving a note and a block spending it to a foreign address, then scans the *spending*
/// block first so that its nullifier is tracked as unlinked.
///
/// Returns the test state, the receiving height, the spending height and the spent nullifier.
fn setup_spend_scanned_before_receive() -> (St, BlockHeight, BlockHeight, Nullifier) {
    let mut st = TestBuilder::new()
        .with_data_store_factory(TestMemDbFactory::new())
        .with_block_cache(MemBlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();

    let dfvk = st.test_account_sapling().unwrap().clone();
    let value = Zatoshis::const_from_u64(RECEIVED);
    let (received_height, _, nf) =
        st.generate_next_block(&dfvk, AddressType::DefaultExternal, value);

    let not_our_key = SaplingPoolTester::sk_to_fvk(&SaplingPoolTester::sk(&[0xf5; 32]));
    let (spent_height, _) = st.generate_next_block_spending(
        &dfvk,
        (nf, value),
        SaplingPoolTester::fvk_default_address(&not_our_key),
        Zatoshis::const_from_u64(SENT_AWAY),
    );

    st.scan_cached_blocks(spent_height, 1);
    let nf = Nullifier::Sapling(nf);
    assert_eq!(
        st.wallet().nullifiers.get(&nf),
        Some(&(spent_height, 0)),
        "an unlinked spend must be tracked"
    );
    assert_eq!(fully_scanned_height(&st), None, "gap below the spend block");

    (st, received_height, spent_height, nf)
}

/// Generates `count` further blocks, each paying the test account, scans them, and returns the
/// new tip.
fn extend_and_scan(st: &mut St, count: u32) -> BlockHeight {
    let dfvk = st.test_account_sapling().unwrap().clone();
    let mut heights = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (h, _, _) = st.generate_next_block(
            &dfvk,
            AddressType::DefaultExternal,
            Zatoshis::const_from_u64(PER_BLOCK),
        );
        heights.push(h);
    }
    let first = *heights.first().expect("count > 0");
    st.scan_cached_blocks(first, count as usize);
    *heights.last().expect("count > 0")
}

fn min_locator_height(st: &St) -> Option<BlockHeight> {
    st.wallet().tx_locator.0.keys().map(|(h, _)| *h).min()
}

fn fully_scanned_height(st: &St) -> Option<BlockHeight> {
    st.wallet()
        .block_fully_scanned()
        .unwrap()
        .map(|m| m.block_height())
}

fn balance(st: &St) -> Zatoshis {
    st.get_total_balance(st.test_account().unwrap().id())
}

#[test]
fn out_of_order_spend_is_matched_through_the_nullifier_map() {
    let (mut st, received_height, spent_height, _nf) = setup_spend_scanned_before_receive();

    // Scanning the receiving block afterwards must find the note already spent.
    st.scan_cached_blocks(received_height, 1);
    assert_eq!(fully_scanned_height(&st), Some(spent_height));
    assert_eq!(
        balance(&st),
        Zatoshis::const_from_u64(RECEIVED - SENT_AWAY),
        "only the change should be spendable"
    );
}

#[test]
fn spend_entries_survive_within_pruning_depth_and_are_dropped_beyond_it() {
    let (mut st, received_height, spent_height, nf) = setup_spend_scanned_before_receive();
    st.scan_cached_blocks(received_height, 1);
    assert_eq!(min_locator_height(&st), Some(received_height));

    // Extend the chain so that `fully_scanned - PRUNING_DEPTH == spent_height` exactly.
    // Entries *at* the cutoff must be kept, entries below it dropped.
    let tip = extend_and_scan(&mut st, PRUNING_DEPTH);
    assert_eq!(fully_scanned_height(&st), Some(tip));
    assert_eq!(tip - PRUNING_DEPTH, spent_height);
    st.wallet_mut()
        .prune_tracked_nullifiers(PRUNING_DEPTH)
        .unwrap();
    assert_eq!(
        st.wallet().nullifiers.get(&nf),
        Some(&(spent_height, 0)),
        "entry at the cutoff height must be retained"
    );
    assert_eq!(
        min_locator_height(&st),
        Some(spent_height),
        "locators below the cutoff must be pruned"
    );

    // One more block moves the cutoff past the spend, so the entry must be dropped.
    let tip = extend_and_scan(&mut st, 1);
    assert_eq!(fully_scanned_height(&st), Some(tip));
    st.wallet_mut()
        .prune_tracked_nullifiers(PRUNING_DEPTH)
        .unwrap();
    assert!(
        st.wallet().nullifiers.get(&nf).is_none(),
        "entry below the cutoff must be pruned"
    );
    assert_eq!(min_locator_height(&st), Some(tip - PRUNING_DEPTH));

    // Pruning the lookup map must not forget that the note was spent.
    let expected = RECEIVED - SENT_AWAY + PER_BLOCK * u64::from(PRUNING_DEPTH + 1);
    assert_eq!(balance(&st), Zatoshis::const_from_u64(expected));
}

#[test]
fn put_blocks_prunes_tracked_nullifiers() {
    // `put_blocks` prunes using the fully scanned height as recorded when the call starts (the
    // scan queue is only advanced afterwards), so an entry ages out no later than the batch
    // following the one that pushed it below the cutoff.
    let (mut st, received_height, _spent_height, nf) = setup_spend_scanned_before_receive();
    st.scan_cached_blocks(received_height, 1);

    extend_and_scan(&mut st, PRUNING_DEPTH + 1);
    extend_and_scan(&mut st, 1);

    assert!(
        st.wallet().nullifiers.get(&nf).is_none(),
        "scanning alone must prune aged-out entries"
    );
    let tip = fully_scanned_height(&st).unwrap();
    assert!(min_locator_height(&st).unwrap() >= tip - PRUNING_DEPTH - 1);
}

#[test]
fn pruning_pauses_while_a_range_below_the_scanned_tip_awaits_rescan() {
    let (mut st, received_height, _spent_height, nf) = setup_spend_scanned_before_receive();
    let activation = st.sapling_activation_height();
    st.scan_cached_blocks(received_height, 1);
    extend_and_scan(&mut st, PRUNING_DEPTH + 5);
    let tip = extend_and_scan(&mut st, 1);
    assert!(st.wallet().nullifiers.get(&nf).is_none());

    // Simulate adding an account with an earlier birthday: the wallet reopens the range from
    // the birthday for a forced rescan, so there is no longer a fully scanned height.
    let rescan_range = activation..(tip + 1);
    st.wallet_mut()
        .scan_queue
        .replace_queue_entries(
            &rescan_range,
            Some(ScanRange::from_parts(
                rescan_range.clone(),
                ScanPriority::Historic,
            ))
            .into_iter(),
            true,
        )
        .unwrap();
    assert_eq!(fully_scanned_height(&st), None);

    // Entries recorded while the gap is open must not be pruned, however low they are.
    let low_nf = sapling::Nullifier([0xAB; 32]);
    st.wallet_mut()
        .insert_sapling_nullifier_map(
            activation,
            &[(
                TxIndex::from(7u16),
                TxId::from_bytes([0xAB; 32]),
                vec![low_nf],
            )],
        )
        .unwrap();
    st.wallet_mut()
        .prune_tracked_nullifiers(PRUNING_DEPTH)
        .unwrap();
    assert_eq!(
        st.wallet().nullifiers.get(&Nullifier::Sapling(low_nf)),
        Some(&(activation, 7))
    );
}
