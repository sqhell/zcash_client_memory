pub mod received;
pub mod sent;

pub(crate) use received::{ReceievedNoteSpends, to_spendable_notes};
pub use received::{ReceivedNote, ReceivedNoteTable};
#[cfg(test)]
pub(crate) use sent::SentNoteId;
pub use sent::{SentNote, SentNoteTable};
pub(crate) use serialization::shielded_protocol_from_proto;

/// The shielded protocol an Orchard-family note belongs to, by its value pool.
#[cfg(feature = "orchard")]
pub(crate) fn shielded_protocol_for_value_pool(
    pool: orchard::ValuePool,
) -> zcash_protocol::ShieldedPool {
    match pool {
        orchard::ValuePool::Orchard => zcash_protocol::ShieldedPool::Orchard,
        orchard::ValuePool::Ironwood => zcash_protocol::ShieldedPool::Ironwood,
    }
}

mod serialization {
    use crate::error::Error;
    use crate::proto::memwallet::{self as proto};
    use crate::read_optional;
    use jubjub::Fr;
    use zcash_client_backend::wallet::{Note, NoteId};

    impl From<NoteId> for proto::NoteId {
        fn from(note_id: NoteId) -> Self {
            Self {
                tx_id: Some(note_id.txid().into()),
                pool: match note_id.protocol() {
                    zcash_protocol::ShieldedPool::Sapling => {
                        proto::PoolType::ShieldedSapling.into()
                    }
                    #[cfg(feature = "orchard")]
                    zcash_protocol::ShieldedPool::Orchard => {
                        proto::PoolType::ShieldedOrchard.into()
                    }
                    #[cfg(feature = "orchard")]
                    zcash_protocol::ShieldedPool::Ironwood => {
                        proto::PoolType::ShieldedIronwood.into()
                    }
                    #[cfg(not(feature = "orchard"))]
                    zcash_protocol::ShieldedPool::Orchard
                    | zcash_protocol::ShieldedPool::Ironwood => panic!(
                        "Attempting to deserialize orchard supporting wallet using library built without orchard feature"
                    ),
                },
                output_index: u32::from(note_id.output_index()),
            }
        }
    }

    impl TryFrom<proto::NoteId> for NoteId {
        type Error = Error;
        fn try_from(note_id: proto::NoteId) -> Result<Self, Self::Error> {
            Ok(Self::new(
                read_optional!(note_id.clone(), tx_id)?.try_into()?,
                shielded_protocol_from_proto(note_id.pool())?,
                note_id.output_index.try_into()?,
            ))
        }
    }

    /// Maps a serialized pool to a shielded protocol, rejecting transparent and pools this build
    /// does not support.
    pub(crate) fn shielded_protocol_from_proto(
        pool: proto::PoolType,
    ) -> Result<zcash_protocol::ShieldedPool, Error> {
        match pool {
            proto::PoolType::ShieldedSapling => Ok(zcash_protocol::ShieldedPool::Sapling),
            #[cfg(feature = "orchard")]
            proto::PoolType::ShieldedOrchard => Ok(zcash_protocol::ShieldedPool::Orchard),
            #[cfg(feature = "orchard")]
            proto::PoolType::ShieldedIronwood => Ok(zcash_protocol::ShieldedPool::Ironwood),
            #[cfg(not(feature = "orchard"))]
            proto::PoolType::ShieldedOrchard | proto::PoolType::ShieldedIronwood => {
                Err(Error::OrchardNotEnabled)
            }
            proto::PoolType::Transparent => Err(Error::CorruptedData(
                "a transparent pool cannot identify a shielded note".into(),
            )),
        }
    }

    impl From<Note> for proto::Note {
        fn from(note: Note) -> Self {
            match note {
                Note::Sapling(note) => Self {
                    protocol: proto::ShieldedProtocol::Sapling.into(),
                    recipient: note.recipient().to_bytes().to_vec(),
                    value: note.value().inner(),
                    rseed: match note.rseed() {
                        sapling::Rseed::AfterZip212(inner) => Some(proto::RSeed {
                            rseed_type: Some(proto::RSeedType::AfterZip212 as i32),
                            payload: inner.to_vec(),
                        }),
                        sapling::Rseed::BeforeZip212(inner) => Some(proto::RSeed {
                            rseed_type: Some(proto::RSeedType::BeforeZip212 as i32),
                            payload: inner.to_bytes().to_vec(),
                        }),
                    },
                    rho: None,
                    note_version: None,
                },
                #[cfg(feature = "orchard")]
                Note::Orchard { note, pool } => Self {
                    protocol: match pool {
                        orchard::ValuePool::Orchard => proto::ShieldedProtocol::Orchard,
                        orchard::ValuePool::Ironwood => proto::ShieldedProtocol::Ironwood,
                    }
                    .into(),
                    recipient: note.recipient().to_raw_address_bytes().to_vec(),
                    value: note.value().inner(),
                    rseed: Some(proto::RSeed {
                        rseed_type: None,
                        payload: note.rseed().as_bytes().to_vec(),
                    }),
                    rho: Some(note.rho().to_bytes().to_vec()),
                    note_version: Some(match note.version() {
                        orchard::note::NoteVersion::V2 => 2,
                        orchard::note::NoteVersion::V3 => 3,
                    }),
                },
            }
        }
    }

