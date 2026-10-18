//! The format v2 wallet blob: a [`WalletMeta`] header in an envelope ahead of the wallet body.
//!
//! A format v1 blob is a bare prost `MemoryWallet` message with `version = 1`. It says nothing
//! about itself: not the network it was saved for, not who wrote it or when, and nothing that
//! lets a reader check it is intact or compare it with another save short of decoding both in
//! full. Format v2 wraps the same body in an envelope:
//!
//! ```text
//! magic        b"ZMWD"                       (4 bytes)
//! header_len   u32, little endian            (4 bytes)
//! WalletMeta   prost message                 (header_len bytes)
//! MemoryWallet prost message, version = 2    (body_len bytes, per the header)
//! ```
//!
//! A v1 body always begins with the bytes `0x08 0x01` (field 1 as a varint with value 1), so
//! the magic cannot be mistaken for one and a reader can tell the two formats apart from the
//! first four bytes. [`MemoryWalletDb::decode_new`] reads both; [`peek_meta`] reads the
//! description of either without decoding the body.

use bytes::Buf;
use prost::Message;
use sha2::{Digest, Sha256};
use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_protocol::consensus::{BlockHeight, MainNetwork, NetworkType, Parameters};

use super::{Lineage, MemoryWalletDb};
use crate::error::{Error, Result};
use crate::proto::memwallet as proto;

/// The first four bytes of a format v2 blob.
pub(crate) const MAGIC: [u8; 4] = *b"ZMWD";

/// The blob format version an envelope with a [`WalletMeta`] header belongs to.
pub(crate) const SCHEMA_VERSION: u32 = 2;

/// The `MemoryWallet.version` of a bare body (format v1).
pub(crate) const BODY_VERSION_V1: u32 = 1;

/// The `MemoryWallet.version` of a body inside an envelope (format v2). The body layout is the
/// same as v1; the version says which format the body was written in, so that a body cannot be
/// moved between formats without the reader noticing.
pub(crate) const BODY_VERSION_V2: u32 = 2;

/// The size of the fixed part of the envelope: the magic and the header length.
const ENVELOPE_PREFIX_LEN: usize = MAGIC.len() + 4;

/// The name a network is written as in a [`WalletMeta`] header.
pub(crate) fn network_name(network: NetworkType) -> &'static str {
    match network {
        NetworkType::Main => "main",
        NetworkType::Test => "test",
        NetworkType::Regtest => "regtest",
    }
}

/// What a save needs from the application: the time of the save and who is making it. Both
/// are recorded in the blob's header and in the wallet's [`Lineage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeContext<'a> {
    /// The current Unix time in milliseconds.
    pub now_ms: u64,
    /// The identifier of the application instance making the save, so that two writers of the
    /// same wallet can be told apart afterwards.
    pub writer_id: &'a str,
}

/// The header of a format v2 blob: what the blob is, where it sits in its line of saves, and
/// how far the wallet it carries has been synced. Everything an application needs to
/// recognise, verify or compare a saved wallet without decoding its body.
///
/// Read from a blob with [`peek_meta`], or built from a live wallet with
/// [`MemoryWalletDb::meta`] (which leaves the body digest empty, since no body was written).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletMeta {
    schema_version: u32,
    network: String,
    lineage: Lineage,
    birthday_height: Option<BlockHeight>,
    fully_scanned_height: Option<BlockHeight>,
    max_scanned_height: Option<BlockHeight>,
    chain_tip_height: Option<BlockHeight>,
    account_count: u32,
    account_ufvks: Vec<String>,
    seed_fingerprint: Option<[u8; 32]>,
    body_sha256: Option<[u8; 32]>,
    body_len: Option<u64>,
    blocks_retained: u32,
}

impl WalletMeta {
    /// The blob format version this header belongs to: always 2.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// The network the wallet was saved for: `"main"`, `"test"` or `"regtest"`.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// Where the blob sits in its line of saves.
    pub fn lineage(&self) -> &Lineage {
        &self.lineage
    }

