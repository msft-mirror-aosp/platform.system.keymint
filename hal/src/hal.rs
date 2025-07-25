// Copyright 2022, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Code for dealing with HAL-defined types, especially conversions to/from internal types.
//!
//! The internal code for KeyMint uses its own type definitions, not the HAL-defined autogenerated
//! types, for two reasons:
//!
//! - The auto-generated types impose a dependency on Binder which is not appropriate for
//!   code being built for a secure environment.
//! - The auto-generated types are not idiomatic Rust, and have reduced type safety.
//!
//! This module includes code to convert between HAL types (re-used under `kmr_hal::hal`) and
//! internal types (under `kmr_wire`), via the [`Fromm`] / [`TryFromm`], [`Innto`] and
//! [`TryInnto`] traits (which are deliberately misspelled to avoid a clash with standard
//! traits -- see below).
//!
//! - Going from wire=>HAL is an infallible conversion, as the wire types are stricter.
//! - Going from HAL=>wire is often a fallible conversion, as there may be "enum" values
//!   that are not in range.
//!
//! This module (and `kmr_wire`) must be kept in sync with the Android KeyMint HAL definition.

#![allow(non_snake_case)]

use crate::binder;
use keymint::{KeyParameterValue::KeyParameterValue, Tag::Tag, TagType::TagType};
use kmr_wire as wire;
use kmr_wire::{keymint::DateTime, keymint::KeyParam, KeySizeInBits, RsaExponent};
use log::{error, warn};
use std::convert::TryFrom;
use std::ffi::CString;

pub use android_hardware_security_keymint::aidl::android::hardware::security::keymint;
pub use android_hardware_security_rkp::aidl::android::hardware::security::keymint as rkp;
pub use android_hardware_security_secureclock::aidl::android::hardware::security::secureclock;
pub use android_hardware_security_sharedsecret::aidl::android::hardware::security::sharedsecret;

#[cfg(test)]
mod tests;

/// Emit a failure for a failed type conversion.
#[inline]
pub fn failed_conversion(err: wire::ValueNotRecognized) -> binder::Status {
    // If conversion from a HAL type failed because an enum value was unrecognized, try to use a
    // more specific error code.
    let errcode = match err {
        wire::ValueNotRecognized::KeyPurpose => keymint::ErrorCode::ErrorCode::UNSUPPORTED_PURPOSE,
        wire::ValueNotRecognized::Algorithm => keymint::ErrorCode::ErrorCode::UNSUPPORTED_ALGORITHM,
        wire::ValueNotRecognized::BlockMode => {
            keymint::ErrorCode::ErrorCode::UNSUPPORTED_BLOCK_MODE
        }
        wire::ValueNotRecognized::PaddingMode => {
            keymint::ErrorCode::ErrorCode::UNSUPPORTED_PADDING_MODE
        }
        wire::ValueNotRecognized::Digest => keymint::ErrorCode::ErrorCode::UNSUPPORTED_DIGEST,
        wire::ValueNotRecognized::KeyFormat => {
            keymint::ErrorCode::ErrorCode::UNSUPPORTED_KEY_FORMAT
        }
        wire::ValueNotRecognized::EcCurve => keymint::ErrorCode::ErrorCode::UNSUPPORTED_EC_CURVE,
        _ => keymint::ErrorCode::ErrorCode::INVALID_ARGUMENT,
    };
    binder::Status::new_service_specific_error(
        errcode.0,
        Some(&CString::new("conversion from HAL type to internal type failed").unwrap()),
    )
}

/// Determine the tag type for a tag, based on the top 4 bits of the tag number.
pub fn tag_type(tag: Tag) -> TagType {
    match ((tag.0 as u32) & 0xf0000000u32) as i32 {
        x if x == TagType::ENUM.0 => TagType::ENUM,
        x if x == TagType::ENUM_REP.0 => TagType::ENUM_REP,
        x if x == TagType::UINT.0 => TagType::UINT,
        x if x == TagType::UINT_REP.0 => TagType::UINT_REP,
        x if x == TagType::ULONG.0 => TagType::ULONG,
        x if x == TagType::DATE.0 => TagType::DATE,
        x if x == TagType::BOOL.0 => TagType::BOOL,
        x if x == TagType::BIGNUM.0 => TagType::BIGNUM,
        x if x == TagType::BYTES.0 => TagType::BYTES,
        x if x == TagType::ULONG_REP.0 => TagType::ULONG_REP,
        _ => TagType::INVALID,
    }
}