    impl From<proto::Note> for Note {
        fn from(note: proto::Note) -> Self {
            match note.protocol() {
                proto::ShieldedProtocol::Sapling => {
                    let recipient =
                        sapling::PaymentAddress::from_bytes(&note.recipient.try_into().unwrap())
                            .unwrap();
                    let value = sapling::value::NoteValue::from_raw(note.value);
                    let rseed = match note.rseed {
                        Some(proto::RSeed {
                            rseed_type: Some(0),
                            payload,
                        }) => sapling::Rseed::BeforeZip212(
                            Fr::from_bytes(&payload.try_into().unwrap()).unwrap(),
                        ),
                        Some(proto::RSeed {
                            rseed_type: Some(1),
                            payload,
                        }) => sapling::Rseed::AfterZip212(payload.try_into().unwrap()),
                        _ => panic!("rseed is required"),
                    };
                    Self::Sapling(sapling::Note::from_parts(recipient, value, rseed))
                }
                #[cfg(feature = "orchard")]
                protocol @ (proto::ShieldedProtocol::Orchard | proto::ShieldedProtocol::Ironwood) => {
                    let pool = match protocol {
                        proto::ShieldedProtocol::Ironwood => orchard::ValuePool::Ironwood,
                        _ => orchard::ValuePool::Orchard,
                    };
                    let recipient = orchard::Address::from_raw_address_bytes(
                        &note.recipient.try_into().unwrap(),
                    )
                    .unwrap();
                    let value = orchard::value::NoteValue::from_raw(note.value);
                    let rho =
                        orchard::note::Rho::from_bytes(&note.rho.unwrap().try_into().unwrap())
                            .unwrap();
                    let rseed = orchard::note::RandomSeed::from_bytes(
                        note.rseed.unwrap().payload.try_into().unwrap(),
                        &rho,
                    )
                    .unwrap();
                    // orchard 0.15 (NU6.3) adds a note-plaintext version: V2 = Orchard,
                    // V3 = Ironwood. Notes persisted before the version field existed are
                    // Orchard-pool notes, so they reconstruct as V2; Ironwood notes without the
                    // field cannot exist, but default to V3 for symmetry.
                    let version = match (note.note_version, pool) {
                        (Some(2), _) => orchard::note::NoteVersion::V2,
                        (Some(3), _) => orchard::note::NoteVersion::V3,
                        (_, orchard::ValuePool::Orchard) => orchard::note::NoteVersion::V2,
                        (_, orchard::ValuePool::Ironwood) => orchard::note::NoteVersion::V3,
                    };
                    Self::Orchard {
                        note: orchard::Note::from_parts(recipient, value, rho, rseed, version)
                            .unwrap(),
                        pool,
                    }
                }
                #[cfg(not(feature = "orchard"))]
                _ => panic!("invalid protocol"),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::proto::memwallet as proto;

        #[test]
        fn test_note_roundtrip() {
            let note = Note::Sapling(sapling::note::Note::from_parts(
                sapling::PaymentAddress::from_bytes(&[
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x8e,
                    0x11, 0x9d, 0x72, 0x99, 0x2b, 0x56, 0x0d, 0x26, 0x50, 0xff, 0xe0, 0xbe, 0x7f,
                    0x35, 0x42, 0xfd, 0x97, 0x00, 0x3c, 0xb7, 0xcc, 0x3a, 0xbf, 0xf8, 0x1a, 0x7f,
                    0x90, 0x37, 0xf3, 0xea,
                ])
                .unwrap(),
                sapling::value::NoteValue::from_raw(99),
                sapling::Rseed::AfterZip212([0; 32]),
            ));

            let proto_note: proto::Note = note.clone().into();
            let recovered: Note = proto_note.into();

            assert_eq!(note, recovered);
        }
    }
}
