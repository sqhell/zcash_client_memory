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
use zcash_protocol::consensus::{BlockHeight, NetworkType, Parameters};

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

    /// The encoded unified full viewing key of every account, in account id order.
    pub fn account_ufvks(&self) -> &[String] {
        &self.account_ufvks
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