    /// The lowest account birthday height, or `None` when the wallet has no accounts.
    pub fn birthday_height(&self) -> Option<BlockHeight> {
        self.birthday_height
    }

    /// The height up to which every block from the birthday had been scanned when the blob was
    /// written, when known (see [`MemoryWalletDb::fully_scanned_height`]).
    pub fn fully_scanned_height(&self) -> Option<BlockHeight> {
        self.fully_scanned_height
    }

    /// The highest scanned block height, or `None` when no block had been scanned.
    pub fn max_scanned_height(&self) -> Option<BlockHeight> {
        self.max_scanned_height
    }

    /// The chain tip height the wallet had last been told about, or `None` before the first
    /// chain tip update.
    pub fn chain_tip_height(&self) -> Option<BlockHeight> {
        self.chain_tip_height
    }

    /// The number of accounts in the wallet.
    pub fn account_count(&self) -> u32 {
        self.account_count
    }

    /// The encoded unified full viewing key of every account, in account id order.
    pub fn account_ufvks(&self) -> &[String] {
        &self.account_ufvks
    }

    /// The ZIP 32 fingerprint of the seed account 0 was derived from, when known.
    pub fn seed_fingerprint(&self) -> Option<[u8; 32]> {
        self.seed_fingerprint
    }

    /// SHA-256 of the body bytes, or `None` for a header built without a body
    /// ([`MemoryWalletDb::meta`]).
    pub fn body_sha256(&self) -> Option<[u8; 32]> {
        self.body_sha256
    }

    /// The length of the body in bytes, or `None` for a header built without a body
    /// ([`MemoryWalletDb::meta`]).
    pub fn body_len(&self) -> Option<u64> {
        self.body_len
    }

    /// The number of block records the body carries.
    pub fn blocks_retained(&self) -> u32 {
        self.blocks_retained
    }

    fn to_proto(&self) -> proto::WalletMeta {
        proto::WalletMeta {
            schema_version: self.schema_version,
            network: self.network.clone(),
            created_at_ms: self.lineage.created_at_ms(),
            saved_at_ms: self.lineage.saved_at_ms(),
            writer_id: self.lineage.writer_id().to_owned(),
            save_seq: self.lineage.save_seq(),
            birthday_height: self.birthday_height.map(u32::from),
            fully_scanned_height: self.fully_scanned_height.map(u32::from),
            max_scanned_height: self.max_scanned_height.map(u32::from),
            chain_tip_height: self.chain_tip_height.map(u32::from),
            account_count: self.account_count,
            account_ufvks: self.account_ufvks.clone(),
            seed_fingerprint: self.seed_fingerprint.map(|fp| fp.to_vec()),
            body_sha256: self.body_sha256.map(|d| d.to_vec()).unwrap_or_default(),
            body_len: self.body_len.unwrap_or(0),
            blocks_retained: self.blocks_retained,
        }
    }
}

impl TryFrom<proto::WalletMeta> for WalletMeta {
    type Error = Error;

