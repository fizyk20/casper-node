use std::collections::BTreeSet;

use casper_types::{
    bytesrepr::{self, Bytes, FromBytes, ToBytes},
    PublicKey,
};

use datasize::DataSize;
use itertools::Itertools;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::error;

/// List of identifiers for finality signatures for a particular past block.
///
/// That past block height is current_height - signature_rewards_max_delay, the latter being defined
/// in the chainspec.
///
/// We need to wait for a few blocks to pass (`signature_rewards_max_delay`) to store the finality
/// signers because we need a bit of time to get the block finality.
#[derive(
    Clone,
    DataSize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
    Debug,
    Default,
)]
pub struct PastFinalitySignatures(Vec<u8>);

impl PastFinalitySignatures {
    /// Creates a new set of recorded finality signaures from the era's validators +
    /// the list of validators which signed.
    pub fn from_validator_set<'a>(
        public_keys: &BTreeSet<PublicKey>,
        all_validators: impl IntoIterator<Item = &'a PublicKey>,
    ) -> Self {
        // Take the validators list
        // Replace the ones who signed with 1 and the ones who didn't with 0
        // Pack everything into bytes
        let result = Self::pack(
            all_validators
                .into_iter()
                .map(|key| u8::from(public_keys.contains(key))),
        );

        let included_count: u32 = result.0.iter().map(|c| c.count_ones()).sum();
        if included_count as usize != public_keys.len() {
            error!(
                included_count,
                expected_count = public_keys.len(),
                "error creating past finality signatures from validator set"
            );
        }

        result
    }

    /// Gets the list of validators which signed from a set of recorded finality signaures (`self`)
    /// + the era's validators.
    pub fn into_validator_set(
        self,
        all_validators: impl IntoIterator<Item = PublicKey>,
    ) -> BTreeSet<PublicKey> {
        self.unpack()
            .zip(all_validators.into_iter())
            .filter_map(|(active, validator)| (active != 0).then_some(validator))
            .collect()
    }

    /// Packs the bits to bytes, to create a `PastFinalitySignature`
    /// from an iterator of bits.
    ///
    /// If a value is neither 1 nor 0, it is interpreted as a 1.
    pub(crate) fn pack(bits: impl Iterator<Item = u8>) -> Self {
        fn set_bit_at(value: u8, position: usize) -> u8 {
            // Sanitize the value (must be 0 or 1):
            let value = u8::from(value != 0);

            value << (7 - position)
        }

        let inner = bits
            .chunks(8)
            .into_iter()
            .map(|bits_chunk| {
                bits_chunk
                    .enumerate()
                    .fold(0, |acc, (pos, value)| acc | set_bit_at(value, pos))
            })
            .collect();

        PastFinalitySignatures(inner)
    }

    /// Unpacks the bytes to bits,
    /// to get a human readable representation of `PastFinalitySignature`.
    pub(crate) fn unpack(&self) -> impl Iterator<Item = u8> + '_ {
        // Returns the bit at the given position (0 or 1):
        fn bit_at(byte: u8, position: u8) -> u8 {
            (byte & (0b1000_0000 >> position)) >> (7 - position)
        }

        self.0
            .iter()
            .flat_map(|&byte| (0..8).into_iter().map(move |i| bit_at(byte, i)))
    }
}

impl ToBytes for PastFinalitySignatures {
    fn to_bytes(&self) -> Result<Vec<u8>, bytesrepr::Error> {
        let mut buffer = bytesrepr::allocate_buffer(self)?;
        buffer.extend(Bytes::from(self.0.as_ref()).to_bytes()?);
        Ok(buffer)
    }

    fn serialized_length(&self) -> usize {
        self.0.serialized_length()
    }
}

impl FromBytes for PastFinalitySignatures {
    fn from_bytes(bytes: &[u8]) -> Result<(Self, &[u8]), bytesrepr::Error> {
        let (inner, rest) = Bytes::from_bytes(bytes)?;
        Ok((PastFinalitySignatures(inner.into()), rest))
    }
}

#[cfg(test)]
mod tests {
    use super::PastFinalitySignatures;
    use casper_types::{
        bytesrepr::{FromBytes, ToBytes},
        testing::TestRng,
        PublicKey,
    };
    use rand::{seq::IteratorRandom, Rng};
    use std::collections::BTreeSet;

