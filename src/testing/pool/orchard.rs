use crate::testing;

use zcash_client_backend::data_api::testing::orchard::OrchardPoolTester;
use zcash_client_backend::data_api::testing::sapling::SaplingPoolTester;

#[test]
fn send_single_step_proposed_transfer() {
    testing::pool::send_single_step_proposed_transfer::<OrchardPoolTester>()
}

#[test]
fn scan_full_block_detects_outputs() {
    testing::pool::scan_full_block_detects_outputs::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
#[cfg(feature = "transparent-inputs")]
fn send_multi_step_proposed_transfer() {
    testing::pool::send_multi_step_proposed_transfer::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
#[cfg(feature = "transparent-inputs")]
fn proposal_fails_if_not_all_ephemeral_outputs_consumed() {
    testing::pool::proposal_fails_if_not_all_ephemeral_outputs_consumed::<OrchardPoolTester>()
}

#[test]
#[allow(deprecated)]
fn create_to_address_fails_on_incorrect_usk() {
    testing::pool::create_to_address_fails_on_incorrect_usk::<OrchardPoolTester>()
}

#[test]
#[allow(deprecated)]
fn proposal_fails_with_no_blocks() {
    testing::pool::proposal_fails_with_no_blocks::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
fn spend_fails_on_unverified_notes() {
    testing::pool::spend_fails_on_unverified_notes::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
fn spend_fails_on_locked_notes() {
    testing::pool::spend_fails_on_locked_notes::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
fn ovk_policy_prevents_recovery_from_chain() {
    testing::pool::ovk_policy_prevents_recovery_from_chain::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
fn spend_succeeds_to_t_addr_zero_change() {
    testing::pool::spend_succeeds_to_t_addr_zero_change::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
fn change_note_spends_succeed() {
    testing::pool::change_note_spends_succeed::<OrchardPoolTester>()
}

#[test]
fn external_address_change_spends_detected_in_restore_from_seed() {
    testing::pool::external_address_change_spends_detected_in_restore_from_seed::<OrchardPoolTester>(
    )
}

#[test]
#[ignore] // FIXME: #1316 This requires support for dust outputs.
fn zip317_spend() {
    testing::pool::zip317_spend::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
#[cfg(feature = "transparent-inputs")]
fn shield_transparent() {
    testing::pool::shield_transparent::<OrchardPoolTester>()
}

#[test]
fn birthday_in_anchor_shard() {
    testing::pool::birthday_in_anchor_shard::<OrchardPoolTester>()
}

#[test]
fn checkpoint_gaps() {
    testing::pool::checkpoint_gaps::<OrchardPoolTester>()
}

#[test]
#[cfg(feature = "orchard")]
fn pool_crossing_required() {
    testing::pool::pool_crossing_required::<OrchardPoolTester, SaplingPoolTester>()
}

#[test]
#[cfg(feature = "orchard")]
fn fully_funded_fully_private() {
    testing::pool::fully_funded_fully_private::<OrchardPoolTester, SaplingPoolTester>()
}

#[test]
#[cfg(all(feature = "orchard", feature = "transparent-inputs"))]
#[ignore] //FIXME
fn fully_funded_send_to_t() {
    testing::pool::fully_funded_send_to_t::<OrchardPoolTester, SaplingPoolTester>()
}

#[test]
#[cfg(feature = "orchard")]
fn multi_pool_checkpoint() {
    testing::pool::multi_pool_checkpoint::<OrchardPoolTester, SaplingPoolTester>()
}

#[test]
#[cfg(feature = "orchard")]
fn multi_pool_checkpoints_with_pruning() {
    testing::pool::multi_pool_checkpoints_with_pruning::<OrchardPoolTester, SaplingPoolTester>()
}

#[test]
fn valid_chain_states() {
    testing::pool::valid_chain_states::<OrchardPoolTester>()
}

#[test]
fn invalid_chain_cache_disconnected() {
    testing::pool::invalid_chain_cache_disconnected::<OrchardPoolTester>()
}

#[test]
fn data_db_truncation() {
    testing::pool::data_db_truncation::<OrchardPoolTester>()
}

#[test]
fn scan_cached_blocks_allows_blocks_out_of_order() {
    testing::pool::scan_cached_blocks_allows_blocks_out_of_order::<OrchardPoolTester>()
}

#[test]
fn scan_cached_blocks_finds_received_notes() {
    testing::pool::scan_cached_blocks_finds_received_notes::<OrchardPoolTester>()
}

#[test]
fn scan_cached_blocks_finds_change_notes() {
    testing::pool::scan_cached_blocks_finds_change_notes::<OrchardPoolTester>()
}

#[test]
fn scan_cached_blocks_detects_spends_out_of_order() {
    testing::pool::scan_cached_blocks_detects_spends_out_of_order::<OrchardPoolTester>()
}

#[test]
#[ignore] //FIXME
fn receive_two_notes_with_same_value() {
    testing::pool::receive_two_notes_with_same_value::<OrchardPoolTester>()
}

// NU6.3 (Ironwood) cases from the backend harness. These exercise the Ironwood note
// commitment tree and the Ironwood routing of Orchard-receiver payments. The harness's
// coinbase-shielding cases are not included: the store cannot select coinbase outputs
// (its transaction table never learns a pure-transparent transaction's block index, so the
// coinbase filter excludes them), which fails before anything Ironwood-specific runs.

#[test]
#[cfg(feature = "pczt-tests")]
fn create_pczt_supports_ironwood_output() {
    testing::pool::create_pczt_supports_ironwood_output()
}

#[test]
fn propose_v5_payment_to_orchard_receiver_is_rejected() {
    testing::pool::propose_v5_payment_to_orchard_receiver_is_rejected()
}

#[test]
fn proposal_records_and_serializes_proposed_version() {
    testing::pool::proposal_records_and_serializes_proposed_version()
}

#[test]
fn spent_nullifiers_stay_hidden_after_pruning() {
    use zcash_client_backend::data_api::WalletRead;
    testing::pool::spent_nullifiers_stay_hidden_after_pruning::<OrchardPoolTester>(
        |wallet, query| {
            wallet
                .get_orchard_nullifiers(query)
                .unwrap()
                .into_iter()
                .map(|(_, nf)| nf)
                .collect()
        },
    )
}

#[test]
fn rolled_back_spend_returns_the_note_to_unspent() {
    use zcash_client_backend::data_api::WalletRead;
    testing::pool::rolled_back_spend_returns_the_note_to_unspent::<OrchardPoolTester>(
        |wallet, query| {
            wallet
                .get_orchard_nullifiers(query)
                .unwrap()
                .into_iter()
                .map(|(_, nf)| nf)
                .collect()
        },
    );
}

#[test]
fn failed_put_blocks_leaves_the_wallet_dirty() {
    testing::pool::failed_put_blocks_leaves_the_wallet_dirty::<OrchardPoolTester>()
}