    /// Fails with [`Error::UnsupportedProtoVersion`] for a header of another schema version and
    /// with [`Error::CorruptedData`] for a digest or seed fingerprint of the wrong length.
    fn try_from(meta: proto::WalletMeta) -> Result<Self> {
        if meta.schema_version != SCHEMA_VERSION {
            return Err(Error::UnsupportedProtoVersion(
                SCHEMA_VERSION,
                meta.schema_version,
            ));
        }
        let seed_fingerprint = meta
            .seed_fingerprint
            .map(|fp| {
                <[u8; 32]>::try_from(fp).map_err(|_| {
                    Error::CorruptedData("header seed fingerprint is not 32 bytes".into())
                })
            })
            .transpose()?;
        let body_sha256 =
            if meta.body_sha256.is_empty() {
                None
            } else {
                Some(<[u8; 32]>::try_from(meta.body_sha256).map_err(|_| {
                    Error::CorruptedData("header body digest is not 32 bytes".into())
                })?)
            };
        Ok(Self {
            schema_version: meta.schema_version,
            network: meta.network,
            lineage: Lineage::from_parts(
                meta.created_at_ms,
                meta.saved_at_ms,
                meta.writer_id,
                meta.save_seq,
            ),
            birthday_height: meta.birthday_height.map(BlockHeight::from),
            fully_scanned_height: meta.fully_scanned_height.map(BlockHeight::from),
            max_scanned_height: meta.max_scanned_height.map(BlockHeight::from),
            chain_tip_height: meta.chain_tip_height.map(BlockHeight::from),
            account_count: meta.account_count,
            account_ufvks: meta.account_ufvks,
            seed_fingerprint,
            body_sha256,
            body_len: (meta.body_len != 0).then_some(meta.body_len),
            blocks_retained: meta.blocks_retained,
        })
    }
}

/// What can be read about a wallet from a format v1 blob without decoding its body: the fields
/// of the body's `accounts` and `scan_queue` messages, which are a fixed cost per account and
/// per scan range, while the blocks, notes and trees are skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyMeta {
    birthday_height: Option<BlockHeight>,
    account_ufvks: Vec<String>,
    chain_tip_height: Option<BlockHeight>,
}

impl LegacyMeta {
    /// The lowest account birthday height, or `None` when the wallet has no accounts.
    pub fn birthday_height(&self) -> Option<BlockHeight> {
        self.birthday_height
    }

    /// The unified full viewing key of every account, in account id order, as the body
    /// stores them: encoded for mainnet whatever network the wallet is for, since a v1 blob
    /// does not say which. [`Self::account_ufvks_for`] re-encodes them for a known network,
    /// which is what a [`WalletMeta`] header and [`AccountIdentity::ufvk`] report.
    ///
    /// [`AccountIdentity::ufvk`]: super::AccountIdentity::ufvk
    pub fn account_ufvks(&self) -> &[String] {
        &self.account_ufvks
    }

    /// The unified full viewing key of every account, in account id order, encoded for the
    /// network `params` describes.
    ///
    /// Fails with [`Error::UfvkDecodeError`] if a stored key does not parse.
    pub fn account_ufvks_for<P: Parameters>(&self, params: &P) -> Result<Vec<String>> {
        self.account_ufvks
            .iter()
            .map(|ufvk| {
                UnifiedFullViewingKey::decode(&MainNetwork, ufvk)
                    .map(|key| key.encode(params))
                    .map_err(Error::UfvkDecodeError)
            })
            .collect()
    }

    /// The chain tip height the wallet had last been told about, or `None` before the first
    /// chain tip update.
    pub fn chain_tip_height(&self) -> Option<BlockHeight> {
        self.chain_tip_height
    }

    fn from_peek(peek: proto::LegacyPeek) -> Self {
        let mut accounts = peek.accounts.map(|a| a.accounts).unwrap_or_default();
        accounts.sort_by_key(|account| account.account_id);

        // An account's birthday is the height after its prior chain state, as
        // `AccountBirthday::height` defines it.
        let birthday_height = accounts
            .iter()
            .filter_map(|account| account.birthday.as_ref()?.prior_chain_state.as_ref())
            .map(|chain_state| chain_state.block_height + 1)
            .min()
            .map(BlockHeight::from);

        // The chain tip is the end of the furthest scan range, as `WalletRead::chain_height`
        // reads it.
        let chain_tip_height = peek
            .scan_queue
            .iter()
            .map(|range| range.end_height)
            .max()
            .map(|end| BlockHeight::from(end.saturating_sub(1)));

        Self {
            birthday_height,
            account_ufvks: accounts
                .into_iter()
                .map(|account| account.viewing_key)
                .collect(),
            chain_tip_height,
        }
    }
}