    #[test]
    fn empty_signatures() {
        let rng = &mut TestRng::new();
        let validators: Vec<_> = std::iter::repeat_with(|| PublicKey::random(rng))
            .take(7)
            .collect();
        let original_signed = BTreeSet::new();

        let past_finality_signatures =
            PastFinalitySignatures::from_validator_set(&original_signed, validators.iter());

        assert_eq!(past_finality_signatures.0, &[0]);

        let signed = past_finality_signatures.into_validator_set(validators.into_iter());

        assert_eq!(original_signed, signed);
    }

    #[test]
    fn from_and_to_methods_match_in_a_simple_case() {
        let rng = &mut TestRng::new();
        let validators: Vec<_> = std::iter::repeat_with(|| PublicKey::random(rng))
            .take(11)
            .collect();
        let signed = {
            let mut signed = BTreeSet::new();
            signed.insert(validators[2].clone());
            signed.insert(validators[5].clone());
            signed.insert(validators[6].clone());
            signed.insert(validators[8].clone());
            signed.insert(validators[10].clone());
            signed
        };

        let past_finality_signatures =
            PastFinalitySignatures::from_validator_set(&signed, validators.iter());

        assert_eq!(past_finality_signatures.0, &[0b0010_0110, 0b1010_0000]);

        let signed_ = past_finality_signatures.into_validator_set(validators.into_iter());

        assert_eq!(signed, signed_);
    }

    #[test]
    fn simple_serialization_roundtrip() {
        let data = PastFinalitySignatures(vec![1, 2, 3, 4, 5]);

        let serialized = data.to_bytes().unwrap();
        assert_eq!(serialized.len(), data.0.len() + 4);
        assert_eq!(data.serialized_length(), data.0.len() + 4);

        let (deserialized, rest) = PastFinalitySignatures::from_bytes(&serialized).unwrap();

        assert_eq!(data, deserialized);
        assert_eq!(rest, &[0u8; 0]);
    }

    #[test]
    fn serialization_roundtrip_of_empty_data() {
        let data = PastFinalitySignatures::default();

        let serialized = data.to_bytes().unwrap();
        assert_eq!(serialized, &[0; 4]);
        assert_eq!(data.serialized_length(), 4);

        let (deserialized, rest) = PastFinalitySignatures::from_bytes(&serialized).unwrap();

        assert_eq!(data, deserialized);
        assert_eq!(rest, &[0u8; 0]);
    }

    #[test]
    fn serialization_roundtrip_of_random_data() {
        let rng = &mut TestRng::new();
        let n_validators = rng.gen_range(50..200);
        let all_validators: BTreeSet<_> = std::iter::repeat_with(|| PublicKey::random(rng))
            .take(n_validators)
            .collect();
        let n_to_sign = rng.gen_range(0..all_validators.len());
        let public_keys = all_validators
            .iter()
            .cloned()
            .choose_multiple(rng, n_to_sign)
            .into_iter()
            .collect();

        let past_finality_signatures =
            PastFinalitySignatures::from_validator_set(&public_keys, all_validators.iter());

        let serialized = past_finality_signatures.to_bytes().unwrap();
        let (deserialized, rest) = PastFinalitySignatures::from_bytes(&serialized).unwrap();

        assert_eq!(public_keys, deserialized.into_validator_set(all_validators));
        assert_eq!(rest, &[0u8; 0]);
    }
}

impl crate::utils::specimen::LargestSpecimen for PastFinalitySignatures {
    fn largest_specimen<E: crate::utils::specimen::SizeEstimator>(
        estimator: &E,
        _cache: &mut crate::utils::specimen::Cache,
    ) -> Self {
        PastFinalitySignatures(vec![
            u8::MAX;
            div_by_8_ceil(estimator.parameter("validator_count"))
        ])
    }
}

#[cfg(any(feature = "testing", test))]
impl PastFinalitySignatures {
    pub(crate) fn random(rng: &mut casper_types::testing::TestRng, n_validators: usize) -> Self {
        let mut bytes = vec![0; div_by_8_ceil(n_validators)];

        rand::RngCore::fill_bytes(rng, bytes.as_mut());

        PastFinalitySignatures(bytes)
    }
}

/// Divides by 8 but round to the higher integer.
fn div_by_8_ceil(n: usize) -> usize {
    (n + 7) / 8
}
