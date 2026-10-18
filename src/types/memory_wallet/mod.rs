#![allow(dead_code)]

mod serialization;

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    num::NonZeroU32,
    ops::{Range, RangeInclusive},
};

use ::transparent::bundle::OutPoint;
#[cfg(feature = "transparent-inputs")]
use ::transparent::keys::NonHardenedChildIndex;
use incrementalmerkletree::{Address, Level, Marking, Position, Retention};
use scanning::ScanQueue;
use shardtree::{
    ShardTree,
    store::{ShardStore, memory::MemoryShardStore},
};
#[cfg(feature = "transparent-inputs")]
use zcash_client_backend::wallet::TransparentAddressMetadata;
use zcash_client_backend::{
    data_api::{
        Account as _, AccountBirthday, AccountPurpose, AccountSource, InputSource, NullifierQuery,
        Ratio, SAPLING_SHARD_HEIGHT, TransactionStatus, WalletRead,
        scanning::{ScanPriority, ScanRange},
        wallet::{ConfirmationsPolicy, TargetHeight},
    },
    wallet::{NoteId, WalletSaplingOutput},
};
use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::{
    ShieldedPool, TxId,
    consensus::{self, BlockHeight, NetworkUpgrade, TxIndex},
};
use zip32::{Scope, fingerprint::SeedFingerprint};

#[cfg(feature = "orchard")]
use zcash_client_backend::{data_api::ORCHARD_SHARD_HEIGHT, wallet::WalletOrchardOutput};

#[cfg(feature = "transparent-inputs")]
use {
    ::transparent::address::TransparentAddress,
    zcash_client_backend::wallet::WalletTransparentOutput,
};

use crate::error::Error;
use crate::types::transparent::{
    TransparentReceivedOutputSpends, TransparentReceivedOutputs, TransparentSpendCache,
};
use crate::types::*;

/// What identifies a wallet account to the application: enough to recognise the account again
/// after the wallet is restored, and to tell a derived account from an imported one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountIdentity {
    account_id: AccountId,
    ufvk: String,
    seed_fingerprint: Option<[u8; 32]>,
    hd_index: Option<u32>,
    birthday_height: BlockHeight,
}

impl AccountIdentity {
    /// The wallet-internal account identifier.
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    /// The account's unified full viewing key, encoded for the wallet's network.
    pub fn ufvk(&self) -> &str {
        &self.ufvk
    }

    /// The ZIP 32 fingerprint of the seed the account's keys were derived from, when known. It
    /// is known for derived accounts and for imported spending accounts that recorded their
    /// derivation; it is `None` for view-only accounts.
    pub fn seed_fingerprint(&self) -> Option<[u8; 32]> {
        self.seed_fingerprint
    }

    /// The ZIP 32 account index the keys were derived at, when known (see
    /// [`Self::seed_fingerprint`]).
    pub fn hd_index(&self) -> Option<u32> {
        self.hd_index
    }

    /// The height from which the account's funds are scanned for.
    pub fn birthday_height(&self) -> BlockHeight {
        self.birthday_height
    }
}

/// The main in-memory wallet database. Implements all the traits needed to be used as a backend.
#[derive(Debug)]
pub struct MemoryWalletDb<P: consensus::Parameters> {
    /// Zcash network parameters for wallet
    pub(crate) params: P,
    /// The accounts in the wallet
    pub(crate) accounts: Accounts,
    /// The wallet that have been scanned and cached that contain relevant wallet data
    pub(crate) blocks: BTreeMap<BlockHeight, MemoryWalletBlock>,
    /// Scanned transactions relevant to accounts in this wallet
    pub(crate) tx_table: TransactionTable,
    /// Notes that an account has received
    pub(crate) received_notes: ReceivedNoteTable,
    /// Notes that have been spent
    pub(crate) received_note_spends: ReceievedNoteSpends,
    /// Nullifiers for notes that have been spent
    pub(crate) nullifiers: NullifierMap,
    /// Stores the outputs of transactions created by the wallet.
    pub(crate) sent_notes: SentNoteTable,
    /// Maps transaction ids to their block height and index
    pub(crate) tx_locator: TxLocatorMap,
    /// Sapling commitment tree
    pub(crate) sapling_tree: ShardTree<
        MemoryShardStore<sapling::Node, BlockHeight>,
        { SAPLING_SHARD_HEIGHT * 2 },
        SAPLING_SHARD_HEIGHT,
    >,
    /// Stores the block height corresponding to the last note commitment in a shard
    pub(crate) sapling_tree_shard_end_heights: BTreeMap<Address, BlockHeight>,
    /// Orchard commitment tree
    #[cfg(feature = "orchard")]
    pub(crate) orchard_tree: ShardTree<
        MemoryShardStore<orchard::tree::MerkleHashOrchard, BlockHeight>,
        { ORCHARD_SHARD_HEIGHT * 2 },
        ORCHARD_SHARD_HEIGHT,
    >,
    #[cfg(feature = "orchard")]
    /// Stores the block height corresponding to the last note commitment in a shard
    pub(crate) orchard_tree_shard_end_heights: BTreeMap<Address, BlockHeight>,
    /// Ironwood (NU6.3) commitment tree. Orchard-shaped, but a separate tree for a separate pool.
    #[cfg(feature = "orchard")]
    pub(crate) ironwood_tree: ShardTree<
        MemoryShardStore<orchard::tree::MerkleHashOrchard, BlockHeight>,
        { ORCHARD_SHARD_HEIGHT * 2 },
        ORCHARD_SHARD_HEIGHT,
    >,
    #[cfg(feature = "orchard")]
    /// Stores the block height corresponding to the last note commitment in a shard
    pub(crate) ironwood_tree_shard_end_heights: BTreeMap<Address, BlockHeight>,

    /// Transparent outputs received by the wallet
    pub(crate) transparent_received_outputs: TransparentReceivedOutputs,
    /// Transparent outputs received by the wallet that have been spent
    pub(crate) transparent_received_output_spends: TransparentReceivedOutputSpends,
    /// Map between transparent outpoints and their spend transactions
    pub(crate) transparent_spend_map: TransparentSpendCache,

    /// Pending requests to the external data provider to enhance transaction data
    pub(crate) transaction_data_request_queue: TransactionDataRequestQueue,
    /// Queue of block ranges that should be scanned along with their priority
    pub(crate) scan_queue: ScanQueue,

    /// The name of the write operation that failed part-way, if any. Set by [`Self::mutate`]
    /// for the duration of an operation and cleared only when it succeeds, so an error leaves
    /// it set. Never serialized: a dirty wallet refuses to be encoded at all, and a decoded
    /// wallet is always clean. Not part of `PartialEq`.
    dirty: Option<&'static str>,
}

impl<P: consensus::Parameters + PartialEq> PartialEq for MemoryWalletDb<P> {
    /// Tests for equality between two `MemoryWalletDb` instances.
    /// but does NOT compare the sapling_tree, orchard_tree and ironwood_tree fields.
    fn eq(&self, other: &Self) -> bool {
        #[cfg(feature = "orchard")]
        let orchard_comparisons = {
            self.orchard_tree_shard_end_heights == other.orchard_tree_shard_end_heights
                && self.ironwood_tree_shard_end_heights == other.ironwood_tree_shard_end_heights
        };
        #[cfg(not(feature = "orchard"))]
        let orchard_comparisons = true;

        #[cfg(feature = "transparent-inputs")]
        let transparent_comparisons = {
            self.transparent_received_outputs == other.transparent_received_outputs
                && self.transparent_received_output_spends
                    == other.transparent_received_output_spends
                && self.transparent_spend_map == other.transparent_spend_map
        };
        #[cfg(not(feature = "transparent-inputs"))]
        let transparent_comparisons = true;

        self.params == other.params
            && self.accounts == other.accounts
            && self.blocks == other.blocks
            && self.tx_table == other.tx_table
            && self.received_notes == other.received_notes
            && self.received_note_spends == other.received_note_spends
            && self.nullifiers == other.nullifiers
            && self.sent_notes == other.sent_notes
            && self.tx_locator == other.tx_locator
            && self.scan_queue == other.scan_queue
            && self.sapling_tree_shard_end_heights == other.sapling_tree_shard_end_heights
            && orchard_comparisons
            && transparent_comparisons
            && self.transaction_data_request_queue == other.transaction_data_request_queue
    }
}