// Neither the `kmr_wire` types nor the `hal` types are local to this crate, which means that Rust's
// orphan rule means we cannot implement the standard conversion traits.  So instead define our own
// equivalent conversion traits that are local, and for which we're allowed to provide
// implementations.  Give them an odd name to avoid confusion with the standard traits.

/// Local equivalent of `From` trait, with a different name to avoid clashes.
pub trait Fromm<T>: Sized {
    /// Convert `val` into type `Self`.
    fn fromm(val: T) -> Self;
}
/// Local equivalent of `TryFrom` trait, with a different name to avoid clashes.
pub trait TryFromm<T>: Sized {
    /// Error type emitted on conversion failure.
    type Error;
    /// Try to convert `val` into type `Self`.
    fn try_fromm(val: T) -> Result<Self, Self::Error>;
}
/// Local equivalent of `Into` trait, with a different name to avoid clashes.
pub trait Innto<T> {
    /// Convert `self` into type `T`.
    fn innto(self) -> T;
}
/// Local equivalent of `TryInto` trait, with a different name to avoid clashes.
pub trait TryInnto<T> {
    /// Error type emitted on conversion failure.
    type Error;
    /// Try to convert `self` into type `T`.
    fn try_innto(self) -> Result<T, Self::Error>;
}
/// Blanket implementation of `Innto` from `Fromm`
impl<T, U> Innto<U> for T
where
    U: Fromm<T>,
{
    fn innto(self) -> U {
        U::fromm(self)
    }
}
/// Blanket implementation of `TryInnto` from `TryFromm`
impl<T, U> TryInnto<U> for T
where
    U: TryFromm<T>,
{
    type Error = U::Error;
    fn try_innto(self) -> Result<U, Self::Error> {
        U::try_fromm(self)
    }
}
/// Blanket implementation of `Fromm<Vec<T>>` from `Fromm<T>`
impl<T, U> Fromm<Vec<T>> for Vec<U>
where
    U: Fromm<T>,
{
    fn fromm(val: Vec<T>) -> Vec<U> {
        val.into_iter().map(|t| <U>::fromm(t)).collect()
    }
}

// Conversions from `kmr_wire` types into the equivalent types in the auto-generated HAL code. These
// conversions are infallible, because the range of the `wire` types is strictly contained within
// the HAL types.

impl Fromm<wire::sharedsecret::SharedSecretParameters>
    for sharedsecret::SharedSecretParameters::SharedSecretParameters
{
    fn fromm(val: wire::sharedsecret::SharedSecretParameters) -> Self {
        Self { seed: val.seed, nonce: val.nonce }
    }
}
impl Fromm<wire::secureclock::Timestamp> for secureclock::Timestamp::Timestamp {
    fn fromm(val: wire::secureclock::Timestamp) -> Self {
        Self { milliSeconds: val.milliseconds }
    }
}
impl Fromm<wire::secureclock::TimeStampToken> for secureclock::TimeStampToken::TimeStampToken {
    fn fromm(val: wire::secureclock::TimeStampToken) -> Self {
        Self { challenge: val.challenge, timestamp: val.timestamp.innto(), mac: val.mac }
    }
}
impl Fromm<wire::keymint::Certificate> for keymint::Certificate::Certificate {
    fn fromm(val: wire::keymint::Certificate) -> Self {
        Self { encodedCertificate: val.encoded_certificate }
    }
}
impl Fromm<wire::rpc::DeviceInfo> for rkp::DeviceInfo::DeviceInfo {
    fn fromm(val: wire::rpc::DeviceInfo) -> Self {
        Self { deviceInfo: val.device_info }
    }
}
impl Fromm<wire::keymint::HardwareAuthToken> for keymint::HardwareAuthToken::HardwareAuthToken {
    fn fromm(val: wire::keymint::HardwareAuthToken) -> Self {
        Self {
            challenge: val.challenge,
            userId: val.user_id,
            authenticatorId: val.authenticator_id,
            authenticatorType: val.authenticator_type.innto(),
            timestamp: val.timestamp.innto(),
            mac: val.mac,
        }
    }
}
impl Fromm<wire::keymint::KeyCharacteristics> for keymint::KeyCharacteristics::KeyCharacteristics {
    fn fromm(val: wire::keymint::KeyCharacteristics) -> Self {
        Self {
            securityLevel: val.security_level.innto(),
            authorizations: val.authorizations.innto(),
        }
    }
}
impl Fromm<wire::keymint::KeyCreationResult> for keymint::KeyCreationResult::KeyCreationResult {
    fn fromm(val: wire::keymint::KeyCreationResult) -> Self {
        Self {
            keyBlob: val.key_blob,
            keyCharacteristics: val.key_characteristics.innto(),
            certificateChain: val.certificate_chain.innto(),
        }
    }
}
impl Fromm<wire::keymint::KeyMintHardwareInfo>
    for keymint::KeyMintHardwareInfo::KeyMintHardwareInfo
{
    fn fromm(val: wire::keymint::KeyMintHardwareInfo) -> Self {
        Self {
            versionNumber: val.version_number,
            securityLevel: val.security_level.innto(),
            keyMintName: val.key_mint_name,
            keyMintAuthorName: val.key_mint_author_name,
            timestampTokenRequired: val.timestamp_token_required,
        }
    }
}
impl Fromm<wire::rpc::MacedPublicKey> for rkp::MacedPublicKey::MacedPublicKey {
    fn fromm(val: wire::rpc::MacedPublicKey) -> Self {
        Self { macedKey: val.maced_key }
    }
}
impl Fromm<wire::rpc::ProtectedData> for rkp::ProtectedData::ProtectedData {
    fn fromm(val: wire::rpc::ProtectedData) -> Self {
        Self { protectedData: val.protected_data }
    }
}
impl Fromm<wire::rpc::HardwareInfo> for rkp::RpcHardwareInfo::RpcHardwareInfo {
    fn fromm(val: wire::rpc::HardwareInfo) -> Self {
        Self {
            versionNumber: val.version_number,
            rpcAuthorName: val.rpc_author_name,
            supportedEekCurve: val.supported_eek_curve as i32,
            uniqueId: val.unique_id,
            supportedNumKeysInCsr: val.supported_num_keys_in_csr,
        }
    }
}

