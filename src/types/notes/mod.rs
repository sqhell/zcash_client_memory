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
    use crate::error::{Error, proto_enum};
    use crate::proto::memwallet::{self as proto};
    use crate::read_optional;
    use jubjub::Fr;
    use zcash_client_backend::wallet::{Note, NoteId};

    fn corrupted(what: &str) -> Error {
        Error::CorruptedData(what.to_owned())
    }

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
            let pool = proto_enum::<proto::PoolType>("pool", note_id.pool)?;
            Ok(Self::new(
                read_optional!(note_id, tx_id)?.try_into()?,
                shielded_protocol_from_proto(pool)?,
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

    impl From<&Note> for proto::Note {
        fn from(note: &Note) -> Self {
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

    impl TryFrom<proto::Note> for Note {
        type Error = Error;

        /// Rejects a note whose protocol is unknown, whose required fields are missing, or
        /// whose bytes do not form a valid address, seed, or nullifier base. A wallet that
        /// panicked here could never be loaded again; one that reported the error can be
        /// restored from the chain.
        fn try_from(note: proto::Note) -> Result<Self, Error> {
            let protocol = proto_enum::<proto::ShieldedProtocol>("protocol", note.protocol)?;
            match protocol {
                proto::ShieldedProtocol::Sapling => {
                    let recipient =
                        sapling::PaymentAddress::from_bytes(&note.recipient.try_into()?)
                            .ok_or_else(|| corrupted("invalid Sapling payment address in note"))?;
                    let value = sapling::value::NoteValue::from_raw(note.value);
                    let rseed = read_optional!(note, rseed)?;
                    let rseed_type = rseed
                        .rseed_type
                        .ok_or(Error::ProtoMissingField("rseed_type"))?;
                    let rseed = match proto_enum::<proto::RSeedType>("rseed_type", rseed_type)? {
                        proto::RSeedType::BeforeZip212 => sapling::Rseed::BeforeZip212(
                            Fr::from_bytes(&rseed.payload.try_into()?)
                                .into_option()
                                .ok_or_else(|| corrupted("invalid pre-ZIP 212 rseed in note"))?,
                        ),
                        proto::RSeedType::AfterZip212 => {
                            sapling::Rseed::AfterZip212(rseed.payload.try_into()?)
                        }
                    };
                    Ok(Self::Sapling(sapling::Note::from_parts(
                        recipient, value, rseed,
                    )))
                }
                #[cfg(feature = "orchard")]
                proto::ShieldedProtocol::Orchard | proto::ShieldedProtocol::Ironwood => {
                    let pool = match protocol {
                        proto::ShieldedProtocol::Ironwood => orchard::ValuePool::Ironwood,
                        _ => orchard::ValuePool::Orchard,
                    };
                    let recipient =
                        orchard::Address::from_raw_address_bytes(&note.recipient.try_into()?)
                            .into_option()
                            .ok_or_else(|| corrupted("invalid Orchard address in note"))?;
                    let value = orchard::value::NoteValue::from_raw(note.value);
                    let rho =
                        orchard::note::Rho::from_bytes(&read_optional!(note, rho)?.try_into()?)
                            .into_option()
                            .ok_or_else(|| corrupted("invalid rho in Orchard note"))?;
                    let rseed = orchard::note::RandomSeed::from_bytes(
                        read_optional!(note, rseed)?.payload.try_into()?,
                        &rho,
                    )
                    .into_option()
                    .ok_or_else(|| corrupted("invalid rseed in Orchard note"))?;
                    // orchard 0.15 (NU6.3) adds a note-plaintext version: V2 = Orchard,
                    // V3 = Ironwood. Notes persisted before the version field existed are
                    // Orchard-pool notes, so they reconstruct as V2; Ironwood notes without the
                    // field cannot exist, but default to V3 for symmetry.
                    let version = match (note.note_version, pool) {
                        (Some(2), _) => orchard::note::NoteVersion::V2,
                        (Some(3), _) => orchard::note::NoteVersion::V3,
                        (Some(other), _) => {
                            return Err(Error::CorruptedData(format!(
                                "unknown Orchard note version {other}"
                            )));
                        }
                        (None, orchard::ValuePool::Orchard) => orchard::note::NoteVersion::V2,
                        (None, orchard::ValuePool::Ironwood) => orchard::note::NoteVersion::V3,
                    };
                    let note = orchard::Note::from_parts(recipient, value, rho, rseed, version)
                        .into_option()
                        .ok_or_else(|| corrupted("Orchard note is not a valid note"))?;
                    Ok(Self::Orchard { note, pool })
                }
                #[cfg(not(feature = "orchard"))]
                proto::ShieldedProtocol::Orchard | proto::ShieldedProtocol::Ironwood => {
                    Err(Error::OrchardNotEnabled)
                }
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

            let proto_note: proto::Note = (&note).into();
            let recovered: Note = proto_note.try_into().unwrap();

            assert_eq!(note, recovered);
        }

        fn sapling_proto_note() -> proto::Note {
            proto::Note {
                protocol: proto::ShieldedProtocol::Sapling as i32,
                recipient: vec![
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x8e,
                    0x11, 0x9d, 0x72, 0x99, 0x2b, 0x56, 0x0d, 0x26, 0x50, 0xff, 0xe0, 0xbe, 0x7f,
                    0x35, 0x42, 0xfd, 0x97, 0x00, 0x3c, 0xb7, 0xcc, 0x3a, 0xbf, 0xf8, 0x1a, 0x7f,
                    0x90, 0x37, 0xf3, 0xea,
                ],
                value: 99,
                rseed: Some(proto::RSeed {
                    rseed_type: Some(proto::RSeedType::AfterZip212 as i32),
                    payload: vec![0; 32],
                }),
                rho: None,
                note_version: None,
            }
        }

        #[test]
        fn note_without_rseed_is_an_error() {
            let proto_note = proto::Note {
                rseed: None,
                ..sapling_proto_note()
            };
            assert!(matches!(
                Note::try_from(proto_note),
                Err(Error::ProtoMissingField("rseed"))
            ));
        }

        #[test]
        fn note_with_unknown_rseed_type_is_an_error() {
            let proto_note = proto::Note {
                rseed: Some(proto::RSeed {
                    rseed_type: Some(7),
                    payload: vec![0; 32],
                }),
                ..sapling_proto_note()
            };
            assert!(matches!(
                Note::try_from(proto_note),
                Err(Error::UnknownEnumValue {
                    field: "rseed_type",
                    value: 7
                })
            ));
        }

        #[test]
        fn note_with_unknown_protocol_is_an_error() {
            let proto_note = proto::Note {
                protocol: 42,
                ..sapling_proto_note()
            };
            assert!(matches!(
                Note::try_from(proto_note),
                Err(Error::UnknownEnumValue {
                    field: "protocol",
                    value: 42
                })
            ));
        }

        #[test]
        fn note_with_short_recipient_is_an_error() {
            let proto_note = proto::Note {
                recipient: vec![0; 10],
                ..sapling_proto_note()
            };
            assert!(matches!(
                Note::try_from(proto_note),
                Err(Error::ByteVecToArrayConversion(_))
            ));
        }
    }
}
