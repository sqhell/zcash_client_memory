use bytes::{Buf, BufMut};
use consensus::Parameters;
use prost::Message;

use super::envelope::{
    BODY_VERSION_V1, BODY_VERSION_V2, HashingBuf, MAGIC, network_name, read_header,
};
use super::*;
use crate::error::Result;
use crate::proto::memwallet as proto;
use crate::read_optional;
use crate::wallet_commitment_trees::serialization::{tree_from_protobuf, tree_to_protobuf};

impl<P: Parameters> MemoryWalletDb<P> {
    /// Encodes the wallet as a bare format v1 body: a protobuf `MemoryWallet` message with no
    /// header. [`Self::encode_with_meta`] is the persistence entry point; this is the body
    /// writer it builds on, kept for the tests and the harness that compare wire forms.
    ///
    /// Fails if any part of the wallet cannot be serialized, or with [`Error::DirtyState`] if a
    /// write operation failed part-way (see [`MemoryWalletDb::is_dirty`]); it never writes a
    /// partial or inconsistent blob.
    pub fn encode<B: BufMut>(&self, buf: &mut B) -> Result<()> {
        self.check_clean()?;
        let proto_wallet = proto::MemoryWallet::try_from(self)?;
        proto_wallet.encode(buf)?;
        Ok(())
    }

    /// Encodes the wallet as a bare format v1 body (see [`Self::encode`]) into a new byte
    /// vector sized up front from the encoded length, so a large wallet is written in one
    /// allocation instead of growing the buffer as it goes.
    ///
    /// Fails in the same cases as [`Self::encode`].
    pub fn encode_to_vec(&self) -> Result<Vec<u8>> {
        self.check_clean()?;
        let proto_wallet = proto::MemoryWallet::try_from(self)?;
        Ok(proto_wallet.encode_to_vec())
    }

    /// Decodes a wallet from a blob written by [`Self::encode_with_meta`] (format v2, an
    /// envelope with a header) or by [`Self::encode`] (format v1, a bare body), telling them
    /// apart by the first four bytes. The decoded wallet reports which through
    /// [`Self::loaded_format_version`]; a v2 blob also restores the wallet's [`Lineage`].
    ///
    /// For a v2 blob the header is checked before the body is trusted: the schema version must
    /// be 2, the network must be the one `params` describes (else [`Error::NetworkMismatch`]),
    /// the body must be exactly as long as the header says, and it must hash to the header's
    /// digest (else [`Error::ChecksumMismatch`], which takes precedence over any decoding error
    /// the corrupted body produced). A truncated envelope fails with [`Error::CorruptedData`].
    ///
    /// A body of any version other than the one its format calls for fails with
    /// [`Error::UnsupportedProtoVersion`].
    pub fn decode_new<B: Buf>(mut buf: B, params: P, max_checkpoints: usize) -> Result<Self> {
        let mut prefix = [0u8; MAGIC.len()];
        let sniffed = buf.remaining().min(prefix.len());
        buf.copy_to_slice(&mut prefix[..sniffed]);

        if prefix[..sniffed] != MAGIC {
            // A bare v1 body: hand the sniffed bytes back ahead of the rest.
            let proto_wallet = proto::MemoryWallet::decode((&prefix[..sniffed]).chain(buf))?;
            return Self::new_from_body(proto_wallet, BODY_VERSION_V1, params, max_checkpoints);
        }

        let meta = read_header(&mut buf)?;
        let expected = network_name(params.network_type());
        if meta.network() != expected {
            return Err(Error::NetworkMismatch {
                expected: expected.to_owned(),
                found: meta.network().to_owned(),
            });
        }
        let (body_sha256, body_len) = match (meta.body_sha256(), meta.body_len()) {
            (Some(digest), Some(len)) => (digest, len),
            _ => {
                return Err(Error::CorruptedData(
                    "wallet blob header carries no body digest".into(),
                ));
            }
        };
        if u64::try_from(buf.remaining())? != body_len {
            return Err(Error::CorruptedData(format!(
                "wallet blob header says the body is {body_len} bytes but {} follow it",
                buf.remaining()
            )));
        }

        // Hash the body in the pass that decodes it. If the hash does not match, the body was
        // corrupted and any decoding error is a symptom of that, not the diagnosis.
        let mut body = HashingBuf::new(buf);
        let decoded = proto::MemoryWallet::decode(&mut body);
        if body.finish() != body_sha256 {
            return Err(Error::ChecksumMismatch);
        }

        let mut wallet = Self::new_from_body(decoded?, BODY_VERSION_V2, params, max_checkpoints)?;
        wallet.lineage = meta.lineage().clone();
        Ok(wallet)
    }