/// The description of a saved wallet, in whichever format the blob was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeekedMeta {
    /// A format v1 blob: a bare body, described by what its account and scan queue records say.
    V1(LegacyMeta),
    /// A format v2 blob: an envelope, described by its header.
    V2(WalletMeta),
}

/// Reads the description of a saved wallet from the start of its blob, without decoding the
/// body. For a format v2 blob only the header is read; for a format v1 blob only the account
/// and scan queue records are decoded, whatever the size of the rest.
///
/// Fails with [`Error::CorruptedData`] for an envelope that is truncated before the end of its
/// header, [`Error::UnsupportedProtoVersion`] for a header or a bare body of a version this
/// crate does not read, and [`Error::ProtoDecodingError`] for a header or body that is not
/// well-formed protobuf. Nothing here verifies the body: a [`PeekedMeta::V2`] carries the
/// digest the body must hash to, but only [`MemoryWalletDb::decode_new`] checks it.
pub fn peek_meta(bytes: &[u8]) -> Result<PeekedMeta> {
    let mut buf = bytes;
    if buf.starts_with(&MAGIC) {
        buf.advance(MAGIC.len());
        return read_header(&mut buf).map(PeekedMeta::V2);
    }

    let peek = proto::LegacyPeek::decode(buf)?;
    if peek.version != BODY_VERSION_V1 {
        return Err(Error::UnsupportedProtoVersion(
            BODY_VERSION_V1,
            peek.version,
        ));
    }
    Ok(PeekedMeta::V1(LegacyMeta::from_peek(peek)))
}

/// Reads the header length and the header from `buf`, which must be positioned just after the
/// magic, leaving `buf` at the start of the body.
///
/// Fails with [`Error::CorruptedData`] if the buffer ends before the header does, and as
/// [`WalletMeta::try_from`] does for the header's contents.
pub(crate) fn read_header<B: Buf>(buf: &mut B) -> Result<WalletMeta> {
    if buf.remaining() < 4 {
        return Err(Error::CorruptedData(
            "wallet blob is truncated before its header length".into(),
        ));
    }
    let header_len = usize::try_from(buf.get_u32_le())?;
    if buf.remaining() < header_len {
        return Err(Error::CorruptedData(format!(
            "wallet blob header claims {header_len} bytes but only {} remain",
            buf.remaining()
        )));
    }
    let header = buf.copy_to_bytes(header_len);
    WalletMeta::try_from(proto::WalletMeta::decode(header)?)
}

/// A [`Buf`] that feeds every byte consumed from the buffer it wraps into a SHA-256 digest, so
/// that a body can be verified in the same pass that decodes it, without a copy.
pub(crate) struct HashingBuf<B> {
    inner: B,
    hasher: Sha256,
}

impl<B: Buf> HashingBuf<B> {
    pub(crate) fn new(inner: B) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// Consumes whatever the wrapped buffer has left and returns the digest of everything that
    /// was consumed through this wrapper.
    pub(crate) fn finish(mut self) -> [u8; 32] {
        let rest = self.inner.remaining();
        self.advance(rest);
        self.hasher.finalize().into()
    }
}

impl<B: Buf> Buf for HashingBuf<B> {
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }

    fn chunk(&self) -> &[u8] {
        self.inner.chunk()
    }

    fn advance(&mut self, mut cnt: usize) {
        while cnt > 0 {
            let chunk = self.inner.chunk();
            let n = chunk.len().min(cnt);
            assert!(n > 0, "advance past the end of the buffer");
            self.hasher.update(&chunk[..n]);
            self.inner.advance(n);
            cnt -= n;
        }
    }
}

