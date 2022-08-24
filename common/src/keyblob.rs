//! Key blob manipulation functionality.

use crate::{
    cbor, cbor_type_error, contains_tag_value, crypto, km_err,
    wire::keymint::{KeyCharacteristics, KeyParam, KeyPurpose, SecurityLevel, VerifiedBootState},
    AsCborValue, CborError, Error,
};
use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use coset::TaggedCborSerializable;
use kmr_derive::AsCborValue;
use log::error;

pub mod legacy;
#[cfg(test)]
mod tests;

/// Nonce value of all zeroes used in AES-GCM key encryption.
const ZERO_NONCE: [u8; 12] = [0u8; 12];

/// Identifier for secure deletion secret storage slot.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, AsCborValue)]
pub struct SecureDeletionSlot(pub u32);

/// Keyblob format version.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, AsCborValue)]
pub enum Version {
    V1 = 0,
}

/// Encrypted key material, as translated to/from CBOR.
#[derive(Clone, Debug)]
pub enum EncryptedKeyBlob {
    V1(EncryptedKeyBlobV1),
    // Future versions go here...
}

impl AsCborValue for EncryptedKeyBlob {
    fn from_cbor_value(value: cbor::value::Value) -> Result<Self, CborError> {
        let mut a = match value {
            cbor::value::Value::Array(a) if a.len() == 2 => a,
            _ => return cbor_type_error(&value, "arr len 2"),
        };
        let inner = a.remove(1);
        let version = Version::from_cbor_value(a.remove(0))?;
        match version {
            Version::V1 => Ok(Self::V1(EncryptedKeyBlobV1::from_cbor_value(inner)?)),
        }
    }
    fn to_cbor_value(self) -> Result<cbor::value::Value, CborError> {
        Ok(match self {
            EncryptedKeyBlob::V1(inner) => cbor::value::Value::Array(vec![
                Version::V1.to_cbor_value()?,
                inner.to_cbor_value()?,
            ]),
        })
    }
    fn cddl_typename() -> Option<String> {
        Some("EncryptedKeyBlob".to_string())
    }
    fn cddl_schema() -> Option<String> {
        Some(format!(
            "&(
    [{}, {}] ; Version::V1
)",
            Version::V1 as i32,
            EncryptedKeyBlobV1::cddl_ref()
        ))
    }
}

/// Encrypted key material, as translated to/from CBOR.
#[derive(Clone, Debug, AsCborValue)]
pub struct EncryptedKeyBlobV1 {
    /// Characteristics associated with the key.
    pub characteristics: Vec<KeyCharacteristics>,
    /// Nonce used for the key derivation.
    pub key_derivation_input: [u8; 32],
    /// Key material encrypted with AES-GCM with:
    ///  - key produced by [`derive_kek`]
    ///  - plaintext is the CBOR-serialization of [`crypto::PlaintextKeyMaterial`]
    ///  - nonce is all zeroes
    ///  - no additional data.
    pub encrypted_key_material: coset::CoseEncrypt0,
    /// Identifier for a slot in secure storage that holds additional secret values
    /// that are required to derive the key encryption key.
    pub secure_deletion_slot: Option<SecureDeletionSlot>,
}

// Implement the local `AsCborValue` trait for `coset::CoseEncrypt0` ensuring/requiring
// use of the relevant CBOR tag.
impl AsCborValue for coset::CoseEncrypt0 {
    fn from_cbor_value(value: cbor::value::Value) -> Result<Self, CborError> {
        match value {
            cbor::value::Value::Tag(tag, inner_value) if tag == coset::CoseEncrypt0::TAG => {
                <coset::CoseEncrypt0 as coset::AsCborValue>::from_cbor_value(*inner_value)
                    .map_err(|e| e.into())
            }
            cbor::value::Value::Tag(_, _) => Err(CborError::UnexpectedItem("tag", "tag 16")),
            _ => cbor_type_error(&value, "tag 16"),
        }
    }
    fn to_cbor_value(self) -> Result<cbor::value::Value, CborError> {
        Ok(cbor::value::Value::Tag(
            coset::CoseEncrypt0::TAG,
            alloc::boxed::Box::new(coset::AsCborValue::to_cbor_value(self)?),
        ))
    }
    fn cddl_schema() -> Option<String> {
        Some(format!("#6.{}(Cose_Encrypt0)", coset::CoseEncrypt0::TAG))
    }
}

/// Secret data that can be mixed into the key derivation inputs for keys that require secure
/// deletion support; if the secret data is lost, the key is effectively deleted because the
/// key encryption key for the keyblob cannot be re-derived.
#[derive(Clone, PartialEq, Eq, AsCborValue)]
pub struct SecureDeletionData {
    /// Secret value that is wiped on factory reset.
    pub factory_reset_secret: [u8; 32],
    /// Per-key secret value that is wiped on key deletion.
    pub secure_deletion_secret: [u8; 16],
}