    /// Builds a wallet from an already decoded bare (format v1) body.
    ///
    /// Fails with [`Error::UnsupportedProtoVersion`] unless the body's version is 1.
    pub fn new_from_proto(
        proto_wallet: proto::MemoryWallet,
        params: P,
        max_checkpoints: usize,
    ) -> Result<Self> {
        Self::new_from_body(proto_wallet, BODY_VERSION_V1, params, max_checkpoints)
    }

    /// Builds a wallet from a decoded body whose version must be `version`: 1 for a bare body,
    /// 2 for the body of an envelope. The two share a layout; the version records which
    /// format the body was written in, and the wallet reports it as its loaded format version.
    fn new_from_body(
        proto_wallet: proto::MemoryWallet,
        version: u32,
        params: P,
        max_checkpoints: usize,
    ) -> Result<Self> {
        if proto_wallet.version != version {
            return Err(Error::UnsupportedProtoVersion(
                version,
                proto_wallet.version,
            ));
        }

        let mut wallet = MemoryWalletDb::new(params, max_checkpoints);
        wallet.loaded_format_version = Some(version);

        wallet.accounts = {
            let proto_accounts = read_optional!(proto_wallet, accounts)?;
            let accounts = proto_accounts
                .accounts
                .into_iter()
                .map(|proto_account| {
                    let id = proto_account.account_id;
                    let account = Account::try_from(proto_account)?;
                    Ok((AccountId::from(id), account))
                })
                .collect::<Result<_>>()?;
            Ok::<Accounts, Error>(Accounts {
                accounts,
                nonce: proto_accounts.account_nonce,
            })
        }?;

        wallet.blocks = proto_wallet
            .blocks
            .into_iter()
            .map(|proto_block| {
                Ok((
                    proto_block.height.into(),
                    MemoryWalletBlock::try_from(proto_block)?,
                ))
            })
            .collect::<Result<_>>()?;

        wallet.tx_table = TransactionTable(
            proto_wallet
                .tx_table
                .into_iter()
                .map(|proto_tx| {
                    let txid = read_optional!(proto_tx, tx_id)?;
                    let tx = read_optional!(proto_tx, tx_entry)?;
                    Ok((txid.try_into()?, tx.try_into()?))
                })
                .collect::<Result<_>>()?,
        );

        wallet.received_notes = ReceivedNoteTable::from_notes(
            proto_wallet
                .received_note_table
                .into_iter()
                .map(ReceivedNote::try_from)
                .collect::<Result<_>>()?,
        );

        wallet.received_note_spends = ReceievedNoteSpends(
            proto_wallet
                .received_note_spends
                .into_iter()
                .map(|proto_spend| {
                    let note_id = read_optional!(proto_spend, note_id)?;
                    let tx_id = read_optional!(proto_spend, tx_id)?;
                    Ok((note_id.try_into()?, tx_id.try_into()?))
                })
                .collect::<Result<_>>()?,
        );

        wallet.nullifiers = NullifierMap(
            proto_wallet
                .nullifiers
                .into_iter()
                .map(|proto_nullifier| {
                    let block_height = proto_nullifier.block_height.into();
                    let tx_index = proto_nullifier.tx_index;
                    let nullifier = read_optional!(proto_nullifier, nullifier)?.try_into()?;
                    Ok((nullifier, (block_height, tx_index)))
                })
                .collect::<Result<_>>()?,
        );

        wallet.sent_notes = SentNoteTable(
            proto_wallet
                .sent_notes
                .into_iter()
                .map(|proto_sent_note| {
                    let sent_note_id = read_optional!(proto_sent_note, sent_note_id)?;
                    let sent_note = read_optional!(proto_sent_note, sent_note)?;
                    Ok((sent_note_id.try_into()?, SentNote::try_from(sent_note)?))
                })
                .collect::<Result<_>>()?,
        );

        wallet.tx_locator = TxLocatorMap(
            proto_wallet
                .tx_locator
                .into_iter()
                .map(|proto_locator| {
                    let block_height = proto_locator.block_height.into();
                    let tx_index = proto_locator.tx_index;
                    let tx_id = read_optional!(proto_locator, tx_id)?.try_into()?;
                    Ok(((block_height, tx_index), tx_id))
                })
                .collect::<Result<_>>()?,
        );

        wallet.scan_queue = ScanQueue(
            proto_wallet
                .scan_queue
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_>>()?,
        );

        wallet.sapling_tree =
            tree_from_protobuf(read_optional!(proto_wallet, sapling_tree)?, 100, 16.into())?;

        wallet.sapling_tree_shard_end_heights = proto_wallet
            .sapling_tree_shard_end_heights
            .into_iter()
            .map(|proto_end_height| {
                let address = Address::from_parts(
                    Level::from(u8::try_from(proto_end_height.level)?),
                    proto_end_height.index,
                );
                let height = proto_end_height.block_height.into();
                Ok((address, height))
            })
            .collect::<Result<_>>()?;

        #[cfg(feature = "orchard")]
        {
            wallet.orchard_tree =
                tree_from_protobuf(read_optional!(proto_wallet, orchard_tree)?, 100, 16.into())?;
        };

        #[cfg(feature = "orchard")]
        {
            wallet.orchard_tree_shard_end_heights = proto_wallet
                .orchard_tree_shard_end_heights
                .into_iter()
                .map(|proto_end_height| {
                    let address = Address::from_parts(
                        Level::from(u8::try_from(proto_end_height.level)?),
                        proto_end_height.index,
                    );
                    let height = proto_end_height.block_height.into();
                    Ok((address, height))
                })
                .collect::<Result<_>>()?;
        };

        // Wallets saved before Ironwood support have no Ironwood tree; they keep the empty tree
        // `MemoryWalletDb::new` created, and scanning fills it from the next scanned block.
        #[cfg(feature = "orchard")]
        if let Some(proto_tree) = proto_wallet.ironwood_tree {
            wallet.ironwood_tree = tree_from_protobuf(proto_tree, 100, 16.into())?;
        }

        #[cfg(feature = "orchard")]
        {
            wallet.ironwood_tree_shard_end_heights = proto_wallet
                .ironwood_tree_shard_end_heights
                .into_iter()
                .map(|proto_end_height| {
                    let address = Address::from_parts(
                        Level::from(u8::try_from(proto_end_height.level)?),
                        proto_end_height.index,
                    );
                    let height = proto_end_height.block_height.into();
                    Ok((address, height))
                })
                .collect::<Result<_>>()?;
        };

        wallet.transparent_received_outputs = TransparentReceivedOutputs(
            proto_wallet
                .transparent_received_outputs
                .into_iter()
                .map(|proto_output| {
                    let outpoint = read_optional!(proto_output, outpoint)?;
                    let output = read_optional!(proto_output, output)?.try_into()?;
                    Ok((OutPoint::try_from(outpoint)?, output))
                })
                .collect::<Result<_>>()?,
        );

        wallet.transparent_received_output_spends = TransparentReceivedOutputSpends(
            proto_wallet
                .transparent_received_output_spends
                .into_iter()
                .map(|proto_spend| {
                    let outpoint = read_optional!(proto_spend, outpoint)?;
                    let txid = read_optional!(proto_spend, tx_id)?.try_into()?;
                    Ok((OutPoint::try_from(outpoint)?, txid))
                })
                .collect::<Result<_>>()?,
        );

        wallet.transparent_spend_map = TransparentSpendCache(
            proto_wallet
                .transparent_spend_map
                .into_iter()
                .map(|proto_spend| {
                    let txid = read_optional!(proto_spend, tx_id)?.try_into()?;
                    let outpoint = read_optional!(proto_spend, outpoint)?;
                    Ok((txid, OutPoint::try_from(outpoint)?))
                })
                .collect::<Result<_>>()?,
        );

        wallet.transaction_data_request_queue = TransactionDataRequestQueue(
            proto_wallet
                .transaction_data_requests
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_>>()?,
        );

        // A wallet saved before block pruning carries a record for every scanned block and no
        // scan counters. Compute the counters from that complete block set and drop the records
        // that are no longer needed, once, here; a wallet saved since is decoded as it was.
        match proto_wallet.scan_counters {
            Some(counters) => wallet.scan_counters = Some(ScanCounters::try_from(counters)?),
            None => {
                wallet.prune_blocks()?;
            }
        }

        Ok(wallet)
    }
}