impl<P: Parameters> MemoryWalletDb<P> {
    /// Describes this wallet as the header of a save would, except for the body digest and
    /// length, which are left empty because no body is written. Nothing is serialized.
    ///
    /// Fails if the fully scanned height cannot be computed or the wallet holds more accounts
    /// or blocks than a `u32` counts.
    pub fn meta(&self) -> Result<WalletMeta> {
        self.build_meta(&self.lineage, None)
    }

    /// Serializes this wallet as a format v2 blob: an envelope whose header describes and
    /// checksums the body. This is the persistence entry point; [`Self::encode`] and
    /// [`Self::encode_to_vec`] write the bare v1 body, which carries no header.
    ///
    /// The save advances the wallet's [`Lineage`]: `save_seq` increments, `created_at_ms` is
    /// fixed by the first save, and `saved_at_ms` and `writer_id` are taken from `ctx`. The
    /// lineage only advances if the whole blob was built, so a failed save leaves it as it was.
    ///
    /// Fails with [`Error::DirtyState`] if a write operation failed part-way (see
    /// [`Self::is_dirty`]), and otherwise as [`Self::meta`] and [`Self::encode`] do.
    pub fn encode_with_meta(&mut self, ctx: EncodeContext<'_>) -> Result<Vec<u8>> {
        self.check_clean()?;

        let mut body = proto::MemoryWallet::try_from(&*self)?;
        body.version = BODY_VERSION_V2;
        let body = body.encode_to_vec();

        let mut lineage = self.lineage.clone();
        lineage.record_save(ctx.now_ms, ctx.writer_id);
        let header = self.build_meta(&lineage, Some(&body))?.to_proto();
        let header_len = u32::try_from(header.encoded_len())?;

        let mut blob = Vec::with_capacity(ENVELOPE_PREFIX_LEN + header.encoded_len() + body.len());
        blob.extend_from_slice(&MAGIC);
        blob.extend_from_slice(&header_len.to_le_bytes());
        header.encode(&mut blob)?;
        blob.extend_from_slice(&body);

        self.lineage = lineage;
        Ok(blob)
    }

