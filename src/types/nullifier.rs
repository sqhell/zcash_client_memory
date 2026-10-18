use std::{collections::BTreeMap, ops::Deref};

use zcash_protocol::{PoolType, consensus::BlockHeight};

/// Maps a nullifier to the block height and transaction index (NOT txid!) where it was spent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NullifierMap(pub(crate) BTreeMap<Nullifier, (BlockHeight, u32)>);

impl NullifierMap {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }
    pub fn insert(&mut self, height: BlockHeight, index: u32, nullifier: Nullifier) {
        self.0.insert(nullifier, (height, index));
    }

    pub fn get(&self, nullifier: &Nullifier) -> Option<&(BlockHeight, u32)> {
        self.0.get(nullifier)
    }
}

impl Deref for NullifierMap {
    type Target = BTreeMap<Nullifier, (BlockHeight, u32)>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A nullifier tagged with the pool it belongs to.
///
/// Ironwood (NU6.3) nullifiers have the Orchard nullifier type but are revealed in a separate
/// pool with its own note commitment tree, so the tag keeps the two pools' entries apart in the
/// nullifier map and the received-note index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Nullifier {
    Sapling(sapling::Nullifier),
    #[cfg(feature = "orchard")]
    Orchard(orchard::note::Nullifier),
    #[cfg(feature = "orchard")]
    Ironwood(orchard::note::Nullifier),
}

impl Nullifier {
    pub(crate) fn _pool(&self) -> PoolType {
        match self {
            Nullifier::Sapling(_) => PoolType::SAPLING,
            #[cfg(feature = "orchard")]
            Nullifier::Orchard(_) => PoolType::ORCHARD,
            #[cfg(feature = "orchard")]
            Nullifier::Ironwood(_) => PoolType::IRONWOOD,
        }
    }

    /// Tags an Orchard-shaped nullifier with the value pool its note belongs to.
    #[cfg(feature = "orchard")]
    pub(crate) fn orchard_family(pool: orchard::ValuePool, nf: orchard::note::Nullifier) -> Self {
        match pool {
            orchard::ValuePool::Orchard => Nullifier::Orchard(nf),
            orchard::ValuePool::Ironwood => Nullifier::Ironwood(nf),
        }
    }
}

impl From<sapling::Nullifier> for Nullifier {
    fn from(n: sapling::Nullifier) -> Self {
        Nullifier::Sapling(n)
    }
}

mod serialization {
    use super::*;
    use crate::{Error, error::proto_enum, proto::memwallet as proto};

    impl From<Nullifier> for proto::Nullifier {
        fn from(nullifier: Nullifier) -> Self {
            match nullifier {
                Nullifier::Sapling(n) => Self {
                    protocol: proto::ShieldedProtocol::Sapling.into(),
                    nullifier: n.to_vec(),
                },
                #[cfg(feature = "orchard")]
                Nullifier::Orchard(n) => Self {
                    protocol: proto::ShieldedProtocol::Orchard.into(),
                    nullifier: n.to_bytes().to_vec(),
                },
                #[cfg(feature = "orchard")]
                Nullifier::Ironwood(n) => Self {
                    protocol: proto::ShieldedProtocol::Ironwood.into(),
                    nullifier: n.to_bytes().to_vec(),
                },
            }
        }
    }

    impl TryFrom<proto::Nullifier> for Nullifier {
        type Error = Error;

        fn try_from(nullifier: proto::Nullifier) -> Result<Self, Self::Error> {
            #[cfg(feature = "orchard")]
            let orchard_nf = |bytes: Vec<u8>| -> Result<orchard::note::Nullifier, Error> {
                orchard::note::Nullifier::from_bytes(&bytes.try_into()?)
                    .into_option()
                    .ok_or(Error::CorruptedData("Invalid Orchard nullifier".into()))
            };
            let protocol = proto_enum::<proto::ShieldedProtocol>("protocol", nullifier.protocol)?;
            Ok(match protocol {
                proto::ShieldedProtocol::Sapling => {
                    Nullifier::Sapling(sapling::Nullifier::from_slice(&nullifier.nullifier)?)
                }
                proto::ShieldedProtocol::Orchard => {
                    #[cfg(not(feature = "orchard"))]
                    return Err(Error::Other("Orchard support is not enabled".into()));
                    #[cfg(feature = "orchard")]
                    Nullifier::Orchard(orchard_nf(nullifier.nullifier)?)
                }
                proto::ShieldedProtocol::Ironwood => {
                    #[cfg(not(feature = "orchard"))]
                    return Err(Error::Other("Orchard support is not enabled".into()));
                    #[cfg(feature = "orchard")]
                    Nullifier::Ironwood(orchard_nf(nullifier.nullifier)?)
                }
            })
        }
    }
}