impl Fromm<wire::keymint::KeyParam> for keymint::KeyParameter::KeyParameter {
    fn fromm(val: wire::keymint::KeyParam) -> Self {
        let (tag, value) = match val {
            // Enum-holding variants.
            KeyParam::Purpose(v) => (Tag::PURPOSE, KeyParameterValue::KeyPurpose(v.innto())),
            KeyParam::Algorithm(v) => (Tag::ALGORITHM, KeyParameterValue::Algorithm(v.innto())),
            KeyParam::BlockMode(v) => (Tag::BLOCK_MODE, KeyParameterValue::BlockMode(v.innto())),
            KeyParam::Digest(v) => (Tag::DIGEST, KeyParameterValue::Digest(v.innto())),
            KeyParam::Padding(v) => (Tag::PADDING, KeyParameterValue::PaddingMode(v.innto())),
            KeyParam::EcCurve(v) => (Tag::EC_CURVE, KeyParameterValue::EcCurve(v.innto())),
            KeyParam::RsaOaepMgfDigest(v) => {
                (Tag::RSA_OAEP_MGF_DIGEST, KeyParameterValue::Digest(v.innto()))
            }
            KeyParam::Origin(v) => (Tag::ORIGIN, KeyParameterValue::Origin(v.innto())),

            // `u32`-holding variants.
            KeyParam::KeySize(v) => (Tag::KEY_SIZE, KeyParameterValue::Integer(v.0 as i32)),
            KeyParam::MinMacLength(v) => {
                (Tag::MIN_MAC_LENGTH, KeyParameterValue::Integer(v as i32))
            }
            KeyParam::MaxUsesPerBoot(v) => {
                (Tag::MAX_USES_PER_BOOT, KeyParameterValue::Integer(v as i32))
            }
            KeyParam::UsageCountLimit(v) => {
                (Tag::USAGE_COUNT_LIMIT, KeyParameterValue::Integer(v as i32))
            }
            KeyParam::UserId(v) => (Tag::USER_ID, KeyParameterValue::Integer(v as i32)),
            KeyParam::UserAuthType(v) => {
                // Special case: auth type is a bitmask, so the Rust types use `u32` but the HAL
                // type has an "enum".
                (
                    Tag::USER_AUTH_TYPE,
                    KeyParameterValue::HardwareAuthenticatorType(
                        keymint::HardwareAuthenticatorType::HardwareAuthenticatorType(v as i32),
                    ),
                )
            }
            KeyParam::AuthTimeout(v) => (Tag::AUTH_TIMEOUT, KeyParameterValue::Integer(v as i32)),
            KeyParam::OsVersion(v) => (Tag::OS_VERSION, KeyParameterValue::Integer(v as i32)),
            KeyParam::OsPatchlevel(v) => (Tag::OS_PATCHLEVEL, KeyParameterValue::Integer(v as i32)),
            KeyParam::VendorPatchlevel(v) => {
                (Tag::VENDOR_PATCHLEVEL, KeyParameterValue::Integer(v as i32))
            }
            KeyParam::BootPatchlevel(v) => {
                (Tag::BOOT_PATCHLEVEL, KeyParameterValue::Integer(v as i32))
            }
            KeyParam::MacLength(v) => (Tag::MAC_LENGTH, KeyParameterValue::Integer(v as i32)),
            KeyParam::MaxBootLevel(v) => {
                (Tag::MAX_BOOT_LEVEL, KeyParameterValue::Integer(v as i32))
            }

            // `u64`-holding variants.
            KeyParam::RsaPublicExponent(v) => {
                (Tag::RSA_PUBLIC_EXPONENT, KeyParameterValue::LongInteger(v.0 as i64))
            }
            KeyParam::UserSecureId(v) => {
                (Tag::USER_SECURE_ID, KeyParameterValue::LongInteger(v as i64))
            }

            // `true`-holding variants.
            KeyParam::CallerNonce => (Tag::CALLER_NONCE, KeyParameterValue::BoolValue(true)),
            KeyParam::IncludeUniqueId => {
                (Tag::INCLUDE_UNIQUE_ID, KeyParameterValue::BoolValue(true))
            }
            KeyParam::BootloaderOnly => (Tag::BOOTLOADER_ONLY, KeyParameterValue::BoolValue(true)),
            KeyParam::RollbackResistance => {
                (Tag::ROLLBACK_RESISTANCE, KeyParameterValue::BoolValue(true))
            }
            KeyParam::EarlyBootOnly => (Tag::EARLY_BOOT_ONLY, KeyParameterValue::BoolValue(true)),
            KeyParam::AllowWhileOnBody => {
                (Tag::ALLOW_WHILE_ON_BODY, KeyParameterValue::BoolValue(true))
            }
            KeyParam::NoAuthRequired => (Tag::NO_AUTH_REQUIRED, KeyParameterValue::BoolValue(true)),
            KeyParam::TrustedUserPresenceRequired => {
                (Tag::TRUSTED_USER_PRESENCE_REQUIRED, KeyParameterValue::BoolValue(true))
            }
            KeyParam::TrustedConfirmationRequired => {
                (Tag::TRUSTED_CONFIRMATION_REQUIRED, KeyParameterValue::BoolValue(true))
            }
            KeyParam::UnlockedDeviceRequired => {
                (Tag::UNLOCKED_DEVICE_REQUIRED, KeyParameterValue::BoolValue(true))
            }
            KeyParam::DeviceUniqueAttestation => {
                (Tag::DEVICE_UNIQUE_ATTESTATION, KeyParameterValue::BoolValue(true))
            }
            KeyParam::StorageKey => (Tag::STORAGE_KEY, KeyParameterValue::BoolValue(true)),
            KeyParam::ResetSinceIdRotation => {
                (Tag::RESET_SINCE_ID_ROTATION, KeyParameterValue::BoolValue(true))
            }

            // `DateTime`-holding variants.
            KeyParam::ActiveDatetime(v) => {
                (Tag::ACTIVE_DATETIME, KeyParameterValue::DateTime(v.ms_since_epoch))
            }
            KeyParam::OriginationExpireDatetime(v) => {
                (Tag::ORIGINATION_EXPIRE_DATETIME, KeyParameterValue::DateTime(v.ms_since_epoch))
            }
            KeyParam::UsageExpireDatetime(v) => {
                (Tag::USAGE_EXPIRE_DATETIME, KeyParameterValue::DateTime(v.ms_since_epoch))
            }
            KeyParam::CreationDatetime(v) => {
                (Tag::CREATION_DATETIME, KeyParameterValue::DateTime(v.ms_since_epoch))
            }
            KeyParam::CertificateNotBefore(v) => {
                (Tag::CERTIFICATE_NOT_BEFORE, KeyParameterValue::DateTime(v.ms_since_epoch))
            }
            KeyParam::CertificateNotAfter(v) => {
                (Tag::CERTIFICATE_NOT_AFTER, KeyParameterValue::DateTime(v.ms_since_epoch))
            }

            // `Vec<u8>`-holding variants.
            KeyParam::ApplicationId(v) => (Tag::APPLICATION_ID, KeyParameterValue::Blob(v)),
            KeyParam::ApplicationData(v) => (Tag::APPLICATION_DATA, KeyParameterValue::Blob(v)),
            KeyParam::AttestationChallenge(v) => {
                (Tag::ATTESTATION_CHALLENGE, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationApplicationId(v) => {
                (Tag::ATTESTATION_APPLICATION_ID, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdBrand(v) => {
                (Tag::ATTESTATION_ID_BRAND, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdDevice(v) => {
                (Tag::ATTESTATION_ID_DEVICE, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdProduct(v) => {
                (Tag::ATTESTATION_ID_PRODUCT, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdSerial(v) => {
                (Tag::ATTESTATION_ID_SERIAL, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdImei(v) => {
                (Tag::ATTESTATION_ID_IMEI, KeyParameterValue::Blob(v))
            }
            #[cfg(feature = "hal_v3")]
            KeyParam::AttestationIdSecondImei(v) => {
                (Tag::ATTESTATION_ID_SECOND_IMEI, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdMeid(v) => {
                (Tag::ATTESTATION_ID_MEID, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdManufacturer(v) => {
                (Tag::ATTESTATION_ID_MANUFACTURER, KeyParameterValue::Blob(v))
            }
            KeyParam::AttestationIdModel(v) => {
                (Tag::ATTESTATION_ID_MODEL, KeyParameterValue::Blob(v))
            }
            KeyParam::Nonce(v) => (Tag::NONCE, KeyParameterValue::Blob(v)),
            KeyParam::RootOfTrust(v) => (Tag::ROOT_OF_TRUST, KeyParameterValue::Blob(v)),
            KeyParam::CertificateSerial(v) => (Tag::CERTIFICATE_SERIAL, KeyParameterValue::Blob(v)),
            KeyParam::CertificateSubject(v) => {
                (Tag::CERTIFICATE_SUBJECT, KeyParameterValue::Blob(v))
            }
            #[cfg(feature = "hal_v4")]
            KeyParam::ModuleHash(v) => (Tag::MODULE_HASH, KeyParameterValue::Blob(v)),
        };
        Self { tag, value }
    }
}

// Conversions from auto-generated HAL types into the equivalent types from `kmr_wire`.  These
// conversions are generally fallible, because the "enum" types generated for the HAL are actually
// `i32` values, which may contain invalid values.

impl Fromm<secureclock::TimeStampToken::TimeStampToken> for wire::secureclock::TimeStampToken {
    fn fromm(val: secureclock::TimeStampToken::TimeStampToken) -> Self {
        Self { challenge: val.challenge, timestamp: val.timestamp.innto(), mac: val.mac }
    }
}
impl Fromm<secureclock::Timestamp::Timestamp> for wire::secureclock::Timestamp {
    fn fromm(val: secureclock::Timestamp::Timestamp) -> Self {
        Self { milliseconds: val.milliSeconds }
    }
}
impl Fromm<sharedsecret::SharedSecretParameters::SharedSecretParameters>
    for wire::sharedsecret::SharedSecretParameters
{
    fn fromm(val: sharedsecret::SharedSecretParameters::SharedSecretParameters) -> Self {
        Self { seed: val.seed, nonce: val.nonce }
    }
}
impl TryFromm<keymint::AttestationKey::AttestationKey> for wire::keymint::AttestationKey {
    type Error = wire::ValueNotRecognized;
    fn try_fromm(val: keymint::AttestationKey::AttestationKey) -> Result<Self, Self::Error> {
        Ok(Self {
            key_blob: val.keyBlob,
            attest_key_params: val
                .attestKeyParams // Vec<KeyParameter>
                .into_iter() // Iter<KeyParameter>
                .filter_map(|p| (&p).try_innto().transpose())
                .collect::<Result<Vec<KeyParam>, _>>()?,
            issuer_subject_name: val.issuerSubjectName,
        })
    }
}
impl TryFromm<keymint::HardwareAuthToken::HardwareAuthToken> for wire::keymint::HardwareAuthToken {
    type Error = wire::ValueNotRecognized;
    fn try_fromm(val: keymint::HardwareAuthToken::HardwareAuthToken) -> Result<Self, Self::Error> {
        Ok(Self {
            challenge: val.challenge,
            user_id: val.userId,
            authenticator_id: val.authenticatorId,
            authenticator_type: val.authenticatorType.try_innto()?,
            timestamp: val.timestamp.innto(),
            mac: val.mac,
        })
    }
}
impl Fromm<rkp::MacedPublicKey::MacedPublicKey> for wire::rpc::MacedPublicKey {
    fn fromm(val: rkp::MacedPublicKey::MacedPublicKey) -> Self {
        Self { maced_key: val.macedKey }
    }
}
impl Fromm<&rkp::MacedPublicKey::MacedPublicKey> for wire::rpc::MacedPublicKey {
    fn fromm(val: &rkp::MacedPublicKey::MacedPublicKey) -> Self {
        Self { maced_key: val.macedKey.to_vec() }
    }
}

macro_rules! value_of {
    {
        $val:expr, $variant:ident
    } => {
        if let keymint::KeyParameterValue::KeyParameterValue::$variant(v) = $val.value {
            Ok(v)
        } else {
            error!("failed to convert parameter '{}' with value {:?}", stringify!($val), $val);
            Err(wire::ValueNotRecognized::$variant)
        }
    }
}

macro_rules! check_bool {
    {
        $val:expr
    } => {
        if let keymint::KeyParameterValue::KeyParameterValue::BoolValue(true) = $val.value {
            Ok(())
        } else {
            Err(wire::ValueNotRecognized::Bool)
        }
    }
}

macro_rules! clone_blob {
    {
        $val:expr
    } => {
        if let keymint::KeyParameterValue::KeyParameterValue::Blob(b) = &$val.value {
            Ok(b.clone())
        } else {
            Err(wire::ValueNotRecognized::Blob)
        }
    }
}

/// Converting a HAL `KeyParameter` to a wire `KeyParam` may fail (producing an `Err`) but may also
/// silently drop unknown tags (producing `Ok(None)`)
impl TryFromm<&keymint::KeyParameter::KeyParameter> for Option<KeyParam> {
    type Error = wire::ValueNotRecognized;
    fn try_fromm(val: &keymint::KeyParameter::KeyParameter) -> Result<Self, Self::Error> {
        Ok(match val.tag {
            // Enum-holding variants.
            keymint::Tag::Tag::PURPOSE => {
                Some(KeyParam::Purpose(value_of!(val, KeyPurpose)?.try_innto()?))
            }
            keymint::Tag::Tag::ALGORITHM => {
                Some(KeyParam::Algorithm(value_of!(val, Algorithm)?.try_innto()?))
            }
            keymint::Tag::Tag::BLOCK_MODE => {
                Some(KeyParam::BlockMode(value_of!(val, BlockMode)?.try_innto()?))
            }
            keymint::Tag::Tag::DIGEST => {
                Some(KeyParam::Digest(value_of!(val, Digest)?.try_innto()?))
            }
            keymint::Tag::Tag::PADDING => {
                Some(KeyParam::Padding(value_of!(val, PaddingMode)?.try_innto()?))
            }
            keymint::Tag::Tag::EC_CURVE => {
                Some(KeyParam::EcCurve(value_of!(val, EcCurve)?.try_innto()?))
            }
            keymint::Tag::Tag::RSA_OAEP_MGF_DIGEST => {
                Some(KeyParam::RsaOaepMgfDigest(value_of!(val, Digest)?.try_innto()?))
            }
            keymint::Tag::Tag::ORIGIN => {
                Some(KeyParam::Origin(value_of!(val, Origin)?.try_innto()?))
            }

            // Special case: although `Tag::USER_AUTH_TYPE` claims to have an associated enum, it's
            // actually a bitmask rather than an enum.
            keymint::Tag::Tag::USER_AUTH_TYPE => {
                let val = value_of!(val, HardwareAuthenticatorType)?;
                Some(KeyParam::UserAuthType(val.0 as u32))
            }

            // `u32`-holding variants.
            keymint::Tag::Tag::KEY_SIZE => {
                Some(KeyParam::KeySize(KeySizeInBits(value_of!(val, Integer)? as u32)))
            }
            keymint::Tag::Tag::MIN_MAC_LENGTH => {
                Some(KeyParam::MinMacLength(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::MAX_USES_PER_BOOT => {
                Some(KeyParam::MaxUsesPerBoot(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::USAGE_COUNT_LIMIT => {
                Some(KeyParam::UsageCountLimit(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::USER_ID => Some(KeyParam::UserId(value_of!(val, Integer)? as u32)),
            keymint::Tag::Tag::AUTH_TIMEOUT => {
                Some(KeyParam::AuthTimeout(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::OS_VERSION => {
                Some(KeyParam::OsVersion(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::OS_PATCHLEVEL => {
                Some(KeyParam::OsPatchlevel(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::VENDOR_PATCHLEVEL => {
                Some(KeyParam::VendorPatchlevel(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::BOOT_PATCHLEVEL => {
                Some(KeyParam::BootPatchlevel(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::MAC_LENGTH => {
                Some(KeyParam::MacLength(value_of!(val, Integer)? as u32))
            }
            keymint::Tag::Tag::MAX_BOOT_LEVEL => {
                Some(KeyParam::MaxBootLevel(value_of!(val, Integer)? as u32))
            }

            // `u64`-holding variants.
            keymint::Tag::Tag::RSA_PUBLIC_EXPONENT => {
                Some(KeyParam::RsaPublicExponent(RsaExponent(value_of!(val, LongInteger)? as u64)))
            }
            keymint::Tag::Tag::USER_SECURE_ID => {
                Some(KeyParam::UserSecureId(value_of!(val, LongInteger)? as u64))
            }

            // `bool`-holding variants; only `true` is allowed.
            keymint::Tag::Tag::CALLER_NONCE => {
                check_bool!(val)?;
                Some(KeyParam::CallerNonce)
            }
            keymint::Tag::Tag::INCLUDE_UNIQUE_ID => {
                check_bool!(val)?;
                Some(KeyParam::IncludeUniqueId)
            }
            keymint::Tag::Tag::BOOTLOADER_ONLY => {
                check_bool!(val)?;
                Some(KeyParam::BootloaderOnly)
            }
            keymint::Tag::Tag::ROLLBACK_RESISTANCE => {
                check_bool!(val)?;
                Some(KeyParam::RollbackResistance)
            }
            keymint::Tag::Tag::EARLY_BOOT_ONLY => {
                check_bool!(val)?;
                Some(KeyParam::EarlyBootOnly)
            }
            keymint::Tag::Tag::NO_AUTH_REQUIRED => {
                check_bool!(val)?;
                Some(KeyParam::NoAuthRequired)
            }
            keymint::Tag::Tag::ALLOW_WHILE_ON_BODY => {
                check_bool!(val)?;
                Some(KeyParam::AllowWhileOnBody)
            }
            keymint::Tag::Tag::TRUSTED_USER_PRESENCE_REQUIRED => {
                check_bool!(val)?;
                Some(KeyParam::TrustedUserPresenceRequired)
            }
            keymint::Tag::Tag::TRUSTED_CONFIRMATION_REQUIRED => {
                check_bool!(val)?;
                Some(KeyParam::TrustedConfirmationRequired)
            }
            keymint::Tag::Tag::UNLOCKED_DEVICE_REQUIRED => {
                check_bool!(val)?;
                Some(KeyParam::UnlockedDeviceRequired)
            }
            keymint::Tag::Tag::DEVICE_UNIQUE_ATTESTATION => {
                check_bool!(val)?;
                Some(KeyParam::DeviceUniqueAttestation)
            }
            keymint::Tag::Tag::STORAGE_KEY => {
                check_bool!(val)?;
                Some(KeyParam::StorageKey)
            }
            keymint::Tag::Tag::RESET_SINCE_ID_ROTATION => {
                check_bool!(val)?;
                Some(KeyParam::ResetSinceIdRotation)
            }

            // `DateTime`-holding variants.
            keymint::Tag::Tag::ACTIVE_DATETIME => Some(KeyParam::ActiveDatetime(DateTime {
                ms_since_epoch: value_of!(val, DateTime)?,
            })),
            keymint::Tag::Tag::ORIGINATION_EXPIRE_DATETIME => {
                Some(KeyParam::OriginationExpireDatetime(DateTime {
                    ms_since_epoch: value_of!(val, DateTime)?,
                }))
            }
            keymint::Tag::Tag::USAGE_EXPIRE_DATETIME => {
                Some(KeyParam::UsageExpireDatetime(DateTime {
                    ms_since_epoch: value_of!(val, DateTime)?,
                }))
            }
            keymint::Tag::Tag::CREATION_DATETIME => Some(KeyParam::CreationDatetime(DateTime {
                ms_since_epoch: value_of!(val, DateTime)?,
            })),
            keymint::Tag::Tag::CERTIFICATE_NOT_BEFORE => {
                Some(KeyParam::CertificateNotBefore(DateTime {
                    ms_since_epoch: value_of!(val, DateTime)?,
                }))
            }
            keymint::Tag::Tag::CERTIFICATE_NOT_AFTER => {
                Some(KeyParam::CertificateNotAfter(DateTime {
                    ms_since_epoch: value_of!(val, DateTime)?,
                }))
            }

            // `Vec<u8>`-holding variants.
            keymint::Tag::Tag::APPLICATION_ID => Some(KeyParam::ApplicationId(clone_blob!(val)?)),
            keymint::Tag::Tag::APPLICATION_DATA => {
                Some(KeyParam::ApplicationData(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ROOT_OF_TRUST => Some(KeyParam::RootOfTrust(clone_blob!(val)?)),
            keymint::Tag::Tag::ATTESTATION_CHALLENGE => {
                Some(KeyParam::AttestationChallenge(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_APPLICATION_ID => {
                Some(KeyParam::AttestationApplicationId(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_BRAND => {
                Some(KeyParam::AttestationIdBrand(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_DEVICE => {
                Some(KeyParam::AttestationIdDevice(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_PRODUCT => {
                Some(KeyParam::AttestationIdProduct(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_SERIAL => {
                Some(KeyParam::AttestationIdSerial(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_IMEI => {
                Some(KeyParam::AttestationIdImei(clone_blob!(val)?))
            }
            #[cfg(feature = "hal_v3")]
            keymint::Tag::Tag::ATTESTATION_ID_SECOND_IMEI => {
                Some(KeyParam::AttestationIdSecondImei(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_MEID => {
                Some(KeyParam::AttestationIdMeid(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_MANUFACTURER => {
                Some(KeyParam::AttestationIdManufacturer(clone_blob!(val)?))
            }
            keymint::Tag::Tag::ATTESTATION_ID_MODEL => {
                Some(KeyParam::AttestationIdModel(clone_blob!(val)?))
            }
            keymint::Tag::Tag::NONCE => Some(KeyParam::Nonce(clone_blob!(val)?)),
            keymint::Tag::Tag::CERTIFICATE_SERIAL => {
                Some(KeyParam::CertificateSerial(clone_blob!(val)?))
            }
            keymint::Tag::Tag::CERTIFICATE_SUBJECT => {
                Some(KeyParam::CertificateSubject(clone_blob!(val)?))
            }
            #[cfg(feature = "hal_v4")]
            keymint::Tag::Tag::MODULE_HASH => Some(KeyParam::ModuleHash(clone_blob!(val)?)),

            // Unsupported variants
            keymint::Tag::Tag::UNIQUE_ID
            | keymint::Tag::Tag::HARDWARE_TYPE
            | keymint::Tag::Tag::MIN_SECONDS_BETWEEN_OPS
            | keymint::Tag::Tag::IDENTITY_CREDENTIAL_KEY
            | keymint::Tag::Tag::ASSOCIATED_DATA
            | keymint::Tag::Tag::CONFIRMATION_TOKEN => {
                error!("Unsupported tag {:?} encountered", val.tag);
                return Err(wire::ValueNotRecognized::Tag);
            }
            _ => {
                warn!("Unknown tag {:?} silently dropped", val.tag);
                None
            }
        })
    }
}

/// Macro that emits conversion implementations for `wire` and HAL enums.
/// - The `hal::keymint` version of the enum is a newtype holding `i32`
/// - The `wire::keymint` version of the enum is an exhaustive enum with `[repr(i32)]`
macro_rules! enum_convert {
    {
        $wenum:ty => $henum:ty
    } => {
        impl Fromm<$wenum> for $henum {
            fn fromm(val: $wenum) -> Self {
                Self(val as i32)
            }
        }
        impl TryFromm<$henum> for $wenum {
            type Error = wire::ValueNotRecognized;
            fn try_fromm(val: $henum) -> Result<Self, Self::Error> {
                Self::try_from(val.0)
            }
        }
    };
}
enum_convert! { wire::keymint::ErrorCode => keymint::ErrorCode::ErrorCode }
enum_convert! { wire::keymint::Algorithm => keymint::Algorithm::Algorithm }
enum_convert! { wire::keymint::BlockMode => keymint::BlockMode::BlockMode }
enum_convert! { wire::keymint::Digest => keymint::Digest::Digest }
enum_convert! { wire::keymint::EcCurve => keymint::EcCurve::EcCurve }
enum_convert! { wire::keymint::HardwareAuthenticatorType =>
keymint::HardwareAuthenticatorType::HardwareAuthenticatorType }
enum_convert! { wire::keymint::KeyFormat => keymint::KeyFormat::KeyFormat }
enum_convert! { wire::keymint::KeyOrigin => keymint::KeyOrigin::KeyOrigin }
enum_convert! { wire::keymint::KeyPurpose => keymint::KeyPurpose::KeyPurpose }
enum_convert! { wire::keymint::PaddingMode => keymint::PaddingMode::PaddingMode }
enum_convert! { wire::keymint::SecurityLevel => keymint::SecurityLevel::SecurityLevel }
enum_convert! { wire::keymint::Tag => keymint::Tag::Tag }
enum_convert! { wire::keymint::TagType => keymint::TagType::TagType }