impl<P: consensus::Parameters> MemoryWalletDb<P> {
    pub fn new(params: P, max_checkpoints: usize) -> Self {
        Self {
            accounts: Accounts::new(),
            params,
            blocks: BTreeMap::new(),
            sapling_tree: ShardTree::new(MemoryShardStore::empty(), max_checkpoints),
            sapling_tree_shard_end_heights: BTreeMap::new(),
            #[cfg(feature = "orchard")]
            orchard_tree: ShardTree::new(MemoryShardStore::empty(), max_checkpoints),
            #[cfg(feature = "orchard")]
            orchard_tree_shard_end_heights: BTreeMap::new(),
            #[cfg(feature = "orchard")]
            ironwood_tree: ShardTree::new(MemoryShardStore::empty(), max_checkpoints),
            #[cfg(feature = "orchard")]
            ironwood_tree_shard_end_heights: BTreeMap::new(),
            tx_table: TransactionTable::new(),
            received_notes: ReceivedNoteTable::new(),
            sent_notes: SentNoteTable::new(),
            nullifiers: NullifierMap::new(),
            tx_locator: TxLocatorMap::new(),
            received_note_spends: ReceievedNoteSpends::new(),
            scan_queue: ScanQueue::new(),
            transparent_received_outputs: TransparentReceivedOutputs::new(),
            transparent_received_output_spends: TransparentReceivedOutputSpends::new(),
            transparent_spend_map: TransparentSpendCache::new(),
            transaction_data_request_queue: TransactionDataRequestQueue::new(),
            dirty: None,
        }
    }

    pub fn params(&self) -> &P {
        &self.params
    }

