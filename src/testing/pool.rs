use zcash_client_backend::data_api::{
    NullifierQuery,
    testing::{AddressType, TestBuilder, TestFvk, pool::ShieldedPoolTester},
};
use zcash_primitives::block::BlockHash;
use zcash_protocol::{local_consensus::LocalNetwork, value::Zatoshis};

use crate::{
    MemoryWalletDb, PRUNING_DEPTH,
    testing::{MemBlockCache, TestMemDbFactory},
};

#[cfg(test)]
mod sapling;

#[cfg(test)]
#[cfg(feature = "orchard")]
mod orchard;

#[cfg(test)]
#[cfg(feature = "orchard")]
mod ironwood;

pub(crate) fn send_single_step_proposed_transfer<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::send_single_step_proposed_transfer::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn scan_full_block_detects_outputs<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::scan_full_block_detects_outputs::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "transparent-inputs")]
pub(crate) fn send_multi_step_proposed_transfer<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::send_multi_step_proposed_transfer::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
        |e, account_id, expected_bad_index| {
            matches!(
                e,
                crate::Error::ReachedGapLimit(acct, bad_index)
                if acct == &account_id && bad_index == &expected_bad_index)
        },
    )
}

#[cfg(feature = "transparent-inputs")]
pub(crate) fn proposal_fails_if_not_all_ephemeral_outputs_consumed<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::proposal_fails_if_not_all_ephemeral_outputs_consumed::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[allow(deprecated)]
pub(crate) fn create_to_address_fails_on_incorrect_usk<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::create_to_address_fails_on_incorrect_usk::<T, _>(
        TestMemDbFactory::new(),
    )
}

#[allow(deprecated)]
pub(crate) fn proposal_fails_with_no_blocks<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::proposal_fails_with_no_blocks::<T, _>(
        TestMemDbFactory::new(),
    )
}

pub(crate) fn spend_fails_on_unverified_notes<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::spend_fails_on_unverified_notes::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn spend_fails_on_locked_notes<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::spend_fails_on_locked_notes::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn ovk_policy_prevents_recovery_from_chain<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::ovk_policy_prevents_recovery_from_chain::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn spend_succeeds_to_t_addr_zero_change<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::spend_succeeds_to_t_addr_zero_change::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn change_note_spends_succeed<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::change_note_spends_succeed::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

// TODO: Implement reset for memdb
pub(crate) fn external_address_change_spends_detected_in_restore_from_seed<
    T: ShieldedPoolTester,
>() {
    zcash_client_backend::data_api::testing::pool::external_address_change_spends_detected_in_restore_from_seed::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[allow(dead_code)]
pub(crate) fn zip317_spend<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::zip317_spend::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

/// NU6.3: a payment to an Orchard receiver becomes a version 6 PCZT with an Ironwood bundle.
#[cfg(feature = "pczt-tests")]
pub(crate) fn create_pczt_supports_ironwood_output() {
    zcash_client_backend::data_api::testing::pool::create_pczt_supports_ironwood_output(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "orchard")]
pub(crate) fn propose_v5_payment_to_orchard_receiver_is_rejected() {
    zcash_client_backend::data_api::testing::pool::propose_v5_payment_to_orchard_receiver_is_rejected(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "orchard")]
pub(crate) fn proposal_records_and_serializes_proposed_version() {
    zcash_client_backend::data_api::testing::pool::proposal_records_and_serializes_proposed_version(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "transparent-inputs")]
pub(crate) fn shield_transparent<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::shield_transparent::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn birthday_in_anchor_shard<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::birthday_in_anchor_shard::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn checkpoint_gaps<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::checkpoint_gaps::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "orchard")]
pub(crate) fn pool_crossing_required<T: ShieldedPoolTester, TT: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::pool_crossing_required::<T, TT>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "orchard")]
pub(crate) fn fully_funded_fully_private<T: ShieldedPoolTester, TT: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::fully_funded_fully_private::<T, TT>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(all(feature = "orchard", feature = "transparent-inputs"))]
pub(crate) fn fully_funded_send_to_t<T: ShieldedPoolTester, TT: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::fully_funded_send_to_t::<T, TT>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "orchard")]
pub(crate) fn multi_pool_checkpoint<T: ShieldedPoolTester, TT: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::multi_pool_checkpoint::<T, TT>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