impl From<&TxId> for proto::TxId {
    fn from(txid: &TxId) -> Self {
        proto::TxId {
            hash: txid.as_ref().to_vec(),
        }
    }
}

impl From<TxId> for proto::TxId {
    fn from(txid: TxId) -> Self {
        proto::TxId {
            hash: txid.as_ref().to_vec(),
        }
    }
}

impl TryFrom<proto::TxId> for TxId {
    type Error = Error;

    fn try_from(txid: proto::TxId) -> Result<Self> {
        Ok(TxId::from_bytes(txid.hash.try_into()?))
    }
}

/// Serializes the shard-end-height map of one note commitment tree.
fn shard_end_heights_to_proto(
    end_heights: &BTreeMap<Address, BlockHeight>,
) -> Vec<proto::TreeEndHeightsRecord> {
    end_heights
        .iter()
        .map(|(address, height)| proto::TreeEndHeightsRecord {
            level: address.level().into(),
            index: address.index(),
            block_height: (*height).into(),
        })
        .collect()
}

impl<P: Parameters> TryFrom<&MemoryWalletDb<P>> for proto::MemoryWallet {
    type Error = Error;

    /// Builds the wire representation of the wallet by borrowing every table; nothing is cloned
    /// except the bytes that end up in the message.
    ///
    /// Fails if a note commitment tree or an account birthday cannot be serialized. Encoding used
    /// to drop such a tree silently, which produced a blob that could never be decoded.
    fn try_from(wallet: &MemoryWalletDb<P>) -> Result<Self> {
        Ok(Self {
            version: BODY_VERSION_V1,
            accounts: Some(proto::Accounts::try_from(&wallet.accounts)?),

            blocks: wallet
                .blocks
                .values()
                .map(proto::WalletBlock::from)
                .collect(),

            tx_table: wallet
                .tx_table
                .0
                .iter()
                .map(|(txid, tx)| proto::TransactionTableRecord {
                    tx_id: Some(txid.into()),
                    tx_entry: Some(tx.into()),
                })
                .collect(),

            received_note_table: wallet
                .received_notes
                .iter()
                .map(proto::ReceivedNote::from)
                .collect(),

            received_note_spends: wallet
                .received_note_spends
                .0
                .iter()
                .map(|(note_id, tx_id)| proto::ReceivedNoteSpendRecord {
                    note_id: Some((*note_id).into()),
                    tx_id: Some(tx_id.into()),
                })
                .collect(),

            nullifiers: wallet
                .nullifiers
                .0
                .iter()
                .map(|(nullifier, (height, tx_index))| proto::NullifierRecord {
                    block_height: (*height).into(),
                    tx_index: *tx_index,
                    nullifier: Some((*nullifier).into()),
                })
                .collect(),

            sent_notes: wallet
                .sent_notes
                .0
                .iter()
                .map(|(id, note)| proto::SentNoteRecord {
                    sent_note_id: Some(id.into()),
                    sent_note: Some(note.into()),
                })
                .collect(),

            tx_locator: wallet
                .tx_locator
                .0
                .iter()
                .map(|((height, tx_index), txid)| proto::TxLocatorRecord {
                    block_height: (*height).into(),
                    tx_index: *tx_index,
                    tx_id: Some(txid.into()),
                })
                .collect(),

            scan_queue: wallet
                .scan_queue
                .iter()
                .map(|r| proto::ScanQueueRecord::from(*r))
                .collect(),

            sapling_tree: tree_to_protobuf(&wallet.sapling_tree)?,
            sapling_tree_shard_end_heights: shard_end_heights_to_proto(
                &wallet.sapling_tree_shard_end_heights,
            ),

            #[cfg(feature = "orchard")]
            orchard_tree: tree_to_protobuf(&wallet.orchard_tree)?,
            #[cfg(not(feature = "orchard"))]
            orchard_tree: None,

            #[cfg(feature = "orchard")]
            orchard_tree_shard_end_heights: shard_end_heights_to_proto(
                &wallet.orchard_tree_shard_end_heights,
            ),
            #[cfg(not(feature = "orchard"))]
            orchard_tree_shard_end_heights: Vec::new(),

            #[cfg(feature = "orchard")]
            ironwood_tree: tree_to_protobuf(&wallet.ironwood_tree)?,
            #[cfg(not(feature = "orchard"))]
            ironwood_tree: None,

            #[cfg(feature = "orchard")]
            ironwood_tree_shard_end_heights: shard_end_heights_to_proto(
                &wallet.ironwood_tree_shard_end_heights,
            ),
            #[cfg(not(feature = "orchard"))]
            ironwood_tree_shard_end_heights: Vec::new(),

            transparent_received_outputs: wallet
                .transparent_received_outputs
                .0
                .iter()
                .map(
                    |(outpoint, output)| proto::TransparentReceivedOutputRecord {
                        outpoint: Some(outpoint.into()),
                        output: Some(output.into()),
                    },
                )
                .collect(),

            transparent_received_output_spends: wallet
                .transparent_received_output_spends
                .0
                .iter()
                .map(
                    |(outpoint, txid)| proto::TransparentReceivedOutputSpendRecord {
                        outpoint: Some(outpoint.into()),
                        tx_id: Some(txid.into()),
                    },
                )
                .collect(),

            transparent_spend_map: wallet
                .transparent_spend_map
                .0
                .iter()
                .map(|(txid, outpoint)| proto::TransparentSpendCacheRecord {
                    tx_id: Some(txid.into()),
                    outpoint: Some(outpoint.into()),
                })
                .collect(),

            transaction_data_requests: wallet
                .transaction_data_request_queue
                .0
                .iter()
                .map(Into::into)
                .collect(),

            scan_counters: wallet.scan_counters.as_ref().map(proto::ScanCounters::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_protocol::consensus::Network;

    #[test]
    fn decode_rejects_unknown_scan_priority() {
        let wallet = MemoryWalletDb::new(Network::TestNetwork, 100);
        let mut proto_wallet = proto::MemoryWallet::try_from(&wallet).unwrap();
        proto_wallet.scan_queue.push(proto::ScanQueueRecord {
            start_height: 10,
            end_height: 20,
            priority: 99,
        });
        let bytes = proto_wallet.encode_to_vec();

        assert!(matches!(
            MemoryWalletDb::decode_new(&bytes[..], Network::TestNetwork, 100),
            Err(Error::UnknownEnumValue {
                field: "priority",
                value: 99
            })
        ));
    }
}