/// Manager for the mapping between secure deletion slots and the corresponding
/// [`SecureDeletionData`] instances.
pub trait SecureDeletionSecretManager {
    /// Find an empty slot, populate it with a fresh [`SecureDeletionData`] and return the slot.
    fn new_secret(
        &mut self,
        rng: &mut dyn crypto::Rng,
    ) -> Result<(SecureDeletionSlot, SecureDeletionData), Error>;

    /// Retrieve a [`SecureDeletionData`] identified by `slot`.
    fn get_secret(&self, slot: SecureDeletionSlot) -> Result<SecureDeletionData, Error>;

    /// Delete the [`SecureDeletionData`] identified by `slot`.
    fn delete_secret(&mut self, slot: SecureDeletionSlot) -> Result<(), Error>;

    /// Delete all secure deletion data.
    fn delete_all(&mut self);
}

/// RAII class to hold a secure deletion slot.  The slot is deleted when the holder is dropped.
struct SlotHolder<'a> {
    mgr: &'a mut dyn SecureDeletionSecretManager,
    slot: Option<SecureDeletionSlot>,
}

impl Drop for SlotHolder<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            if let Err(e) = self.mgr.delete_secret(slot) {
                error!("Failed to delete recently-acquired SDD slot {:?}: {:?}", slot, e);
            }
        }
    }
}

impl<'a> SlotHolder<'a> {
    /// Reserve a new secure deletion slot.
    fn new(
        mgr: &'a mut dyn SecureDeletionSecretManager,
        rng: &mut dyn crypto::Rng,
    ) -> Result<(Self, SecureDeletionData), Error> {
        let (slot, sdd) = mgr.new_secret(rng)?;
        Ok((Self { mgr, slot: Some(slot) }, sdd))
    }

    /// Acquire ownership of the secure deletion slot.
    fn consume(mut self) -> SecureDeletionSlot {
        self.slot.take().unwrap()
    }
}

/// Root of trust information for binding into keyblobs.
#[derive(Debug, Clone, AsCborValue)]
pub struct RootOfTrustInfo {
    pub verified_boot_key: [u8; 32],
    pub device_boot_locked: bool,
    pub verified_boot_state: VerifiedBootState,
    pub verified_boot_hash: [u8; 32],
}

/// Derive a key encryption key used for key blob encryption. The key is an AES-256 key derived
/// from `root_key` using HKDF (RFC 5869) with HMAC-SHA256:
/// - input keying material = a root key held in hardware
/// - salt = absent
/// - info = the following three or four chunks of context data concatenated:
///    - content of `key_derivation_input` (which is random data)
///    - CBOR-serialization of `characteristics`
///    - CBOR-serialized array of additional `KeyParam` items in `hidden`
///    - (if `sdd` provided) CBOR serialization of the `SecureDeletionData`
pub fn derive_kek(
    hmac: &dyn crypto::Hmac,
    root_key: &[u8],
    key_derivation_input: &[u8; 32],
    characteristics: Vec<KeyCharacteristics>,
    hidden: Vec<KeyParam>,
    sdd: Option<SecureDeletionData>,
) -> Result<crypto::aes::Key, Error> {
    let mut info = key_derivation_input.to_vec();
    info.extend_from_slice(&characteristics.into_vec()?);
    info.extend_from_slice(&hidden.into_vec()?);
    if let Some(sdd) = sdd {
        info.extend_from_slice(&sdd.into_vec()?);
    }
    let data = crypto::hkdf::<32>(hmac, &[], root_key, &info)?;
    Ok(crypto::aes::Key::Aes256(data))
}

/// Plaintext key blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaintextKeyBlob {
    /// Characteristics associated with the key.
    pub characteristics: Vec<KeyCharacteristics>,
    /// Key Material
    pub key_material: crypto::PlaintextKeyMaterial,
}

pub fn characteristics_at(
    chars: &[KeyCharacteristics],
    sec_level: SecurityLevel,
) -> Result<&[KeyParam], Error> {
    for chars in chars {
        if chars.security_level == sec_level {
            return Ok(&chars.authorizations);
        }
    }
    Err(km_err!(InvalidKeyBlob, "no parameters at security level {:?} found", sec_level))
}

impl PlaintextKeyBlob {
    /// Return the set of key parameters at the provided security level. This method assumes that
    /// the characteristics are well-formed (i.e. do not have duplicate entries for the same
    /// security level).
    // TODO: add code to police this when parsing externally-provided keyblobs
    pub fn characteristics_at(&self, sec_level: SecurityLevel) -> Result<&[KeyParam], Error> {
        characteristics_at(&self.characteristics, sec_level)
    }

    /// Check that the key is suitable for the given purpose.
    pub fn suitable_for(&self, purpose: KeyPurpose, sec_level: SecurityLevel) -> Result<(), Error> {
        if contains_tag_value!(self.characteristics_at(sec_level)?, Purpose, purpose) {
            Ok(())
        } else {
            Err(km_err!(IncompatiblePurpose, "purpose {:?} not supported by keyblob", purpose))
        }
    }
}