#[cfg(feature = "orchard")]
pub(crate) fn multi_pool_checkpoints_with_pruning<T: ShieldedPoolTester, TT: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::multi_pool_checkpoints_with_pruning::<T, TT>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn valid_chain_states<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::valid_chain_states::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn invalid_chain_cache_disconnected<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::invalid_chain_cache_disconnected::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn data_db_truncation<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::data_db_truncation::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn scan_cached_blocks_allows_blocks_out_of_order<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::scan_cached_blocks_allows_blocks_out_of_order::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn scan_cached_blocks_finds_received_notes<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::scan_cached_blocks_finds_received_notes::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn scan_cached_blocks_finds_change_notes<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::scan_cached_blocks_finds_change_notes::<T, _>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

pub(crate) fn scan_cached_blocks_detects_spends_out_of_order<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::scan_cached_blocks_detects_spends_out_of_order::<
        T,
        _,
    >(TestMemDbFactory::new(), MemBlockCache::new())
}

pub(crate) fn receive_two_notes_with_same_value<T: ShieldedPoolTester>() {
    zcash_client_backend::data_api::testing::pool::receive_two_notes_with_same_value::<T>(
        TestMemDbFactory::new(),
        MemBlockCache::new(),
    )
}

/// A note whose spend was detected by scanning must not be reported by the unspent-nullifier
/// query, before or after the chain nullifier map is pruned below the spend.
///
/// The query used to infer "unspent" from the absence of a nullifier-map entry. That map only
/// ever holds nullifiers the scanner could not link to a wallet note, so a spend detected in
/// order was never in it and its note was reported as unspent from the start; and `put_blocks`
/// now drops entries more than `PRUNING_DEPTH` blocks below the fully scanned height, so a spend
/// linked out of order came back as unspent once it aged past the pruning depth. The query must
/// rely on the wallet's spend record instead.
///
/// `nullifiers` performs the pool's nullifier query on the store.
pub(crate) fn spent_nullifiers_stay_hidden_after_pruning<T: ShieldedPoolTester>(
    nullifiers: impl Fn(&MemoryWalletDb<LocalNetwork>, NullifierQuery) -> Vec<Nf<T>>,
) where
    Nf<T>: PartialEq + std::fmt::Debug,
{
    let mut st = TestBuilder::new()
        .with_data_store_factory(TestMemDbFactory::new())
        .with_block_cache(MemBlockCache::new())
        .with_account_from_sapling_activation(BlockHash([0; 32]))
        .build();

    let dfvk = T::test_account_fvk(&st);
    let value = Zatoshis::const_from_u64(100_000);
    let (h, _, nf) = st.generate_next_block(&dfvk, AddressType::DefaultExternal, value);
    st.scan_cached_blocks(h, 1);
    assert_eq!(nullifiers(st.wallet(), NullifierQuery::Unspent), vec![nf]);

    // Spend the note in the next block, with change back to the wallet. Scanning links the
    // spend to the note directly, so it never enters the chain nullifier map.
    let to = T::sk_default_address(&T::sk(&[0xf5; 32]));
    let (spend_height, _) =
        st.generate_next_block_spending(&dfvk, (nf, value), to, Zatoshis::const_from_u64(60_000));
    st.scan_cached_blocks(spend_height, 1);
    assert!(
        !nullifiers(st.wallet(), NullifierQuery::Unspent).contains(&nf),
        "a mined spend hides the nullifier"
    );

    // Scan past the pruning depth, then one more batch: `put_blocks` prunes against the fully
    // scanned height as of the start of the batch, so the spend's block only falls below the
    // cutoff on the following call.
    for _ in 0..=PRUNING_DEPTH {
        st.generate_empty_block();
    }
    st.scan_cached_blocks(spend_height + 1, PRUNING_DEPTH as usize + 1);
    let (h, _) = st.generate_empty_block();
    st.scan_cached_blocks(h, 1);
    let cutoff = spend_height + 1;
    assert!(
        st.wallet()
            .nullifiers
            .values()
            .all(|(height, _)| *height > cutoff),
        "nullifier-map entries at or below the spend's height have been pruned"
    );

    assert!(
        !nullifiers(st.wallet(), NullifierQuery::Unspent).contains(&nf),
        "the note must stay spent once the nullifier map no longer covers its height"
    );
    assert!(
        nullifiers(st.wallet(), NullifierQuery::All).contains(&nf),
        "the All query still reports the spent note's nullifier"
    );
}

/// The nullifier type of a pool tester's viewing key.
type Nf<T> = <<T as ShieldedPoolTester>::Fvk as TestFvk>::Nullifier;