    fn build_meta(&self, lineage: &Lineage, body: Option<&[u8]>) -> Result<WalletMeta> {
        let identities = self.account_identities();
        let (body_sha256, body_len) = match body {
            Some(body) => (
                Some(Sha256::digest(body).into()),
                Some(u64::try_from(body.len())?),
            ),
            None => (None, None),
        };
        Ok(WalletMeta {
            schema_version: SCHEMA_VERSION,
            network: network_name(self.params.network_type()).to_owned(),
            lineage: lineage.clone(),
            birthday_height: self.wallet_birthday(),
            fully_scanned_height: self.fully_scanned_height()?,
            max_scanned_height: self.max_scanned_height(),
            chain_tip_height: self.chain_tip_height()?,
            account_count: u32::try_from(identities.len())?,
            seed_fingerprint: identities.first().and_then(|id| id.seed_fingerprint()),
            account_ufvks: identities
                .into_iter()
                .map(|id| id.ufvk().to_owned())
                .collect(),
            body_sha256,
            body_len,
            blocks_retained: u32::try_from(self.blocks.len())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretVec;
    use zcash_client_backend::data_api::{AccountBirthday, WalletWrite};
    use zcash_primitives::block::BlockHash;
    use zcash_protocol::consensus::Network;

    use super::*;
    use crate::error::Error;

    /// A wallet with one derived account and a chain tip, so that every header field has
    /// something to say.
    fn wallet(network: Network) -> MemoryWalletDb<Network> {
        let mut db = MemoryWalletDb::new(network, 100);
        db.create_account(
            "primary",
            &SecretVec::new(vec![7u8; 32]),
            &AccountBirthday::from_sapling_activation(&network, BlockHash([0; 32])),
            None,
        )
        .unwrap();
        db.update_chain_tip(BlockHeight::from(1_000_000)).unwrap();
        db
    }

    fn ctx(now_ms: u64, writer_id: &str) -> EncodeContext<'_> {
        EncodeContext { now_ms, writer_id }
    }

    /// The header read from a blob, with the body digest fields cleared so that it compares
    /// equal to a header built from a live wallet.
    fn peeked_v2_without_digest(bytes: &[u8]) -> WalletMeta {
        match peek_meta(bytes).unwrap() {
            PeekedMeta::V2(mut meta) => {
                assert!(meta.body_sha256().is_some());
                assert!(meta.body_len().is_some());
                meta.body_sha256 = None;
                meta.body_len = None;
                meta
            }
            PeekedMeta::V1(_) => panic!("an envelope peeks as V2"),
        }
    }

    #[test]
    fn meta_round_trips_through_the_envelope() {
        let mut db = wallet(Network::TestNetwork);
        assert_eq!(db.loaded_format_version(), None);
        assert_eq!(db.lineage(), &Lineage::default());

        let bytes = db
            .encode_with_meta(ctx(1_700_000_000_000, "writer-a"))
            .unwrap();
        assert!(bytes.starts_with(&MAGIC));

        let decoded = MemoryWalletDb::decode_new(&bytes[..], Network::TestNetwork, 100).unwrap();
        assert_eq!(decoded, db);
        assert_eq!(decoded.loaded_format_version(), Some(2));
        assert_eq!(decoded.lineage(), db.lineage());
        assert_eq!(decoded.lineage().save_seq(), 1);
        assert_eq!(decoded.lineage().created_at_ms(), 1_700_000_000_000);
        assert_eq!(decoded.lineage().saved_at_ms(), 1_700_000_000_000);
        assert_eq!(decoded.lineage().writer_id(), "writer-a");

        // The header describes the wallet the body decodes to.
        let peeked = peeked_v2_without_digest(&bytes);
        assert_eq!(peeked, decoded.meta().unwrap());
        assert_eq!(peeked, db.meta().unwrap());
        assert_eq!(peeked.schema_version(), SCHEMA_VERSION);
        assert_eq!(peeked.network(), "test");
        assert_eq!(peeked.birthday_height(), db.wallet_birthday());
        assert_eq!(
            peeked.chain_tip_height(),
            Some(BlockHeight::from(1_000_000))
        );
        assert_eq!(peeked.max_scanned_height(), None);
        assert_eq!(peeked.fully_scanned_height(), None);
        assert_eq!(peeked.account_count(), 1);
        let identities = db.account_identities();
        assert_eq!(peeked.account_ufvks(), [identities[0].ufvk().to_owned()]);
        assert_eq!(peeked.seed_fingerprint(), identities[0].seed_fingerprint());
        assert!(peeked.seed_fingerprint().is_some());
        assert_eq!(peeked.blocks_retained(), 0);

        // The digest in the header is the digest of the body that follows it.
        let PeekedMeta::V2(header) = peek_meta(&bytes).unwrap() else {
            unreachable!()
        };
        let body_len = usize::try_from(header.body_len().unwrap()).unwrap();
        let body = &bytes[bytes.len() - body_len..];
        assert_eq!(
            header.body_sha256().unwrap(),
            <[u8; 32]>::from(Sha256::digest(body))
        );
        assert_eq!(proto::MemoryWallet::decode(body).unwrap().version, 2);

        // A re-save of the decoded wallet continues the lineage it was decoded with.
        let mut decoded = decoded;
        let again = decoded
            .encode_with_meta(ctx(1_700_000_000_500, "writer-b"))
            .unwrap();
        let peeked = peeked_v2_without_digest(&again);
        assert_eq!(peeked.lineage().save_seq(), 2);
        assert_eq!(peeked.lineage().created_at_ms(), 1_700_000_000_000);
        assert_eq!(peeked.lineage().saved_at_ms(), 1_700_000_000_500);
        assert_eq!(peeked.lineage().writer_id(), "writer-b");
    }

    #[test]
    fn each_save_advances_the_sequence_and_the_first_fixes_the_creation_time() {
        let mut db = wallet(Network::TestNetwork);
        let first = db.encode_with_meta(ctx(10, "w")).unwrap();
        let second = db.encode_with_meta(ctx(20, "w")).unwrap();
        let third = db.encode_with_meta(ctx(30, "w")).unwrap();

        for (bytes, seq, saved_at) in [(&first, 1, 10), (&second, 2, 20), (&third, 3, 30)] {
            let meta = peeked_v2_without_digest(bytes);
            assert_eq!(meta.lineage().save_seq(), seq);
            assert_eq!(meta.lineage().created_at_ms(), 10);
            assert_eq!(meta.lineage().saved_at_ms(), saved_at);
        }
        assert_eq!(db.lineage().save_seq(), 3);
        assert_eq!(db.lineage().created_at_ms(), 10);
        assert_eq!(db.lineage().saved_at_ms(), 30);
    }

    #[test]
    fn a_v1_blob_still_decodes_and_peeks() {
        let db = wallet(Network::TestNetwork);
        let bytes = db.encode_to_vec().unwrap();
        assert_eq!(&bytes[..2], &[0x08, 0x01]);

        let decoded = MemoryWalletDb::decode_new(&bytes[..], Network::TestNetwork, 100).unwrap();
        assert_eq!(decoded, db);
        assert_eq!(decoded.loaded_format_version(), Some(1));
        assert_eq!(decoded.lineage(), &Lineage::default());

        let PeekedMeta::V1(legacy) = peek_meta(&bytes).unwrap() else {
            panic!("a bare body peeks as V1")
        };
        assert_eq!(legacy.birthday_height(), db.wallet_birthday());
        assert!(legacy.birthday_height().is_some());
        // The body stores keys in the mainnet encoding whatever the network; re-encoded for
        // the wallet's network they match what the wallet reports.
        let identity = &db.account_identities()[0];
        assert_eq!(
            legacy.account_ufvks(),
            [
                UnifiedFullViewingKey::decode(&Network::TestNetwork, identity.ufvk())
                    .unwrap()
                    .encode(&MainNetwork)
            ]
        );
        assert_eq!(
            legacy.account_ufvks_for(&Network::TestNetwork).unwrap(),
            [identity.ufvk().to_owned()]
        );
        assert_eq!(
            legacy.chain_tip_height(),
            Some(BlockHeight::from(1_000_000))
        );

        // An empty wallet has nothing to say, but still peeks.
        let empty = MemoryWalletDb::new(Network::TestNetwork, 100)
            .encode_to_vec()
            .unwrap();
        let PeekedMeta::V1(legacy) = peek_meta(&empty).unwrap() else {
            panic!("a bare body peeks as V1")
        };
        assert_eq!(legacy.birthday_height(), None);
        assert!(legacy.account_ufvks().is_empty());
        assert_eq!(legacy.chain_tip_height(), None);
    }

    #[test]
    fn a_body_of_the_wrong_version_for_its_format_is_rejected() {
        let db = wallet(Network::TestNetwork);
        let mut body = proto::MemoryWallet::try_from(&db).unwrap();
        body.version = BODY_VERSION_V2;
        let bare_v2 = body.encode_to_vec();
        assert!(matches!(
            MemoryWalletDb::decode_new(&bare_v2[..], Network::TestNetwork, 100),
            Err(Error::UnsupportedProtoVersion(1, 2))
        ));
        assert!(matches!(
            peek_meta(&bare_v2),
            Err(Error::UnsupportedProtoVersion(1, 2))
        ));
    }

    #[test]
    fn a_corrupted_body_is_rejected_by_its_checksum() {
        let mut db = wallet(Network::TestNetwork);
        let bytes = db.encode_with_meta(ctx(1, "w")).unwrap();
        let header_len =
            usize::try_from(u32::from_le_bytes(bytes[4..8].try_into().unwrap())).unwrap();
        let body_start = ENVELOPE_PREFIX_LEN + header_len;

        // Flipping a byte anywhere in the body fails the checksum, whether or not the body
        // still decodes as protobuf.
        for offset in [body_start, (body_start + bytes.len()) / 2, bytes.len() - 1] {
            let mut corrupted = bytes.clone();
            corrupted[offset] ^= 0x55;
            assert!(
                matches!(
                    MemoryWalletDb::decode_new(&corrupted[..], Network::TestNetwork, 100),
                    Err(Error::ChecksumMismatch)
                ),
                "a flipped byte at {offset} must fail the checksum"
            );
        }
    }

    #[test]
    fn a_blob_from_another_network_is_rejected() {
        let mut main = wallet(Network::MainNetwork);
        let bytes = main.encode_with_meta(ctx(1, "w")).unwrap();
        assert!(matches!(
            MemoryWalletDb::decode_new(&bytes[..], Network::TestNetwork, 100),
            Err(Error::NetworkMismatch { expected, found }) if expected == "test" && found == "main"
        ));

        let mut test = wallet(Network::TestNetwork);
        let bytes = test.encode_with_meta(ctx(1, "w")).unwrap();
        assert!(matches!(
            MemoryWalletDb::decode_new(&bytes[..], Network::MainNetwork, 100),
            Err(Error::NetworkMismatch { expected, found }) if expected == "main" && found == "test"
        ));
    }

    #[test]
    fn a_truncated_envelope_is_rejected_cleanly() {
        let mut db = wallet(Network::TestNetwork);
        let bytes = db.encode_with_meta(ctx(1, "w")).unwrap();

        // The header length points past the end of the buffer.
        let mut oversized = bytes.clone();
        oversized[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            MemoryWalletDb::decode_new(&oversized[..], Network::TestNetwork, 100),
            Err(Error::CorruptedData(_))
        ));
        assert!(matches!(
            peek_meta(&oversized),
            Err(Error::CorruptedData(_))
        ));

        // The buffer ends inside the fixed prefix, inside the header, and inside the body.
        for len in [4, 6, 12, bytes.len() - 1] {
            assert!(
                matches!(
                    MemoryWalletDb::decode_new(&bytes[..len], Network::TestNetwork, 100),
                    Err(Error::CorruptedData(_))
                ),
                "a blob cut at {len} bytes must be rejected"
            );
        }
        assert!(matches!(
            peek_meta(&bytes[..6]),
            Err(Error::CorruptedData(_))
        ));

        // Trailing bytes after the body are not a valid blob either.
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(matches!(
            MemoryWalletDb::decode_new(&padded[..], Network::TestNetwork, 100),
            Err(Error::CorruptedData(_))
        ));
    }

    #[test]
    fn a_dirty_wallet_refuses_to_save_and_keeps_its_lineage() {
        let mut db = wallet(Network::TestNetwork);
        db.encode_with_meta(ctx(1, "w")).unwrap();
        let _ = db.mutate("put_blocks", |_| Err::<(), _>(Error::NonSequentialBlocks));
        assert!(matches!(
            db.encode_with_meta(ctx(2, "w")),
            Err(Error::DirtyState("put_blocks"))
        ));
        assert_eq!(db.lineage().save_seq(), 1);
        assert_eq!(db.lineage().saved_at_ms(), 1);
    }

    #[test]
    fn a_fresh_wallet_meta_has_no_body_digest() {
        let db = MemoryWalletDb::new(Network::MainNetwork, 100);
        let meta = db.meta().unwrap();
        assert_eq!(meta.network(), "main");
        assert_eq!(meta.body_sha256(), None);
        assert_eq!(meta.body_len(), None);
        assert_eq!(meta.account_count(), 0);
        assert_eq!(meta.lineage(), &Lineage::default());
    }
}