/// Consume a plaintext keyblob and emit an encrypted version.  If `sdd_mgr` is provided,
/// a secure deletion slot will be embedded into the keyblob.
pub fn encrypt(
    sdd_mgr: Option<&mut dyn SecureDeletionSecretManager>,
    aes: &dyn crypto::Aes,
    hmac: &dyn crypto::Hmac,
    rng: &mut dyn crypto::Rng,
    root_key: &[u8],
    plaintext_keyblob: PlaintextKeyBlob,
    hidden: Vec<KeyParam>,
) -> Result<EncryptedKeyBlob, Error> {
    // Determine if secure deletion is required.
    let requires_sdd = (&plaintext_keyblob.characteristics)
        .iter()
        .flat_map(|chars| chars.authorizations.iter())
        .any(|param| matches!(param, KeyParam::RollbackResistance | KeyParam::UsageCountLimit(1)));
    let (slot_holder, sdd) = match (requires_sdd, sdd_mgr) {
        (true, Some(sdd_mgr)) => {
            // Store the reserved slot in a [`SlotHolder`] so that it will definitely
            // be released if there are any errors encountered below.
            let (holder, sdd) = SlotHolder::new(sdd_mgr, rng)?;
            (Some(holder), Some(sdd))
        }
        (true, None) => {
            return Err(km_err!(
                RollbackResistanceUnavailable,
                "no secure secret storage available"
            ))
        }
        (false, _) => (None, None),
    };
    let characteristics = plaintext_keyblob.characteristics;
    let mut key_derivation_input = [0u8; 32];
    rng.fill_bytes(&mut key_derivation_input[..]);
    let kek =
        derive_kek(hmac, root_key, &key_derivation_input, characteristics.clone(), hidden, sdd)?;

    // Encrypt the plaintext key material into a `Cose_Encrypt0` structure.
    let cose_encrypt = coset::CoseEncrypt0Builder::new()
        .protected(coset::HeaderBuilder::new().algorithm(coset::iana::Algorithm::A256GCM).build())
        .try_create_ciphertext::<_, Error>(
            &plaintext_keyblob.key_material.into_vec()?,
            &[],
            move |pt, aad| {
                let mut op = aes.begin_aead(
                    kek,
                    crypto::aes::GcmMode::GcmTag16 { nonce: ZERO_NONCE },
                    crypto::SymmetricOperation::Encrypt,
                )?;
                op.update_aad(aad)?;
                let mut ct = op.update(pt)?;
                ct.extend_from_slice(&op.finish()?);
                Ok(ct)
            },
        )?
        .build();

    Ok(EncryptedKeyBlob::V1(EncryptedKeyBlobV1 {
        characteristics,
        key_derivation_input,
        encrypted_key_material: cose_encrypt,
        secure_deletion_slot: slot_holder.map(|h| h.consume()),
    }))
}

/// Consume an encrypted keyblob and emit an decrypted version.
pub fn decrypt(
    sdd_mgr: Option<&dyn SecureDeletionSecretManager>,
    aes: &dyn crypto::Aes,
    hmac: &dyn crypto::Hmac,
    root_key: &[u8],
    encrypted_keyblob: EncryptedKeyBlob,
    hidden: Vec<KeyParam>,
) -> Result<PlaintextKeyBlob, Error> {
    let EncryptedKeyBlob::V1(encrypted_keyblob) = encrypted_keyblob;
    let sdd = match (encrypted_keyblob.secure_deletion_slot, sdd_mgr) {
        (Some(slot), Some(sdd_mgr)) => Some(sdd_mgr.get_secret(slot)?),
        (Some(_slot), None) => {
            return Err(km_err!(
                InvalidKeyBlob,
                "keyblob has sdd slot but no secure storage available"
            ))
        }
        (None, _) => None,
    };
    let characteristics = encrypted_keyblob.characteristics;
    let kek = derive_kek(
        hmac,
        root_key,
        &encrypted_keyblob.key_derivation_input,
        characteristics.clone(),
        hidden,
        sdd,
    )?;
    let cose_encrypt = encrypted_keyblob.encrypted_key_material;

    let extended_aad = coset::enc_structure_data(
        coset::EncryptionContext::CoseEncrypt0,
        cose_encrypt.protected.clone(),
        &[], // no external AAD
    );

    let mut op = aes.begin_aead(
        kek,
        crypto::aes::GcmMode::GcmTag16 { nonce: ZERO_NONCE },
        crypto::SymmetricOperation::Decrypt,
    )?;
    op.update_aad(&extended_aad)?;
    let mut pt_data = op.update(&cose_encrypt.ciphertext.unwrap_or_default())?;
    pt_data.extend_from_slice(&op.finish()?);

    Ok(PlaintextKeyBlob {
        characteristics,
        key_material: <crypto::PlaintextKeyMaterial>::from_slice(&pt_data)?,
    })
}