    /// The write operation that failed part-way and left the wallet in an inconsistent state,
    /// if any. A dirty wallet refuses further writes and refuses to be encoded; the caller
    /// should discard it and rebuild from the last successfully persisted encoding.
    pub fn is_dirty(&self) -> Option<&'static str> {
        self.dirty
    }

    /// Returns [`Error::DirtyState`] if a write operation failed part-way.
    pub(crate) fn check_clean(&self) -> Result<(), Error> {
        match self.dirty {
            Some(op) => Err(Error::DirtyState(op)),
            None => Ok(()),
        }
    }

    /// Runs `f`, a write operation that touches persisted state, as a unit: the wallet is marked
    /// dirty with the operation's name before `f` starts and marked clean only if `f` returns
    /// `Ok`. There is no rollback, so if `f` fails part-way the wallet stays dirty, every later
    /// mutation and every attempt to encode it fail with [`Error::DirtyState`], and the caller
    /// must rebuild the wallet from its last good encoding. Refuses to start on a dirty wallet.
    pub(crate) fn mutate<R>(
        &mut self,
        op: &'static str,
        f: impl FnOnce(&mut Self) -> Result<R, Error>,
    ) -> Result<R, Error> {
        self.check_clean()?;
        self.dirty = Some(op);
        let result = f(self)?;
        self.dirty = None;
        Ok(result)
    }

    pub(crate) fn add_account(
        &mut self,
        account_name: &str,
        kind: AccountSource,
        viewing_key: UnifiedFullViewingKey,
        birthday: AccountBirthday,
    ) -> Result<(AccountId, Account), Error> {
        self.mutate("add_account", |db| {
            db.add_account_inner(account_name, kind, viewing_key, birthday)
        })
    }

    fn add_account_inner(
        &mut self,
        account_name: &str,
        kind: AccountSource,
        viewing_key: UnifiedFullViewingKey,
        birthday: AccountBirthday,
    ) -> Result<(AccountId, Account), Error> {
        let (id, account) = self.accounts.new_account(
            account_name,
            kind,
            viewing_key.to_owned(),
            birthday.clone(),
        )?;

        // If a birthday frontier is available, insert it into the note commitment tree. If the
        // birthday frontier is the empty frontier, we don't need to do anything.
        if let Some(frontier) = birthday.sapling_frontier().value() {
            tracing::debug!("Inserting Sapling frontier into ShardTree: {:?}", frontier);
            self.sapling_tree.insert_frontier_nodes(
                frontier.clone(),
                Retention::Checkpoint {
                    // This subtraction is safe, because all leaves in the tree appear in blocks, and
                    // the invariant that birthday.height() always corresponds to the block for which
                    // `frontier` is the tree state at the start of the block. Together, this means
                    // there exists a prior block for which frontier is the tree state at the end of
                    // the block.
                    id: birthday.height() - 1,
                    marking: Marking::Reference,
                },
            )?;
        }

        #[cfg(feature = "orchard")]
        if let Some(frontier) = birthday.orchard_frontier().value() {
            tracing::debug!("Inserting Orchard frontier into ShardTree: {:?}", frontier);
            self.orchard_tree.insert_frontier_nodes(
                frontier.clone(),
                Retention::Checkpoint {
                    // This subtraction is safe, because all leaves in the tree appear in blocks, and
                    // the invariant that birthday.height() always corresponds to the block for which
                    // `frontier` is the tree state at the start of the block. Together, this means
                    // there exists a prior block for which frontier is the tree state at the end of
                    // the block.
                    id: birthday.height() - 1,
                    marking: Marking::Reference,
                },
            )?;
        }

        #[cfg(feature = "orchard")]
        if let Some(frontier) = birthday.prior_chain_state().final_ironwood_tree().value() {
            tracing::debug!("Inserting Ironwood frontier into ShardTree: {:?}", frontier);
            self.ironwood_tree.insert_frontier_nodes(
                frontier.clone(),
                Retention::Checkpoint {
                    // See the Sapling case above for why this subtraction is safe.
                    id: birthday.height() - 1,
                    marking: Marking::Reference,
                },
            )?;
        }

        // The ignored range always starts at Sapling activation
        let sapling_activation_height = self
            .params
            .activation_height(NetworkUpgrade::Sapling)
            .expect("Sapling activation height must be available.");

        // Add the ignored range up to the birthday height.
        if sapling_activation_height < birthday.height() {
            let ignored_range = sapling_activation_height..birthday.height();
            self.scan_queue.replace_queue_entries(
                &ignored_range,
                Some(ScanRange::from_parts(
                    ignored_range.clone(),
                    ScanPriority::Ignored,
                ))
                .into_iter(),
                false,
            )?;
        };

        // Rewrite the scan ranges from the birthday height up to the chain tip so that we'll ensure we
        // re-scan to find any notes that might belong to the newly added account.
        if let Some(t) = self.chain_height()? {
            let rescan_range = birthday.height()..(t + 1);
            self.scan_queue.replace_queue_entries(
                &rescan_range,
                Some(ScanRange::from_parts(
                    rescan_range.clone(),
                    ScanPriority::Historic,
                ))
                .into_iter(),
                true, // force rescan
            )?;
        }

        Ok((id, account))
    }

    #[cfg(feature = "transparent-inputs")]
    pub(crate) fn first_unsafe_index(&self, account_id: AccountId) -> Result<u32, Error> {
        let first_unmined_index = if let Some(account) = self.accounts.get(account_id) {
            let mut idx = 0;
            for (tidx, eph_addr) in account.ephemeral_addresses.iter().rev() {
                if eph_addr
                    .seen
                    .and_then(|txid| self.tx_table.get(&txid))
                    .and_then(|tx| tx.mined_height())
                    .is_some()
                {
                    idx = tidx.checked_add(1).unwrap();
                    break;
                }
            }
            idx
        } else {
            0
        };

        Ok(core::cmp::min(
            1 << 31,
            first_unmined_index
                .checked_add(account::EPHEMERAL_GAP_LIMIT)
                .unwrap(),
        ))
    }

    /// Returns the wallet account that most likely funded the transaction with the
    /// given txid, based on the wallet's stored spend records (across transparent,
    /// sapling, and orchard pools), if any. If multiple wallet accounts contributed
    /// inputs to the transaction, the account that contributed the most value is
    /// selected; ties are broken in favor of the account whose oldest contributed
    /// input has the lowest mined height (with unmined inputs sorting last).
    ///
    /// `zcash_client_backend` does not currently support representing multiple
    /// funding accounts on a single output; this heuristic provides a deterministic
    /// single-account choice when more than one wallet account contributed funds.
    pub(crate) fn find_funding_account(&self, creating_txid: &TxId) -> Option<AccountId> {
        // Per-account: (total_value, oldest_mined_height).
        let mut contribs: std::collections::BTreeMap<AccountId, (u64, BlockHeight)> =
            std::collections::BTreeMap::new();

        let mut record = |account_id: AccountId, value: u64, source_txid: &TxId| {
            let mined = self
                .tx_table
                .get_transaction(source_txid)
                .and_then(|tx| tx.mined_height())
                .unwrap_or(BlockHeight::from_u32(u32::MAX));
            contribs
                .entry(account_id)
                .and_modify(|(v, h)| {
                    *v += value;
                    if mined < *h {
                        *h = mined;
                    }
                })
                .or_insert((value, mined));
        };

        #[cfg(feature = "transparent-inputs")]
        for (outpoint, spending_txid) in self.transparent_received_output_spends.iter() {
            if spending_txid == creating_txid {
                if let Some(received) = self.transparent_received_outputs.get(outpoint) {
                    record(
                        received.account_id,
                        u64::from(received.txout.value()),
                        &received.transaction_id,
                    );
                }
            }
        }

        for (note_id, spending_txid) in self.received_note_spends.iter() {
            if spending_txid == creating_txid {
                if let Some(received) = self.received_notes.iter().find(|n| &n.note_id == note_id) {
                    record(
                        received.account_id,
                        received.note.value().into_u64(),
                        &received.txid,
                    );
                }
            }
        }

        contribs
            .into_iter()
            .max_by(|(a_id, (a_v, a_h)), (b_id, (b_v, b_h))| {
                // Highest value first; older (lower) mined height first; account id ascending.
                a_v.cmp(b_v)
                    .then_with(|| b_h.cmp(a_h))
                    .then_with(|| b_id.cmp(a_id))
            })
            .map(|(id, _)| id)
    }

    pub(crate) fn get_funding_accounts(
        &self,
        tx: &Transaction,
    ) -> Result<BTreeSet<AccountId>, Error> {
        let mut funding_accounts = BTreeSet::new();
        #[cfg(feature = "transparent-inputs")]
        funding_accounts.extend(
            self.transparent_received_outputs.detect_spending_accounts(
                tx.transparent_bundle()
                    .iter()
                    .flat_map(|bundle| bundle.vin.iter().map(|txin| txin.prevout())),
            )?,
        );

        funding_accounts.extend(self.received_notes.detect_sapling_spending_accounts(
            tx.sapling_bundle().iter().flat_map(|bundle| {
                bundle
                    .shielded_spends()
                    .iter()
                    .map(|spend| spend.nullifier())
            }),
        )?);

        #[cfg(feature = "orchard")]
        funding_accounts.extend(
            self.received_notes.detect_orchard_spending_accounts(
                tx.orchard_bundle()
                    .iter()
                    .flat_map(|bundle| bundle.actions().iter().map(|action| action.nullifier())),
            )?,
        );

        #[cfg(feature = "orchard")]
        funding_accounts.extend(
            self.received_notes.detect_ironwood_spending_accounts(
                tx.ironwood_bundle()
                    .iter()
                    .flat_map(|bundle| bundle.actions().iter().map(|action| action.nullifier())),
            )?,
        );

        Ok(funding_accounts)
    }

    pub(crate) fn get_received_notes(&self) -> &ReceivedNoteTable {
        &self.received_notes
    }

    pub(crate) fn get_received_note(&self, note_id: NoteId) -> Option<&ReceivedNote> {
        self.received_notes.find_by_note_id(&note_id)
    }

    pub(crate) fn mark_sapling_note_spent(
        &mut self,
        nf: sapling::Nullifier,
        txid: TxId,
    ) -> Result<(), Error> {
        let note_id = self
            .received_notes
            .find_by_nullifier(&Nullifier::Sapling(nf))
            .map(|n| n.note_id())
            .ok_or(Error::NoteNotFound)?;
        self.received_note_spends.insert_spend(note_id, txid);
        Ok(())
    }

    /// Returns true if the note is in the spent notes table and the transaction that spent it is
    /// in the transaction table and has either been mined or can be mined in the future
    /// (i.e. it hasn't or will not expire)
    /// Returns, for every received note whose nullifier `select` accepts, the account that
    /// received the note together with the selected nullifier.
    ///
    /// For [`NullifierQuery::Unspent`], notes with an effective spend (see
    /// [`Self::spend_is_effective`]) are omitted. The wallet's spend record is consulted rather
    /// than the chain nullifier map: that map is pruned below the fully scanned height, so its
    /// absence of an entry cannot distinguish an unspent note from one spent long ago.
    pub(crate) fn select_nullifiers<N>(
        &self,
        query: NullifierQuery,
        select: impl Fn(Option<&Nullifier>) -> Option<N>,
    ) -> Vec<(AccountId, N)> {
        self.received_notes
            .iter()
            .filter_map(|note| {
                let nf = select(note.nullifier())?;
                match query {
                    NullifierQuery::All => Some((note.account_id(), nf)),
                    NullifierQuery::Unspent => (!self.spend_is_effective(&note.note_id()))
                        .then_some((note.account_id(), nf)),
                }
            })
            .collect()
    }

    /// Whether the note has a recorded spend that expiry cannot undo: the spending transaction
    /// is mined, or has no expiry height. A spend by an unmined transaction that can still
    /// expire is not effective, so the note's nullifier keeps being watched for.
    pub(crate) fn spend_is_effective(&self, note_id: &NoteId) -> bool {
        self.received_note_spends
            .get(note_id)
            .and_then(|txid| self.tx_table.get(txid))
            .is_some_and(|tx| {
                // As in `zcash_client_sqlite`, an unmined spend only counts once it can no longer
                // expire: a raw transaction with expiry height 0. `None` means the expiry is
                // unknown (the entry came from a scan), so the spend may still lapse.
                matches!(tx.status(), TransactionStatus::Mined(_))
                    || tx.expiry_height() == Some(BlockHeight::from(0))
            })
    }

    pub(crate) fn note_is_spent(
        &self,
        note: &ReceivedNote,
        target_height: TargetHeight,
    ) -> Result<bool, Error> {
        let spend = self.received_note_spends.get(&note.note_id());

        let spent = match spend {
            Some(txid) => {
                let spending_tx = self
                    .tx_table
                    .get(txid)
                    .ok_or(Error::TransactionNotFound(*txid))?;
                match spending_tx.status() {
                    TransactionStatus::Mined(_height) => true,
                    TransactionStatus::TxidNotRecognized => unreachable!(),
                    TransactionStatus::NotInMainChain => {
                        // The spend counts while the transaction cannot have lapsed: it never
                        // expires (expiry height 0) or expires at or after the target height.
                        // An unknown expiry (`None`, an entry created by a scan that was then
                        // rolled back) may have lapsed, so the note is treated as unspent.
                        spending_tx.expiry_height().is_some_and(|h| {
                            h == BlockHeight::from(0) || h >= BlockHeight::from(target_height)
                        })
                    }
                }
            }
            None => false,
        };
        Ok(spent)
    }

    /// To be spendable a note must be:
    /// - unspent (obviously)
    /// - not dust (value > 5000 ZATs)
    /// - be associated with an account with a ufvk
    /// - have a recipient key scope
    /// - We know the nullifier
    /// - We know the commitment tree position
    /// - be in a block less than or equal to the anchor height
    /// - not be in the given exclude list
    ///
    /// Additionally the tree shard containing the node must not be in an unscanned range
    /// excluding ranges that start above the anchor height or end below the wallet birthday.
    /// This is determined by looking at the scan queue
    pub(crate) fn note_is_spendable(
        &self,
        note: &ReceivedNote,
        birthday_height: BlockHeight,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        exclude: &[<MemoryWalletDb<P> as InputSource>::NoteRef],
    ) -> Result<bool, Error> {
        let note_account = self
            .get_account(note.account_id())?
            .ok_or(Error::AccountUnknown(note.account_id))?;
        let note_txn = self
            .tx_table
            .get(&note.txid())
            .ok_or_else(|| Error::TransactionNotFound(note.txid()))?;

        let unscanned_ranges = self.unscanned_ranges();
        let anchor_height = target_height - u32::from(confirmations_policy.trusted());

        let note_in_unscanned_range =
            unscanned_ranges
                .iter()
                .any(|(start_height, end_height, start, end_exclusive)| {
                    let in_range = note.commitment_tree_position.is_some_and(|pos| {
                        if let (Some(start), Some(end_exclusive)) = (start, end_exclusive) {
                            pos >= *start && pos < *end_exclusive
                        } else {
                            true
                        }
                    });
                    in_range && *end_height > birthday_height && *start_height <= anchor_height
                });

        Ok(!self.note_is_spent(note, target_height)?
            && !note_in_unscanned_range
            && note.note.value().into_u64() > 5000
            && note_account.ufvk().is_some()
            && note.nullifier().is_some()
            && note.commitment_tree_position.is_some()
            && note_txn.mined_height().is_some()
            && match note.recipient_key_scope {
                Some(Scope::External) => {
                    target_height.saturating_sub(u32::from(confirmations_policy.untrusted()))
                        >= note_txn.mined_height().unwrap()
                }
                Some(Scope::Internal) => {
                    target_height.saturating_sub(u32::from(confirmations_policy.trusted()))
                        >= note_txn.mined_height().unwrap()
                }
                None => false,
            }
            && !exclude.contains(&note.note_id()))
    }

    /// To be pending a note must be:
    /// - mined, but without sufficient confirmations to be spent; or
    /// - unmined and unexpired
    pub(crate) fn note_is_pending(
        &self,
        note: &ReceivedNote,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<bool, Error> {
        let tx = self
            .tx_table
            .get(&note.txid())
            .ok_or_else(|| Error::TransactionNotFound(note.txid()))?;

        Ok(tx.mined_height().map_or_else(
            || {
                tx.expiry_height()
                    .iter()
                    .all(|h| *h >= BlockHeight::from(target_height))
            },
            |h| match note.recipient_key_scope {
                // If we don't know the recipient key scope, we treat the note as untrusted.
                Some(Scope::External) | None => {
                    BlockHeight::from(target_height) - h
                        < u32::from(confirmations_policy.untrusted())
                }
                Some(Scope::Internal) => {
                    BlockHeight::from(target_height) - h < u32::from(confirmations_policy.trusted())
                }
            },
        ))
    }

    pub(crate) fn summary_height(
        &self,
        min_confirmations: u32,
    ) -> Result<Option<BlockHeight>, Error> {
        let chain_tip_height = match self.chain_height()? {
            Some(height) => height,
            None => return Ok(None),
        };
        let summary_height =
            (chain_tip_height + 1).saturating_sub(std::cmp::max(min_confirmations, 1));
        Ok(Some(summary_height))
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn mark_orchard_note_spent(
        &mut self,
        nf: orchard::note::Nullifier,
        txid: TxId,
    ) -> Result<(), Error> {
        self.mark_orchard_family_note_spent(Nullifier::Orchard(nf), txid)
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn mark_ironwood_note_spent(
        &mut self,
        nf: orchard::note::Nullifier,
        txid: TxId,
    ) -> Result<(), Error> {
        self.mark_orchard_family_note_spent(Nullifier::Ironwood(nf), txid)
    }

    #[cfg(feature = "orchard")]
    fn mark_orchard_family_note_spent(&mut self, nf: Nullifier, txid: TxId) -> Result<(), Error> {
        let note_id = self
            .received_notes
            .find_by_nullifier(&nf)
            .map(|n| n.note_id())
            .ok_or(Error::NoteNotFound)?;
        self.received_note_spends.insert_spend(note_id, txid);
        Ok(())
    }

    pub(crate) fn max_zip32_account_index(
        &self,
        seed_fingerprint: &SeedFingerprint,
    ) -> Result<Option<zip32::AccountId>, Error> {
        Ok(self
            .accounts
            .values()
            .filter_map(|a| match a.source() {
                AccountSource::Derived { derivation, .. } => {
                    if derivation.seed_fingerprint() == seed_fingerprint {
                        Some(derivation.account_index())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .max())
    }
    pub(crate) fn insert_received_sapling_note(
        &mut self,
        note_id: NoteId,
        output: &WalletSaplingOutput<AccountId>,
        spent_in: Option<TxId>,
    ) {
        self.received_notes
            .insert_received_note(ReceivedNote::from_wallet_sapling_output(note_id, output));
        if let Some(spent_in) = spent_in {
            self.received_note_spends.insert_spend(note_id, spent_in);
        }
    }
    /// Records a scanned Orchard-family output (Orchard or Ironwood, by the output's value pool)
    /// as a received note.
    #[cfg(feature = "orchard")]
    pub(crate) fn insert_received_orchard_note(
        &mut self,
        note_id: NoteId,
        output: &WalletOrchardOutput<AccountId>,
        spent_in: Option<TxId>,
    ) {
        self.received_notes
            .insert_received_note(ReceivedNote::from_wallet_orchard_output(note_id, output));
        if let Some(spent_in) = spent_in {
            self.received_note_spends.insert_spend(note_id, spent_in);
        }
    }
    /// Drops nullifier-map and transaction-locator entries mined below
    /// `fully_scanned_height - pruning_depth`, matching `zcash_client_sqlite`'s
    /// `prune_tracked_nullifiers`.
    ///
    /// The maps exist so that a note found by out-of-order scanning can be matched to a spend in a
    /// block that was scanned earlier. Every block below the fully scanned height has been scanned,
    /// so no undiscovered note can exist there, and once the height is also past reorg depth the
    /// entries below it can never be looked up again. Without this, a historic sync keeps every
    /// nullifier on chain in memory (and in the serialized wallet) for the life of the wallet.
    pub(crate) fn prune_tracked_nullifiers(&mut self, pruning_depth: u32) -> Result<(), Error> {
        if let Some(meta) = self.block_fully_scanned()? {
            self.prune_nullifier_map_below(meta.block_height().saturating_sub(pruning_depth));
        }
        Ok(())
    }

    /// Removes every nullifier-map and transaction-locator entry mined below `cutoff`.
    pub(crate) fn prune_nullifier_map_below(&mut self, cutoff: BlockHeight) {
        self.tx_locator.0.retain(|(height, _), _| *height >= cutoff);
        self.nullifiers.0.retain(|_, (height, _)| *height >= cutoff);
    }

    pub(crate) fn insert_sapling_nullifier_map(
        &mut self,
        block_height: BlockHeight,
        new_entries: &[(TxIndex, TxId, Vec<sapling::Nullifier>)],
    ) -> Result<(), Error> {
        for (tx_index, txid, nullifiers) in new_entries {
            for nf in nullifiers.iter() {
                self.nullifiers
                    .insert(block_height, u32::from(*tx_index), Nullifier::Sapling(*nf));
            }
            match self.tx_locator.entry((block_height, u32::from(*tx_index))) {
                Entry::Occupied(x) => {
                    if txid == x.get() {
                        // This is a duplicate entry
                        continue;
                    } else {
                        return Err(Error::ConflictingTxLocator);
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(*txid);
                }
            }
        }
        Ok(())
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn insert_orchard_nullifier_map(
        &mut self,
        block_height: BlockHeight,
        new_entries: &[(TxIndex, TxId, Vec<orchard::note::Nullifier>)],
    ) -> Result<(), Error> {
        self.insert_orchard_family_nullifier_map(
            orchard::ValuePool::Orchard,
            block_height,
            new_entries,
        )
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn insert_ironwood_nullifier_map(
        &mut self,
        block_height: BlockHeight,
        new_entries: &[(TxIndex, TxId, Vec<orchard::note::Nullifier>)],
    ) -> Result<(), Error> {
        self.insert_orchard_family_nullifier_map(
            orchard::ValuePool::Ironwood,
            block_height,
            new_entries,
        )
    }

    #[cfg(feature = "orchard")]
    fn insert_orchard_family_nullifier_map(
        &mut self,
        pool: orchard::ValuePool,
        block_height: BlockHeight,
        new_entries: &[(TxIndex, TxId, Vec<orchard::note::Nullifier>)],
    ) -> Result<(), Error> {
        for (tx_index, txid, nullifiers) in new_entries {
            for nf in nullifiers.iter() {
                self.nullifiers.insert(
                    block_height,
                    u32::from(*tx_index),
                    Nullifier::orchard_family(pool, *nf),
                );
            }
            match self.tx_locator.entry((block_height, u32::from(*tx_index))) {
                Entry::Occupied(x) => {
                    if txid == x.get() {
                        // This is a duplicate entry
                        continue;
                    } else {
                        return Err(Error::ConflictingTxLocator);
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(*txid);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn block_height_extrema(&self) -> Option<RangeInclusive<BlockHeight>> {
        let (min, max) = self.blocks.keys().fold((None, None), |(min, max), height| {
            (
                Some(min.map_or(height, |min| std::cmp::min(min, height))),
                Some(max.map_or(height, |max| std::cmp::max(max, height))),
            )
        });
        if let (Some(min), Some(max)) = (min, max) {
            Some(*min..=*max)
        } else {
            None
        }
    }

    pub(crate) fn sapling_tip_shard_end_height(&self) -> Option<BlockHeight> {
        self.sapling_tree_shard_end_heights.values().max().copied()
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn orchard_tip_shard_end_height(&self) -> Option<BlockHeight> {
        self.orchard_tree_shard_end_heights.values().max().copied()
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn ironwood_tip_shard_end_height(&self) -> Option<BlockHeight> {
        self.ironwood_tree_shard_end_heights.values().max().copied()
    }

    pub(crate) fn get_sapling_max_checkpointed_height(
        &self,
        chain_tip_height: BlockHeight,
        min_confirmations: NonZeroU32,
    ) -> Result<Option<BlockHeight>, Error> {
        max_checkpointed_height(
            self.sapling_tree.store(),
            chain_tip_height,
            min_confirmations,
        )
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn get_orchard_max_checkpointed_height(
        &self,
        chain_tip_height: BlockHeight,
        min_confirmations: NonZeroU32,
    ) -> Result<Option<BlockHeight>, Error> {
        max_checkpointed_height(
            self.orchard_tree.store(),
            chain_tip_height,
            min_confirmations,
        )
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn get_ironwood_max_checkpointed_height(
        &self,
        chain_tip_height: BlockHeight,
        min_confirmations: NonZeroU32,
    ) -> Result<Option<BlockHeight>, Error> {
        max_checkpointed_height(
            self.ironwood_tree.store(),
            chain_tip_height,
            min_confirmations,
        )
    }

    /// Get the unscanned ranges from the scan queue and their corresponding sapling tree indices
    /// This can be used to determine if a note is in an unscanned range and therefore not spendable
    pub(crate) fn unscanned_ranges(
        &self,
    ) -> Vec<(BlockHeight, BlockHeight, Option<Position>, Option<Position>)> {
        self.scan_queue
            .iter()
            .filter(|(_, _, priority)| priority > &ScanPriority::Scanned)
            .map(|(start, end, _)| {
                (
                    *start,
                    *end,
                    self.first_subtree_for_height(start)
                        .map(|a| a.position_range_start()),
                    self.last_subtree_for_height(end)
                        .map(|a| a.position_range_end()),
                )
            })
            .collect()
    }

    /// Return the address of the last subtree in the sapling tree where note for a give block height was found
    pub(crate) fn last_subtree_for_height(&self, height: &BlockHeight) -> Option<Address> {
        self.sapling_tree_shard_end_heights
            .iter()
            .filter(|(_, h)| *h == height)
            .map(|(a, _)| *a)
            .max()
    }

    ///  Return the address of the first subtree in the sapling tree where note for a give block height was found
    pub(crate) fn first_subtree_for_height(&self, height: &BlockHeight) -> Option<Address> {
        // The first subtree is the last subtree for the previous height
        self.last_subtree_for_height(&height.saturating_sub(1))
    }

    /// Makes the required changes to the scan queue to reflect the completion of a scan
    pub(crate) fn scan_complete(
        &mut self,
        range: Range<BlockHeight>,
        wallet_note_positions: &[(ShieldedPool, Position)],
    ) -> Result<(), Error> {
        let wallet_birthday = self.get_wallet_birthday()?;

        // Determine the range of block heights for which we will be updating the scan queue.
        let extended_range = {
            // If notes have been detected in the scan, we need to extend any adjacent un-scanned
            // ranges starting from the wallet birthday to include the blocks needed to complete
            // the note commitment tree subtrees containing the positions of the discovered notes.
            // We will query by subtree index to find these bounds.
            let mut required_sapling_subtrees = BTreeSet::new();
            #[cfg(feature = "orchard")]
            let mut required_orchard_subtrees = BTreeSet::new();
            #[cfg(feature = "orchard")]
            let mut required_ironwood_subtrees = BTreeSet::new();
            for (protocol, position) in wallet_note_positions {
                match protocol {
                    ShieldedPool::Sapling => {
                        required_sapling_subtrees.insert(
                            Address::above_position(SAPLING_SHARD_HEIGHT.into(), *position).index(),
                        );
                    }
                    ShieldedPool::Orchard => {
                        #[cfg(feature = "orchard")]
                        required_orchard_subtrees.insert(
                            Address::above_position(ORCHARD_SHARD_HEIGHT.into(), *position).index(),
                        );

                        #[cfg(not(feature = "orchard"))]
                        return Err(Error::OrchardNotEnabled);
                    }
                    ShieldedPool::Ironwood => {
                        #[cfg(feature = "orchard")]
                        required_ironwood_subtrees.insert(
                            Address::above_position(ORCHARD_SHARD_HEIGHT.into(), *position).index(),
                        );

                        #[cfg(not(feature = "orchard"))]
                        return Err(Error::OrchardNotEnabled);
                    }
                }
            }

            let extended_range = self.extend_range(
                &ShieldedPool::Sapling,
                &range,
                required_sapling_subtrees,
                self.params.activation_height(NetworkUpgrade::Sapling),
                wallet_birthday,
            )?;

            #[cfg(feature = "orchard")]
            let extended_range = self
                .extend_range(
                    &ShieldedPool::Orchard,
                    extended_range.as_ref().unwrap_or(&range),
                    required_orchard_subtrees,
                    self.params.activation_height(NetworkUpgrade::Nu5),
                    wallet_birthday,
                )?
                .or(extended_range);

            #[cfg(feature = "orchard")]
            let extended_range = self
                .extend_range(
                    &ShieldedPool::Ironwood,
                    extended_range.as_ref().unwrap_or(&range),
                    required_ironwood_subtrees,
                    self.params.activation_height(NetworkUpgrade::Nu6_3),
                    wallet_birthday,
                )?
                .or(extended_range);

            #[allow(clippy::let_and_return)]
            extended_range
        };

        let query_range = extended_range.clone().unwrap_or_else(|| range.clone());

        let scanned = ScanRange::from_parts(range.clone(), ScanPriority::Scanned);

        // If any of the extended range actually extends beyond the scanned range, we need to
        // scan that extension in order to make the found note(s) spendable. We need to avoid
        // creating empty ranges here, as that acts as an optimization barrier preventing
        // `SpanningTree` from merging non-empty scanned ranges on either side.
        let extended_before = extended_range
            .as_ref()
            .map(|extended| {
                ScanRange::from_parts(extended.start..range.start, ScanPriority::FoundNote)
            })
            .filter(|range| !range.is_empty());
        let extended_after = extended_range
            .map(|extended| ScanRange::from_parts(range.end..extended.end, ScanPriority::FoundNote))
            .filter(|range| !range.is_empty());

        let replacement = Some(scanned)
            .into_iter()
            .chain(extended_before)
            .chain(extended_after);

        self.scan_queue
            .replace_queue_entries(&query_range, replacement, false)
    }

    // Given a range of block heights, extend the range to include the subtrees containing the
    // given subtree indices, bounded by the wallet birthday and the fallback start height.
    fn extend_range(
        &self,
        pool: &ShieldedPool,
        range: &Range<BlockHeight>,
        required_subtree_indices: BTreeSet<u64>,
        fallback_start_height: Option<BlockHeight>,
        birthday_height: Option<BlockHeight>,
    ) -> Result<Option<Range<BlockHeight>>, Error> {
        // we'll either have both min and max bounds, or we'll have neither
        let subtree_index_bounds = required_subtree_indices
            .iter()
            .min()
            .zip(required_subtree_indices.iter().max());

        let shard_end = |index| -> Result<_, Error> {
            match pool {
                ShieldedPool::Sapling => Ok(self
                    .sapling_tree_shard_end_heights
                    .get(&Address::from_parts(SAPLING_SHARD_HEIGHT.into(), index))
                    .cloned()),
                ShieldedPool::Orchard => {
                    #[cfg(feature = "orchard")]
                    {
                        Ok(self
                            .orchard_tree_shard_end_heights
                            .get(&Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), index))
                            .cloned())
                    }
                    #[cfg(not(feature = "orchard"))]
                    panic!("Unsupported pool")
                }
                ShieldedPool::Ironwood => {
                    #[cfg(feature = "orchard")]
                    {
                        Ok(self
                            .ironwood_tree_shard_end_heights
                            .get(&Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), index))
                            .cloned())
                    }
                    #[cfg(not(feature = "orchard"))]
                    panic!("Unsupported pool")
                }
            }
        };

        // If no notes belonging to the wallet were found, we don't need to extend the scanning
        // range suggestions to include the associated subtrees, and our bounds are just the
        // scanned range. Otherwise, ensure that all shard ranges starting from the wallet
        // birthday are included.
        subtree_index_bounds
            .map(|(min_idx, max_idx)| {
                let range_min = if *min_idx > 0 {
                    // get the block height of the end of the previous shard
                    shard_end(*min_idx - 1)?
                } else {
                    // our lower bound is going to be the fallback height
                    fallback_start_height
                };

                // bound the minimum to the wallet birthday
                let range_min =
                    range_min.map(|h| birthday_height.map_or(h, |b| std::cmp::max(b, h)));

                // Get the block height for the end of the current shard, and make it an
                // exclusive end bound.
                let range_max = shard_end(*max_idx)?.map(|end| end + 1);

                Ok(Range {
                    start: range.start.min(range_min.unwrap_or(range.start)),
                    end: range.end.max(range_max.unwrap_or(range.end)),
                })
            })
            .transpose()
    }

    pub(crate) fn get_sent_notes(&self) -> &SentNoteTable {
        &self.sent_notes
    }

    pub(crate) fn sapling_scan_progress(
        &self,
        birthday_height: &BlockHeight,
        fully_scanned_height: &BlockHeight,
        chain_tip_height: &BlockHeight,
    ) -> Result<Option<Ratio<u64>>, Error> {
        if fully_scanned_height == chain_tip_height {
            let outputs_sum = self
                .blocks
                .iter()
                .filter(|(height, _)| height >= &birthday_height)
                .fold(0, |sum, (_, block)| {
                    sum + block.sapling_output_count.unwrap_or(0)
                });
            Ok(Some(Ratio::new(
                u64::from(outputs_sum),
                u64::from(outputs_sum),
            )))
        } else {
            // Get the starting note commitment tree size from the wallet birthday, or failing that
            // from the blocks table.
            let start_size = self
                .accounts
                .values()
                .filter_map(|account| {
                    if account.birthday().height() == *birthday_height {
                        Some(account.birthday().sapling_frontier().tree_size())
                    } else {
                        None
                    }
                })
                .next()
                .or_else(|| {
                    self.blocks
                        .iter()
                        .filter(|(height, _)| height <= &birthday_height)
                        .map(|(_, block)| {
                            u64::from(
                                block.sapling_commitment_tree_size.unwrap_or(0)
                                    - block.sapling_output_count.unwrap_or(0),
                            )
                        })
                        .max()
                });

            // Compute the total blocks scanned so far above the starting height
            let scanned_count = self
                .blocks
                .iter()
                .filter(|(height, _)| height > &birthday_height)
                .fold(0_u64, |acc, (_, block)| {
                    acc + u64::from(block.sapling_output_count.unwrap_or(0))
                });

            // We don't have complete information on how many outputs will exist in the shard at
            // the chain tip without having scanned the chain tip block, so we overestimate by
            // computing the maximum possible number of notes directly from the shard indices.
            //
            // TODO: it would be nice to be able to reliably have the size of the commitment tree
            // at the chain tip without having to have scanned that block.

            let shard_index_iter = self
                .sapling_tree_shard_end_heights
                .iter()
                .filter(|(_, height)| height > &birthday_height)
                .map(|(address, _)| address.index());

            let min_idx = shard_index_iter.clone().min().unwrap_or(0);
            let max_idx = shard_index_iter.max().unwrap_or(0);

            let max_tree_size = Some(min_idx << SAPLING_SHARD_HEIGHT);
            let min_tree_size = Some((max_idx + 1) << SAPLING_SHARD_HEIGHT);

            Ok(start_size.or(min_tree_size).zip(max_tree_size).map(
                |(min_tree_size, max_tree_size)| {
                    Ratio::new(scanned_count, max_tree_size - min_tree_size)
                },
            ))
        }
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn orchard_scan_progress(
        &self,
        birthday_height: &BlockHeight,
        fully_scanned_height: &BlockHeight,
        chain_tip_height: &BlockHeight,
    ) -> Result<Option<Ratio<u64>>, Error> {
        self.orchard_family_scan_progress(
            orchard::ValuePool::Orchard,
            birthday_height,
            fully_scanned_height,
            chain_tip_height,
        )
    }

    #[cfg(feature = "orchard")]
    pub(crate) fn ironwood_scan_progress(
        &self,
        birthday_height: &BlockHeight,
        fully_scanned_height: &BlockHeight,
        chain_tip_height: &BlockHeight,
    ) -> Result<Option<Ratio<u64>>, Error> {
        self.orchard_family_scan_progress(
            orchard::ValuePool::Ironwood,
            birthday_height,
            fully_scanned_height,
            chain_tip_height,
        )
    }

    /// Scan progress for the Orchard or the Ironwood pool, which share the tree shape and
    /// therefore the estimate; only the per-block counters and shard heights differ.
    #[cfg(feature = "orchard")]
    fn orchard_family_scan_progress(
        &self,
        pool: orchard::ValuePool,
        birthday_height: &BlockHeight,
        fully_scanned_height: &BlockHeight,
        chain_tip_height: &BlockHeight,
    ) -> Result<Option<Ratio<u64>>, Error> {
        let action_count = |block: &MemoryWalletBlock| match pool {
            orchard::ValuePool::Orchard => block.orchard_action_count.unwrap_or(0),
            orchard::ValuePool::Ironwood => block.ironwood_action_count.unwrap_or(0),
        };
        let tree_size = |block: &MemoryWalletBlock| match pool {
            orchard::ValuePool::Orchard => block.orchard_commitment_tree_size.unwrap_or(0),
            orchard::ValuePool::Ironwood => block.ironwood_commitment_tree_size.unwrap_or(0),
        };
        let shard_end_heights = match pool {
            orchard::ValuePool::Orchard => &self.orchard_tree_shard_end_heights,
            orchard::ValuePool::Ironwood => &self.ironwood_tree_shard_end_heights,
        };

        if fully_scanned_height == chain_tip_height {
            let outputs_sum = self
                .blocks
                .iter()
                .filter(|(height, _)| height >= &birthday_height)
                .fold(0, |sum, (_, block)| sum + action_count(block));
            Ok(Some(Ratio::new(
                u64::from(outputs_sum),
                u64::from(outputs_sum),
            )))
        } else {
            // Get the starting note commitment tree size from the wallet birthday, or failing that
            // from the blocks table.
            let start_size = self
                .accounts
                .values()
                .filter_map(|account| {
                    if account.birthday().height() == *birthday_height {
                        Some(match pool {
                            orchard::ValuePool::Orchard => {
                                account.birthday().orchard_frontier().tree_size()
                            }
                            orchard::ValuePool::Ironwood => account
                                .birthday()
                                .prior_chain_state()
                                .final_ironwood_tree()
                                .tree_size(),
                        })
                    } else {
                        None
                    }
                })
                .next()
                .or_else(|| {
                    self.blocks
                        .iter()
                        .filter(|(height, _)| height <= &birthday_height)
                        .map(|(_, block)| u64::from(tree_size(block) - action_count(block)))
                        .max()
                });

            // Compute the total blocks scanned so far above the starting height
            let scanned_count = self
                .blocks
                .iter()
                .filter(|(height, _)| height > &birthday_height)
                .fold(0_u64, |acc, (_, block)| {
                    acc + u64::from(action_count(block))
                });

            // We don't have complete information on how many outputs will exist in the shard at
            // the chain tip without having scanned the chain tip block, so we overestimate by
            // computing the maximum possible number of notes directly from the shard indices.
            //
            // TODO: it would be nice to be able to reliably have the size of the commitment tree
            // at the chain tip without having to have scanned that block.

            let shard_index_iter = shard_end_heights
                .iter()
                .filter(|(_, height)| height > &birthday_height)
                .map(|(address, _)| address.index());

            let min_idx = shard_index_iter.clone().min().unwrap_or(0);
            let max_idx = shard_index_iter.max().unwrap_or(0);

            let max_tree_size = Some(min_idx << ORCHARD_SHARD_HEIGHT);
            let min_tree_size = Some((max_idx + 1) << ORCHARD_SHARD_HEIGHT);

            Ok(start_size.or(min_tree_size).zip(max_tree_size).map(
                |(min_tree_size, max_tree_size)| {
                    Ratio::new(scanned_count, max_tree_size - min_tree_size)
                },
            ))
        }
    }

    #[cfg(feature = "transparent-inputs")]
    pub(crate) fn find_account_for_transparent_address(
        &self,
        address: &TransparentAddress,
    ) -> Result<Option<AccountId>, Error> {
        self.accounts.find_account_for_transparent_address(address)
    }

    pub(crate) fn mark_transparent_output_spent(
        &mut self,
        spent_in_tx: &TxId,
        outpoint: &OutPoint,
    ) -> Result<bool, Error> {
        // TODO: Remove it from the search queue

        self.transparent_received_output_spends
            .insert(outpoint.clone(), *spent_in_tx);

        // TODO: Check if this is an update and therefore we need to add something to transparent_spend_map

        Ok(false)
    }

    #[cfg(feature = "transparent-inputs")]
    #[allow(unreachable_code, unused_variables)] //FIXME: need address key scope detection
    pub(crate) fn put_transparent_output(
        &mut self,
        output: &WalletTransparentOutput<AccountId>,
        receiving_account: &AccountId,
        known_unspent: bool,
    ) -> Result<OutPoint, Error> {
        use crate::types::transparent::ReceivedTransparentOutput;

        let address = output.recipient_address();
        // get the block height of the block that mined the output only if we have it in the block table
        // otherwise return None
        let block = output
            .mined_height()
            .and_then(|h| self.blocks.get(&h).map(|b| b.height));
        let txid = TxId::from_bytes(output.outpoint().hash().to_vec().try_into().unwrap());

        // insert a new tx into the transactions table for the one that spent this output. If there is already one then do an update
        self.tx_table
            .put_tx_partial(&txid, &block, output.mined_height());

        // look for a spent_height for this output by querying transparent_received_output_spends.
        // If there isn't one then return None (this is an unspent output)
        // otherwise return the height found by joining on the tx table
        let spent_height = self
            .transparent_received_output_spends
            .get(output.outpoint())
            .and_then(|txid| {
                self.tx_table
                    .tx_status(txid)
                    .and_then(|status| match status {
                        TransactionStatus::Mined(height) => Some(height),
                        _ => None,
                    })
            });

        // The max observed unspent height is either the spending transaction's mined height - 1, or
        // the current chain tip height if the UTXO was received via a path that confirmed that it was
        // unspent, such as by querying the UTXO set of the network.
        let max_observed_unspent = match spent_height {
            Some(h) => Some(h - 1),
            None => {
                if known_unspent {
                    self.chain_height()?
                } else {
                    None
                }
            }
        };

        // insert into transparent_received_outputs table. Update if it exists
        #[allow(clippy::diverging_sub_expression)] // FIXME
        match self
            .transparent_received_outputs
            .entry(output.outpoint().clone())
        {
            Entry::Occupied(mut entry) => {
                entry.get_mut().transaction_id = txid;
                entry.get_mut().address = *address;
                entry.get_mut().account_id = *receiving_account;
                entry.get_mut().txout = output.txout().clone();
            }
            Entry::Vacant(entry) => {
                // Look up the key scope for this address by checking ephemeral,
                // unified, and legacy transparent addresses in the account
                let key_scope = self
                    .accounts
                    .get(*receiving_account)
                    .and_then(|account| account.find_key_scope_for_transparent_address(address))
                    .unwrap_or(::transparent::keys::TransparentKeyScope::EXTERNAL);

                entry.insert(ReceivedTransparentOutput::new(
                    txid,
                    *receiving_account,
                    *address,
                    key_scope,
                    output.txout().clone(),
                    max_observed_unspent.unwrap_or(BlockHeight::from(0)),
                ));
            }
        }

        // look in transparent_spend_map for a record of the output already having been spent, then mark it as spent using the
        // stored reference to the spending transaction.
        if self
            .transparent_spend_map
            .contains(&txid, output.outpoint())
        {
            self.mark_transparent_output_spent(&txid, output.outpoint())?;
        }

        Ok(output.outpoint().clone())
    }

    #[cfg(feature = "transparent-inputs")]
    pub(crate) fn get_known_ephemeral_addresses(
        &self,
        account_id: AccountId,
        index_range: Option<Range<NonHardenedChildIndex>>,
    ) -> Result<Vec<(TransparentAddress, TransparentAddressMetadata)>, Error> {
        Ok(self
            .accounts
            .get(account_id)
            .map(Account::ephemeral_addresses)
            .unwrap_or_else(|| Ok(vec![]))?
            .into_iter()
            .filter(|(_addr, meta)| {
                index_range
                    .as_ref()
                    .is_none_or(|range| meta.address_index().is_some_and(|i| range.contains(&i)))
            })
            .collect::<Vec<_>>())
    }

    // Public accessor methods for transaction history feature

    /// Returns a reference to the received notes table for transaction history queries
    pub fn received_notes(&self) -> &ReceivedNoteTable {
        &self.received_notes
    }

    /// Returns a reference to the sent notes table for transaction history queries
    pub fn sent_notes(&self) -> &SentNoteTable {
        &self.sent_notes
    }

    /// Returns a reference to the transaction table for transaction history queries
    pub fn tx_table(&self) -> &TransactionTable {
        &self.tx_table
    }

    /// Iterates over the wallet's spend records: each received note that is known to be spent,
    /// paired with the transaction that spends it. Spends are recorded both when the wallet
    /// builds a transaction and when scanning detects one, so this covers transactions created
    /// elsewhere with the same keys.
    pub fn received_note_spends(&self) -> impl Iterator<Item = (&NoteId, &TxId)> + '_ {
        self.received_note_spends.0.iter()
    }

    /// Returns the block_time for a given block height, if the block is in the wallet's cache
    pub fn get_block_time(&self, height: BlockHeight) -> Option<u32> {
        self.blocks.get(&height).map(|block| block.block_time)
    }

    // Sync-state accessors, for applications that drive scanning and persistence themselves.

    /// The lowest birthday height over all accounts, or `None` if the wallet has no accounts.
    /// No block below it needs to be scanned.
    pub fn wallet_birthday(&self) -> Option<BlockHeight> {
        self.accounts
            .values()
            .map(|account| account.birthday().height())
            .min()
    }

    /// The height up to which every block from the wallet birthday has been scanned, or `None`
    /// if there is no such contiguous range yet (no accounts, nothing scanned, or a gap right
    /// after the birthday). Notes below this height are complete; nothing above it is
    /// guaranteed to be.
    pub fn fully_scanned_height(&self) -> Result<Option<BlockHeight>, Error> {
        Ok(self.block_fully_scanned()?.map(|meta| meta.block_height()))
    }

    /// The highest block height that has been scanned, or `None` if no block has been. Unlike
    /// [`Self::fully_scanned_height`] this says nothing about the blocks below it.
    pub fn max_scanned_height(&self) -> Option<BlockHeight> {
        self.blocks.keys().max().copied()
    }

    /// The chain tip height the wallet was last told about through `update_chain_tip`, or
    /// `None` before the first update.
    pub fn chain_tip_height(&self) -> Result<Option<BlockHeight>, Error> {
        self.chain_height()
    }

    /// The identity of every account in the wallet, in account id order.
    pub fn account_identities(&self) -> Vec<AccountIdentity> {
        self.accounts
            .values()
            .map(|account| {
                let derivation = match account.kind() {
                    AccountSource::Derived { derivation, .. } => Some(derivation),
                    AccountSource::Imported {
                        purpose: AccountPurpose::Spending { derivation },
                        ..
                    } => derivation.as_ref(),
                    AccountSource::Imported {
                        purpose: AccountPurpose::ViewOnly,
                        ..
                    } => None,
                };
                AccountIdentity {
                    account_id: account.account_id(),
                    ufvk: account.viewing_key().encode(&self.params),
                    seed_fingerprint: derivation.map(|d| d.seed_fingerprint().to_bytes()),
                    hd_index: derivation.map(|d| u32::from(d.account_index())),
                    birthday_height: account.birthday().height(),
                }
            })
            .collect()
    }
}

/// Returns the greatest checkpoint height in `store` whose block has at least
/// `min_confirmations` confirmations at `chain_tip_height`, or `None` if the tree has no such
/// checkpoint.
///
/// Only the tree's checkpoints are visited, of which there are at most `max_checkpoints`. Probing
/// every block height downwards from the tip would walk the whole chain whenever the tree has no
/// checkpoints, which is the state of the Orchard tree before NU5 and of the Ironwood tree before
/// NU6.3 or in a wallet restored from a snapshot taken before Ironwood support.
fn max_checkpointed_height<H: Clone>(
    store: &MemoryShardStore<H, BlockHeight>,
    chain_tip_height: BlockHeight,
    min_confirmations: NonZeroU32,
) -> Result<Option<BlockHeight>, Error> {
    let max_checkpoint_height = BlockHeight::from_u32(
        u32::from(chain_tip_height).saturating_sub(u32::from(min_confirmations) - 1),
    );
    let mut result: Option<BlockHeight> = None;
    store.for_each_checkpoint(store.checkpoint_count()?, |height, _| {
        if *height <= max_checkpoint_height {
            result = Some(result.map_or(*height, |r| r.max(*height)));
        }
        Ok(())
    })?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PRUNING_DEPTH;
    use zcash_client_backend::data_api::WalletWrite;
    use zcash_protocol::consensus::Network;

    fn sapling_nf(byte: u8) -> Nullifier {
        Nullifier::Sapling(sapling::Nullifier([byte; 32]))
    }

    #[test]
    fn empty_wallet_has_no_sync_state() {
        let db = MemoryWalletDb::new(Network::TestNetwork, 100);
        assert_eq!(db.wallet_birthday(), None);
        assert_eq!(db.fully_scanned_height().unwrap(), None);
        assert_eq!(db.max_scanned_height(), None);
        assert_eq!(db.chain_tip_height().unwrap(), None);
        assert!(db.account_identities().is_empty());
    }

    #[test]
    fn wallet_summary_without_accounts_is_none_even_after_a_chain_tip() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        assert!(
            db.get_wallet_summary(ConfirmationsPolicy::MIN)
                .unwrap()
                .is_none()
        );

        // An empty wallet still records the chain tip (as an ignored range), which used to make
        // the summary look up a wallet birthday that does not exist.
        db.update_chain_tip(BlockHeight::from(1_000_000)).unwrap();
        assert_eq!(
            db.chain_tip_height().unwrap(),
            Some(BlockHeight::from(1_000_000))
        );
        assert!(
            db.get_wallet_summary(ConfirmationsPolicy::MIN)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mutate_marks_the_wallet_dirty_only_while_an_operation_runs() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        assert_eq!(db.is_dirty(), None);

        let seen = db
            .mutate("op", |db| {
                assert_eq!(db.is_dirty(), Some("op"));
                Ok(7)
            })
            .unwrap();
        assert_eq!(seen, 7);
        assert_eq!(db.is_dirty(), None);
    }

    #[test]
    fn a_failed_mutation_leaves_the_wallet_dirty_and_refuses_further_writes() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        assert!(matches!(
            db.mutate("first", |_| Err::<(), _>(Error::Other("boom".into()))),
            Err(Error::Other(_))
        ));
        assert_eq!(db.is_dirty(), Some("first"));

        // The next operation is refused before it runs, naming the one that failed.
        let mut ran = false;
        assert!(matches!(
            db.mutate("second", |_| {
                ran = true;
                Ok(())
            }),
            Err(Error::DirtyState("first"))
        ));
        assert!(!ran);
        assert_eq!(db.is_dirty(), Some("first"));
    }

    #[test]
    fn a_dirty_wallet_refuses_to_encode() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        let mut buf = vec![];
        db.encode(&mut buf).unwrap();
        assert!(!buf.is_empty());

        let _ = db.mutate("put_blocks", |_| Err::<(), _>(Error::NonSequentialBlocks));
        let mut buf = vec![];
        assert!(matches!(
            db.encode(&mut buf),
            Err(Error::DirtyState("put_blocks"))
        ));
        assert!(buf.is_empty());
        assert!(matches!(
            db.encode_to_vec(),
            Err(Error::DirtyState("put_blocks"))
        ));
    }

    fn txid(byte: u8) -> TxId {
        TxId::from_bytes([byte; 32])
    }

    /// Inserts one Sapling nullifier (and its transaction locator) per `(height, byte)` pair.
    fn insert_sapling_entries(db: &mut MemoryWalletDb<Network>, entries: &[(u32, u8)]) {
        for (height, byte) in entries {
            db.insert_sapling_nullifier_map(
                (*height).into(),
                &[(
                    TxIndex::from(*byte as u16),
                    txid(*byte),
                    vec![sapling::Nullifier([*byte; 32])],
                )],
            )
            .unwrap();
        }
    }

    fn locator_heights(db: &MemoryWalletDb<Network>) -> Vec<u32> {
        db.tx_locator.0.keys().map(|(h, _)| u32::from(*h)).collect()
    }

    #[test]
    fn prune_nullifier_map_below_keeps_entries_at_and_above_cutoff() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        insert_sapling_entries(&mut db, &[(100, 1), (199, 2), (200, 3), (201, 4), (500, 5)]);
        assert_eq!(db.nullifiers.0.len(), 5);
        assert_eq!(db.tx_locator.0.len(), 5);

        db.prune_nullifier_map_below(200.into());

        assert!(db.nullifiers.get(&sapling_nf(1)).is_none());
        assert!(db.nullifiers.get(&sapling_nf(2)).is_none());
        assert_eq!(db.nullifiers.get(&sapling_nf(3)), Some(&(200.into(), 3)));
        assert_eq!(db.nullifiers.get(&sapling_nf(4)), Some(&(201.into(), 4)));
        assert_eq!(db.nullifiers.get(&sapling_nf(5)), Some(&(500.into(), 5)));
        assert_eq!(locator_heights(&db), vec![200, 201, 500]);
    }

    #[test]
    fn prune_nullifier_map_below_is_idempotent() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        insert_sapling_entries(&mut db, &[(100, 1), (300, 2)]);

        db.prune_nullifier_map_below(200.into());
        let after_first = (db.nullifiers.clone(), db.tx_locator.clone());
        db.prune_nullifier_map_below(200.into());

        assert_eq!((db.nullifiers.clone(), db.tx_locator.clone()), after_first);
        assert_eq!(locator_heights(&db), vec![300]);
    }

    #[test]
    fn prune_nullifier_map_below_zero_keeps_everything() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        insert_sapling_entries(&mut db, &[(0, 1), (100, 2)]);

        db.prune_nullifier_map_below(0.into());

        assert_eq!(db.nullifiers.0.len(), 2);
        assert_eq!(locator_heights(&db), vec![0, 100]);
    }

    #[test]
    #[cfg(feature = "orchard")]
    fn prune_nullifier_map_below_prunes_orchard_entries() {
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        let orchard_nf = |byte: u8| {
            // `CtOption::unwrap` panics if the bytes are not a canonical field element.
            orchard::note::Nullifier::from_bytes(&[byte; 32]).unwrap()
        };
        for (height, byte) in [(100u32, 1u8), (200, 2)] {
            db.insert_orchard_nullifier_map(
                height.into(),
                &[(
                    TxIndex::from(byte as u16),
                    txid(byte),
                    vec![orchard_nf(byte)],
                )],
            )
            .unwrap();
        }

        db.prune_nullifier_map_below(200.into());

        assert!(
            db.nullifiers
                .get(&Nullifier::Orchard(orchard_nf(1)))
                .is_none()
        );
        assert_eq!(
            db.nullifiers.get(&Nullifier::Orchard(orchard_nf(2))),
            Some(&(200.into(), 2))
        );
        assert_eq!(locator_heights(&db), vec![200]);
    }

    #[test]
    fn prune_tracked_nullifiers_is_a_noop_without_a_fully_scanned_height() {
        // A wallet with no accounts has no birthday, hence no fully scanned height, so there is
        // no safe cutoff and nothing may be pruned.
        let mut db = MemoryWalletDb::new(Network::TestNetwork, 100);
        insert_sapling_entries(&mut db, &[(0, 1), (1_000, 2), (1_000_000, 3)]);
        assert!(db.block_fully_scanned().unwrap().is_none());

        db.prune_tracked_nullifiers(PRUNING_DEPTH).unwrap();

        assert_eq!(db.nullifiers.0.len(), 3);
        assert_eq!(locator_heights(&db), vec![0, 1_000, 1_000_000]);
    }
}
