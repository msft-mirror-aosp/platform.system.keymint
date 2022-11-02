//! KeyMint trusted application (TA) implementation.

#![no_std]
extern crate alloc;

use alloc::{
    collections::BTreeMap,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};
use core::cmp::Ordering;
use core::mem::size_of;
use core::{cell::RefCell, convert::TryFrom};
use kmr_common::{
    crypto::{self, RawKeyMaterial},
    keyblob::{self, RootOfTrustInfo, SecureDeletionSlot},
    km_err, tag, vec_try, vec_try_with_capacity, Error, FallibleAllocExt,
};
use kmr_derive::AsCborValue;
use kmr_wire::{
    coset::TaggedCborSerializable,
    keymint::{
        Digest, ErrorCode, HardwareAuthToken, KeyCharacteristics, KeyMintHardwareInfo, KeyOrigin,
        KeyParam, SecurityLevel, VerifiedBootState,
    },
    secureclock::{TimeStampToken, Timestamp},
    sharedsecret::SharedSecretParameters,
    *,
};
use log::{debug, error, info, warn};

mod cert;
mod clock;
pub mod device;
mod keys;
mod operation;
mod rkp;
mod secret;

use keys::KeyImport;
use operation::{OpHandle, Operation};

#[cfg(test)]
mod tests;

/// Maximum number of parallel operations supported when running as TEE.
const MAX_TEE_OPERATIONS: usize = 32;

/// Maximum number of parallel operations supported when running as StrongBox.
const MAX_STRONGBOX_OPERATIONS: usize = 4;

/// Maximum number of keys whose use count can be tracked.
const MAX_USE_COUNTED_KEYS: usize = 32;

/// Per-key ID use count.
struct UseCount {
    key_id: KeyId,
    count: u64,
}

/// Attestation chain information.
struct AttestationChainInfo {
    /// Chain of certificates from intermediate to root.
    chain: Vec<keymint::Certificate>,
    /// Subject field from the first certificate in the chain, as an ASN.1 DER encoded `Name` (cf
    /// RFC 5280 s4.1.2.4).
    issuer: Vec<u8>,
}

/// KeyMint device implementation, running in secure environment.
pub struct KeyMintTa<'a> {
    /**
     * State that is fixed on construction.
     */

    /// Trait objects that hold this device's implementations of the abstract cryptographic
    /// functionality traits.
    imp: crypto::Implementation<'a>,

    /// Trait objects that hold this device's implementations of per-device functionality.
    dev: device::Implementation<'a>,

    /// Information about this particular KeyMint implementation's hardware.
    hw_info: HardwareInfo,

    /**
     * State that is set after the TA starts, but latched thereafter.
     */

    /// Parameters for shared secret negotiation.
    shared_secret_params: Option<SharedSecretParameters>,

    /// Information provided by the bootloader once at start of day.
    boot_info: Option<BootInfo>,
    rot_data: Option<Vec<u8>>,

    /// Information provided by the HAL service once at start of day.
    hal_info: Option<HalInfo>,

    /// Attestation chain information, retrieved on first use.
    attestation_chain_info: RefCell<BTreeMap<device::SigningKeyType, AttestationChainInfo>>,

    /// Attestation ID information, fixed forever for a device, but retrieved on first use.
    attestation_id_info: RefCell<Option<Rc<AttestationIdInfo>>>,

    /// Whether the device is still in early-boot.
    in_early_boot: bool,

    /// Negotiated key for checking HMAC-ed data.
    hmac_key: Option<Vec<u8>>,

    /**
     * State that changes during operation.
     */

    /// Whether the device's screen is locked.
    device_locked: RefCell<LockState>,

    /// Challenge for root-of-trust transfer (StrongBox only).
    rot_challenge: [u8; 16],

    /// The operation table.
    operations: Vec<Option<Operation>>,

    /// Use counts for keys where this is tracked.
    use_count: [Option<UseCount>; MAX_USE_COUNTED_KEYS],

    /// Operation handle of the (single) in-flight operation that requires trusted user presence.
    presence_required_op: Option<OpHandle>,
}

/// Device lock state
#[derive(Clone, Copy, Debug)]
enum LockState {
    /// Device is unlocked.
    Unlocked,
    /// Device has been locked since the given time.
    LockedSince(Timestamp),
    /// Device has been locked since the given time, and can only be unlocked with a password
    /// (rather than a biometric).
    PasswordLockedSince(Timestamp),
}

/// Hardware information.
#[derive(Clone, Debug)]
pub struct HardwareInfo {
    // Fields that correspond to the HAL `KeyMintHardwareInfo` type.
    pub security_level: SecurityLevel,
    pub version_number: i32,
    pub impl_name: &'static str,
    pub author_name: &'static str,
    pub unique_id: &'static str,
    // The `timestamp_token_required` field in `KeyMintHardwareInfo` is skipped here because it gets
    // set depending on whether a local clock is available.

    // Indication of whether secure boot is enforced for the processor running this code.
    pub fused: bool, // Used as `DeviceInfo.fused` for RKP
}

/// Information provided once at start-of-day, normally by the bootloader.
///
/// Field order is fixed, to match the CBOR type definition of `RootOfTrust` in `IKeyMintDevice`.
#[derive(Clone, Debug, AsCborValue, PartialEq, Eq)]
pub struct BootInfo {
    pub verified_boot_key: [u8; 32],
    pub device_boot_locked: bool,
    pub verified_boot_state: VerifiedBootState,
    pub verified_boot_hash: [u8; 32],
    pub boot_patchlevel: u32, // YYYYMMDD format
}

// Implement the `coset` CBOR serialization traits in terms of the local `AsCborValue` trait,
// in order to get access to tagged versions of serialize/deserialize.
impl coset::AsCborValue for BootInfo {
    fn from_cbor_value(value: cbor::value::Value) -> coset::Result<Self> {
        <Self as AsCborValue>::from_cbor_value(value).map_err(|e| e.into())
    }
    fn to_cbor_value(self) -> coset::Result<cbor::value::Value> {
        <Self as AsCborValue>::to_cbor_value(self).map_err(|e| e.into())
    }
}

impl TaggedCborSerializable for BootInfo {
    const TAG: u64 = 40001;
}

/// Information provided once at service start by the HAL service, describing
/// the state of the userspace operating system (which may change from boot to
/// boot, e.g. for running GSI).
#[derive(Clone, Copy, Debug)]
pub struct HalInfo {
    pub os_version: u32,
    pub os_patchlevel: u32,     // YYYYMM format
    pub vendor_patchlevel: u32, // YYYYMMDD format
}

/// Identifier for a keyblob.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct KeyId([u8; 32]);

impl<'a> KeyMintTa<'a> {
    /// Create a new [`KeyMintTa`] instance.
    pub fn new(
        hw_info: HardwareInfo,
        imp: crypto::Implementation<'a>,
        dev: device::Implementation<'a>,
    ) -> Self {
        let max_operations = if hw_info.security_level == SecurityLevel::Strongbox {
            MAX_STRONGBOX_OPERATIONS
        } else {
            MAX_TEE_OPERATIONS
        };
        Self {
            imp,
            dev,
            in_early_boot: true,
            // TODO: figure out whether an initial locked state is possible
            device_locked: RefCell::new(LockState::Unlocked),
            hmac_key: None,
            rot_challenge: [0; 16],
            // Work around Rust limitation that `vec![None; n]` doesn't work.
            operations: (0..max_operations).map(|_| None).collect(),
            use_count: Default::default(),
            presence_required_op: None,
            shared_secret_params: None,
            hw_info,
            boot_info: None,
            rot_data: None,
            hal_info: None,
            attestation_chain_info: RefCell::new(BTreeMap::new()),
            attestation_id_info: RefCell::new(None),
        }
    }

    /// Indicate whether the current device is acting as a StrongBox instance.
    pub fn is_strongbox(&self) -> bool {
        self.hw_info.security_level == SecurityLevel::Strongbox
    }

    /// Indicate whether the current device has secure storage available.
    fn secure_storage_available(&self) -> kmr_common::tag::SecureStorage {
        if self.dev.sdd_mgr.is_some() {
            kmr_common::tag::SecureStorage::Available
        } else {
            kmr_common::tag::SecureStorage::Unavailable
        }
    }

    /// Parse and decrypt an encrypted key blob.
    fn keyblob_parse_decrypt(
        &self,
        key_blob: &[u8],
        params: &[KeyParam],
    ) -> Result<(keyblob::PlaintextKeyBlob, Option<SecureDeletionSlot>), Error> {
        // TODO: cope with previous versions/encodings of keys
        let encrypted_keyblob = keyblob::EncryptedKeyBlob::new(key_blob)?;
        let hidden = tag::hidden(params, self.root_of_trust()?)?;
        let sdd_slot = encrypted_keyblob.secure_deletion_slot();
        let keyblob = self.keyblob_decrypt(encrypted_keyblob, hidden)?;
        Ok((keyblob, sdd_slot))
    }

    /// Decrypt an encrypted key blob.
    fn keyblob_decrypt(
        &self,
        encrypted_keyblob: keyblob::EncryptedKeyBlob,
        hidden: Vec<KeyParam>,
    ) -> Result<keyblob::PlaintextKeyBlob, Error> {
        let root_kek = self.root_kek(encrypted_keyblob.kek_context())?;
        let keyblob = keyblob::decrypt(
            match &self.dev.sdd_mgr {
                None => None,
                Some(mr) => Some(*mr),
            },
            self.imp.aes,
            self.imp.hkdf,
            &root_kek,
            encrypted_keyblob,
            hidden,
        )?;
        let key_chars = keyblob.characteristics_at(self.hw_info.security_level)?;

        fn check(v: &u32, curr: u32, name: &str) -> Result<(), Error> {
            match (*v).cmp(&curr) {
                Ordering::Less => Err(km_err!(
                    KeyRequiresUpgrade,
                    "keyblob with old {} {} needs upgrade to current {}",
                    name,
                    v,
                    curr
                )),
                Ordering::Equal => Ok(()),
                Ordering::Greater => Err(km_err!(
                    InvalidKeyBlob,
                    "keyblob with future {} {} (current {})",
                    name,
                    v,
                    curr
                )),
            }
        }

        for param in key_chars {
            match param {
                KeyParam::OsVersion(v) => {
                    if let Some(hal_info) = &self.hal_info {
                        if hal_info.os_version == 0 {
                            // Special case: upgrades to OS version zero are always allowed.
                            if *v != 0 {
                                warn!("requesting upgrade to OS version 0");
                                return Err(km_err!(
                                    KeyRequiresUpgrade,
                                    "keyblob with OS version {} needs upgrade to current version 0",
                                    v,
                                ));
                            }
                        } else {
                            check(v, hal_info.os_version, "OS version")?;
                        }
                    } else {
                        error!("OS version not available, can't check for upgrade from {}", v);
                    }
                }
                KeyParam::OsPatchlevel(v) => {
                    if let Some(hal_info) = &self.hal_info {
                        check(v, hal_info.os_patchlevel, "OS patchlevel")?;
                    } else {
                        error!("OS patchlevel not available, can't check for upgrade from {}", v);
                    }
                }
                KeyParam::VendorPatchlevel(v) => {
                    if let Some(hal_info) = &self.hal_info {
                        check(v, hal_info.vendor_patchlevel, "vendor patchlevel")?;
                    } else {
                        error!(
                            "vendor patchlevel not available, can't check for upgrade from {}",
                            v
                        );
                    }
                }
                KeyParam::BootPatchlevel(v) => {
                    if let Some(boot_info) = &self.boot_info {
                        check(v, boot_info.boot_patchlevel, "boot patchlevel")?;
                    } else {
                        error!("boot patchlevel not available, can't check for upgrade from {}", v);
                    }
                }
                _ => {}
            }
        }

        Ok(keyblob)
    }

    /// Generate a unique identifier for a keyblob.
    fn key_id(&self, keyblob: &[u8]) -> Result<KeyId, Error> {
        let mut hmac_op =
            self.imp.hmac.begin(crypto::hmac::Key(vec_try![0; 16]?).into(), Digest::Sha256)?;
        hmac_op.update(keyblob)?;
        let tag = hmac_op.finish()?;

        Ok(KeyId(
            tag.try_into()
                .map_err(|_e| km_err!(UnknownError, "wrong size output from HMAC-SHA256"))?,
        ))
    }

    /// Increment the use count for the given key ID, failing if `max_uses` is reached.
    fn update_use_count(&mut self, key_id: KeyId, max_uses: u32) -> Result<(), Error> {
        let mut free_idx = None;
        let mut slot_idx = None;
        for idx in 0..self.use_count.len() {
            match &self.use_count[idx] {
                None if free_idx.is_none() => free_idx = Some(idx),
                None => {}
                Some(UseCount { key_id: k, count: _count }) if *k == key_id => {
                    slot_idx = Some(idx);
                    break;
                }
                Some(_) => {}
            }
        }
        if slot_idx.is_none() {
            // First use of this key ID; use a free slot if available.
            if let Some(idx) = free_idx {
                self.use_count[idx] = Some(UseCount { key_id, count: 0 });
                slot_idx = Some(idx);
            }
        }

        if let Some(idx) = slot_idx {
            let c = self.use_count[idx].as_mut().unwrap(); // safe: code above guarantees
            if c.count >= max_uses as u64 {
                Err(km_err!(KeyMaxOpsExceeded, "use count {} >= limit {}", c.count, max_uses))
            } else {
                c.count += 1;
                Ok(())
            }
        } else {
            Err(km_err!(TooManyOperations, "too many use-counted keys already in play"))
        }
    }

    /// Configure the boot-specific root of trust info.  KeyMint implementors should call this
    /// method when this information arrives from the bootloader (which happens in an
    /// implementation-specific manner).
    pub fn set_boot_info(&mut self, boot_info: BootInfo) {
        if !self.in_early_boot {
            error!("Rejecting attempt to set boot info {:?} after early boot", boot_info);
        }
        if self.boot_info.is_none() {
            info!("Setting boot_info to {:?}", boot_info);
            let rot_info = RootOfTrustInfo {
                verified_boot_key: boot_info.verified_boot_key,
                device_boot_locked: boot_info.device_boot_locked,
                verified_boot_state: boot_info.verified_boot_state,
                verified_boot_hash: boot_info.verified_boot_hash,
            };
            self.boot_info = Some(boot_info);
            self.rot_data =
                Some(rot_info.into_vec().unwrap_or_else(|_| {
                    b"Internal error! Failed to encode root-of-trust".to_vec()
                }));
        } else {
            warn!(
                "Boot info already set to {:?}, ignoring new values {:?}",
                self.boot_info, boot_info
            );
        }
    }

    /// Configure the HAL-derived information, learnt from the userspace
    /// operating system.
    pub fn set_hal_info(&mut self, hal_info: HalInfo) {
        if self.hal_info.is_none() {
            info!("Setting hal_info to {:?}", hal_info);
            self.hal_info = Some(hal_info);
        } else {
            warn!(
                "Hal info already set to {:?}, ignoring new values {:?}",
                self.hal_info, hal_info
            );
        }
    }

    /// Configure attestation IDs externally.
    pub fn set_attestation_ids(&self, ids: AttestationIdInfo) {
        if self.dev.attest_ids.is_some() {
            error!("Attempt to set attestation IDs externally");
        } else if self.attestation_id_info.borrow().is_some() {
            error!("Attempt to set attestation IDs when already set");
        } else {
            warn!("Setting attestation IDs directly");
            *self.attestation_id_info.borrow_mut() = Some(Rc::new(ids));
        }
    }

    /// Retrieve the attestation ID information for the device, if available.
    fn get_attestation_ids(&self) -> Option<Rc<AttestationIdInfo>> {
        if self.attestation_id_info.borrow().is_none() {
            if let Some(get_ids_impl) = self.dev.attest_ids.as_ref() {
                // Attestation IDs are not populated, but we have a trait implementation that
                // may provide them.
                match get_ids_impl.get() {
                    Ok(ids) => *self.attestation_id_info.borrow_mut() = Some(Rc::new(ids)),
                    Err(e) => error!("Failed to retrieve attestation IDs: {:?}", e),
                }
            }
        }
        self.attestation_id_info.borrow().as_ref().cloned()
    }

    /// Process a single serialized request, returning a serialized response.
    pub fn process(&mut self, req_data: &[u8]) -> Vec<u8> {
        let rsp = match PerformOpReq::from_slice(req_data) {
            Ok(req) => {
                debug!("-> TA: received request {:?}", req);
                self.process_req(req)
            }
            Err(e) => {
                error!("failed to decode CBOR request: {:?}", e);
                error_rsp(ErrorCode::UnknownError)
            }
        };
        debug!("<- TA: send response {:?}", rsp);
        match rsp.into_vec() {
            Ok(rsp_data) => rsp_data,
            Err(e) => {
                error!("failed to encode CBOR response: {:?}", e);
                invalid_cbor_rsp_data().to_vec()
            }
        }
    }

    /// Process a single request, returning a [`PerformOpResponse`].
    ///
    /// Select the appropriate method based on the request type, and use the
    /// request fields as parameters to the method.  In the opposite direction,
    /// build a response message from the values returned by the method.
    fn process_req(&mut self, req: PerformOpReq) -> PerformOpResponse {
        match req {
            // Internal messages.
            PerformOpReq::SetBootInfo(req) => {
                let verified_boot_state = match VerifiedBootState::try_from(req.verified_boot_state)
                {
                    Ok(state) => state,
                    Err(e) => return op_error_rsp(SetBootInfoRequest::CODE, Error::Cbor(e)),
                };
                self.set_boot_info(BootInfo {
                    verified_boot_key: req.verified_boot_key,
                    device_boot_locked: req.device_boot_locked,
                    verified_boot_state,
                    verified_boot_hash: req.verified_boot_hash,
                    boot_patchlevel: req.boot_patchlevel,
                });
                PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::SetBootInfo(SetBootInfoResponse {})),
                }
            }
            PerformOpReq::SetHalInfo(req) => {
                self.set_hal_info(HalInfo {
                    os_version: req.os_version,
                    os_patchlevel: req.os_patchlevel,
                    vendor_patchlevel: req.vendor_patchlevel,
                });
                PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::SetHalInfo(SetHalInfoResponse {})),
                }
            }
            PerformOpReq::SetAttestationIds(req) => {
                self.set_attestation_ids(req.ids);
                PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::SetAttestationIds(SetAttestationIdsResponse {})),
                }
            }

            // ISharedSecret messages.
            PerformOpReq::SharedSecretGetSharedSecretParameters(_req) => {
                match self.get_shared_secret_params() {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::SharedSecretGetSharedSecretParameters(
                            GetSharedSecretParametersResponse { ret },
                        )),
                    },
                    Err(e) => op_error_rsp(GetSharedSecretParametersRequest::CODE, e),
                }
            }
            PerformOpReq::SharedSecretComputeSharedSecret(req) => {
                match self.compute_shared_secret(&req.params) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::SharedSecretComputeSharedSecret(
                            ComputeSharedSecretResponse { ret },
                        )),
                    },
                    Err(e) => op_error_rsp(ComputeSharedSecretRequest::CODE, e),
                }
            }

            // ISecureClock messages.
            PerformOpReq::SecureClockGenerateTimeStamp(req) => {
                match self.generate_timestamp(req.challenge) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::SecureClockGenerateTimeStamp(
                            GenerateTimeStampResponse { ret },
                        )),
                    },
                    Err(e) => op_error_rsp(GenerateTimeStampRequest::CODE, e),
                }
            }

            // IKeyMintDevice messages.
            PerformOpReq::DeviceGetHardwareInfo(_req) => match self.get_hardware_info() {
                Ok(ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::DeviceGetHardwareInfo(GetHardwareInfoResponse { ret })),
                },
                Err(e) => op_error_rsp(GetHardwareInfoRequest::CODE, e),
            },
            PerformOpReq::DeviceAddRngEntropy(req) => match self.add_rng_entropy(&req.data) {
                Ok(_ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::DeviceAddRngEntropy(AddRngEntropyResponse {})),
                },
                Err(e) => op_error_rsp(AddRngEntropyRequest::CODE, e),
            },
            PerformOpReq::DeviceGenerateKey(req) => {
                match self.generate_key(&req.key_params, req.attestation_key) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceGenerateKey(GenerateKeyResponse { ret })),
                    },
                    Err(e) => op_error_rsp(GenerateKeyRequest::CODE, e),
                }
            }
            PerformOpReq::DeviceImportKey(req) => {
                match self.import_key(
                    &req.key_params,
                    req.key_format,
                    &req.key_data,
                    req.attestation_key,
                    KeyImport::NonWrapped,
                ) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceImportKey(ImportKeyResponse { ret })),
                    },
                    Err(e) => op_error_rsp(ImportKeyRequest::CODE, e),
                }
            }
            PerformOpReq::DeviceImportWrappedKey(req) => {
                match self.import_wrapped_key(
                    &req.wrapped_key_data,
                    &req.wrapping_key_blob,
                    &req.masking_key,
                    &req.unwrapping_params,
                    req.password_sid,
                    req.biometric_sid,
                ) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceImportWrappedKey(ImportWrappedKeyResponse {
                            ret,
                        })),
                    },
                    Err(e) => op_error_rsp(ImportWrappedKeyRequest::CODE, e),
                }
            }
            PerformOpReq::DeviceUpgradeKey(req) => {
                match self.upgrade_key(&req.key_blob_to_upgrade, req.upgrade_params) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceUpgradeKey(UpgradeKeyResponse { ret })),
                    },
                    Err(e) => op_error_rsp(UpgradeKeyRequest::CODE, e),
                }
            }
            PerformOpReq::DeviceDeleteKey(req) => match self.delete_key(&req.key_blob) {
                Ok(_ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::DeviceDeleteKey(DeleteKeyResponse {})),
                },
                Err(e) => op_error_rsp(DeleteKeyRequest::CODE, e),
            },
            PerformOpReq::DeviceDeleteAllKeys(_req) => match self.delete_all_keys() {
                Ok(_ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::DeviceDeleteAllKeys(DeleteAllKeysResponse {})),
                },
                Err(e) => op_error_rsp(DeleteAllKeysRequest::CODE, e),
            },
            PerformOpReq::DeviceDestroyAttestationIds(_req) => match self.destroy_attestation_ids()
            {
                Ok(_ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::DeviceDestroyAttestationIds(
                        DestroyAttestationIdsResponse {},
                    )),
                },
                Err(e) => op_error_rsp(DestroyAttestationIdsRequest::CODE, e),
            },
            PerformOpReq::DeviceBegin(req) => {
                match self.begin_operation(req.purpose, &req.key_blob, req.params, req.auth_token) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceBegin(BeginResponse { ret })),
                    },
                    Err(e) => op_error_rsp(BeginRequest::CODE, e),
                }
            }
            PerformOpReq::DeviceDeviceLocked(req) => {
                match self.device_locked(req.password_only, req.timestamp_token) {
                    Ok(_ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceDeviceLocked(DeviceLockedResponse {})),
                    },
                    Err(e) => op_error_rsp(DeviceLockedRequest::CODE, e),
                }
            }
            PerformOpReq::DeviceEarlyBootEnded(_req) => match self.early_boot_ended() {
                Ok(_ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::DeviceEarlyBootEnded(EarlyBootEndedResponse {})),
                },
                Err(e) => op_error_rsp(EarlyBootEndedRequest::CODE, e),
            },
            PerformOpReq::DeviceConvertStorageKeyToEphemeral(req) => {
                match self.convert_storage_key_to_ephemeral(&req.storage_key_blob) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceConvertStorageKeyToEphemeral(
                            ConvertStorageKeyToEphemeralResponse { ret },
                        )),
                    },
                    Err(e) => op_error_rsp(ConvertStorageKeyToEphemeralRequest::CODE, e),
                }
            }
            PerformOpReq::DeviceGetKeyCharacteristics(req) => {
                match self.get_key_characteristics(&req.key_blob, req.app_id, req.app_data) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::DeviceGetKeyCharacteristics(
                            GetKeyCharacteristicsResponse { ret },
                        )),
                    },
                    Err(e) => op_error_rsp(GetKeyCharacteristicsRequest::CODE, e),
                }
            }
            PerformOpReq::GetRootOfTrustChallenge(_req) => match self.get_root_of_trust_challenge()
            {
                Ok(ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::GetRootOfTrustChallenge(
                        GetRootOfTrustChallengeResponse { ret },
                    )),
                },
                Err(e) => op_error_rsp(GetRootOfTrustChallengeRequest::CODE, e),
            },
            PerformOpReq::GetRootOfTrust(req) => match self.get_root_of_trust(&req.challenge) {
                Ok(ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::GetRootOfTrust(GetRootOfTrustResponse { ret })),
                },
                Err(e) => op_error_rsp(GetRootOfTrustRequest::CODE, e),
            },
            PerformOpReq::SendRootOfTrust(req) => {
                match self.send_root_of_trust(&req.root_of_trust) {
                    Ok(_ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::SendRootOfTrust(SendRootOfTrustResponse {})),
                    },
                    Err(e) => op_error_rsp(SendRootOfTrustRequest::CODE, e),
                }
            }

            // IKeyMintOperation messages.
            PerformOpReq::OperationUpdateAad(req) => match self.op_update_aad(
                OpHandle(req.op_handle),
                &req.input,
                req.auth_token,
                req.timestamp_token,
            ) {
                Ok(_ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::OperationUpdateAad(UpdateAadResponse {})),
                },
                Err(e) => op_error_rsp(UpdateAadRequest::CODE, e),
            },
            PerformOpReq::OperationUpdate(req) => {
                match self.op_update(
                    OpHandle(req.op_handle),
                    &req.input,
                    req.auth_token,
                    req.timestamp_token,
                ) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::OperationUpdate(UpdateResponse { ret })),
                    },
                    Err(e) => op_error_rsp(UpdateRequest::CODE, e),
                }
            }
            PerformOpReq::OperationFinish(req) => {
                match self.op_finish(
                    OpHandle(req.op_handle),
                    req.input.as_deref(),
                    req.signature.as_deref(),
                    req.auth_token,
                    req.timestamp_token,
                    req.confirmation_token.as_deref(),
                ) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::OperationFinish(FinishResponse { ret })),
                    },
                    Err(e) => op_error_rsp(FinishRequest::CODE, e),
                }
            }
            PerformOpReq::OperationAbort(req) => match self.op_abort(OpHandle(req.op_handle)) {
                Ok(_ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::OperationAbort(AbortResponse {})),
                },
                Err(e) => op_error_rsp(AbortRequest::CODE, e),
            },

            // IRemotelyProvisionedComponentOperation messages.
            PerformOpReq::RpcGetHardwareInfo(_req) => match self.get_rpc_hardware_info() {
                Ok(ret) => PerformOpResponse {
                    error_code: ErrorCode::Ok,
                    rsp: Some(PerformOpRsp::RpcGetHardwareInfo(GetRpcHardwareInfoResponse { ret })),
                },
                Err(e) => op_error_rsp(GetRpcHardwareInfoRequest::CODE, e),
            },
            PerformOpReq::RpcGenerateEcdsaP256KeyPair(req) => {
                match self.generate_ecdsa_p256_keypair(req.test_mode) {
                    Ok((pubkey, ret)) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::RpcGenerateEcdsaP256KeyPair(
                            GenerateEcdsaP256KeyPairResponse { maced_public_key: pubkey, ret },
                        )),
                    },
                    Err(e) => op_error_rsp(GenerateEcdsaP256KeyPairRequest::CODE, e),
                }
            }
            PerformOpReq::RpcGenerateCertificateRequest(req) => {
                match self.generate_cert_req(
                    req.test_mode,
                    req.keys_to_sign,
                    &req.endpoint_encryption_cert_chain,
                    &req.challenge,
                ) {
                    Ok((device_info, protected_data, ret)) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::RpcGenerateCertificateRequest(
                            GenerateCertificateRequestResponse { device_info, protected_data, ret },
                        )),
                    },
                    Err(e) => op_error_rsp(GenerateCertificateRequestRequest::CODE, e),
                }
            }
            PerformOpReq::RpcGenerateCertificateV2Request(req) => {
                match self.generate_cert_req_v2(req.keys_to_sign, &req.challenge) {
                    Ok(ret) => PerformOpResponse {
                        error_code: ErrorCode::Ok,
                        rsp: Some(PerformOpRsp::RpcGenerateCertificateV2Request(
                            GenerateCertificateRequestV2Response { ret },
                        )),
                    },
                    Err(e) => op_error_rsp(GenerateCertificateRequestV2Request::CODE, e),
                }
            }
        }
    }

    fn add_rng_entropy(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() > 2048 {
            return Err(km_err!(InvalidInputLength, "entropy size {} too large", data.len()));
        };

        info!("add {} bytes of entropy", data.len());
        self.imp.rng.add_entropy(data);
        Ok(())
    }

    fn early_boot_ended(&mut self) -> Result<(), Error> {
        info!("early boot ended");
        self.in_early_boot = false;
        Ok(())
    }

    fn device_locked(
        &mut self,
        password_only: bool,
        timestamp_token: Option<TimeStampToken>,
    ) -> Result<(), Error> {
        info!(
            "device locked, password-required={}, timestamp={:?}",
            password_only, timestamp_token
        );

        let now = if let Some(clock) = &self.imp.clock {
            clock.now().into()
        } else if let Some(token) = timestamp_token {
            // Note that any `challenge` value in the `TimeStampToken` cannot be checked, because
            // there is nothing to check it against.
            let mac_input = clock::timestamp_token_mac_input(&token)?;
            if !self.verify_device_hmac(&mac_input, &token.mac)? {
                return Err(km_err!(InvalidArgument, "timestamp MAC not verified"));
            }
            token.timestamp
        } else {
            return Err(km_err!(InvalidArgument, "no clock and no external timestamp provided!"));
        };

        *self.device_locked.borrow_mut() = if password_only {
            LockState::PasswordLockedSince(now)
        } else {
            LockState::LockedSince(now)
        };
        Ok(())
    }

    fn get_hardware_info(&self) -> Result<KeyMintHardwareInfo, Error> {
        Ok(KeyMintHardwareInfo {
            version_number: self.hw_info.version_number,
            security_level: self.hw_info.security_level,
            key_mint_name: self.hw_info.impl_name.to_string(),
            key_mint_author_name: self.hw_info.author_name.to_string(),
            timestamp_token_required: self.imp.clock.is_none(),
        })
    }

    fn delete_key(&mut self, keyblob: &[u8]) -> Result<(), Error> {
        // Parse the keyblob. It cannot be decrypted, because hidden parameters are not available
        // (there is no `params` for them to arrive in).
        if let Ok(keyblob::EncryptedKeyBlob::V1(encrypted_keyblob)) =
            keyblob::EncryptedKeyBlob::new(keyblob)
        {
            // We have to trust that any secure deletion slot in the keyblob is valid, because the
            // key can't be decrypted.
            if let (Some(sdd_mgr), Some(slot)) =
                (&mut self.dev.sdd_mgr, encrypted_keyblob.secure_deletion_slot)
            {
                if let Err(e) = sdd_mgr.delete_secret(slot) {
                    error!("failed to delete secure deletion slot: {:?}", e);
                }
            }
        } else {
            error!("failed to parse keyblob, ignoring");
        }
        Ok(())
    }

    fn delete_all_keys(&mut self) -> Result<(), Error> {
        if let Some(sdd_mgr) = &mut self.dev.sdd_mgr {
            error!("secure deleting all keys! device unlikely to survive reboot!");
            sdd_mgr.delete_all();
        }
        Ok(())
    }

    fn destroy_attestation_ids(&mut self) -> Result<(), Error> {
        match self.dev.attest_ids.as_mut() {
            Some(attest_ids) => {
                error!("destroying all device attestation IDs!");
                attest_ids.destroy_all()
            }
            None => {
                error!("destroying device attestation IDs requested but not supported");
                Err(km_err!(Unimplemented, "no attestation ID functionality available"))
            }
        }
    }

    fn get_root_of_trust_challenge(&mut self) -> Result<[u8; 16], Error> {
        if !self.is_strongbox() {
            return Err(km_err!(Unimplemented, "root-of-trust challenge only for StrongBox"));
        }
        self.imp.rng.fill_bytes(&mut self.rot_challenge[..]);
        Ok(self.rot_challenge)
    }

    fn get_root_of_trust(&mut self, challenge: &[u8]) -> Result<Vec<u8>, Error> {
        if self.is_strongbox() {
            return Err(km_err!(Unimplemented, "root-of-trust retrieval not for StrongBox"));
        }
        let payload = if let Some(info) = &self.boot_info {
            info.clone()
                .to_tagged_vec()
                .map_err(|_e| km_err!(UnknownError, "Failed to CBOR-encode RootOfTrust"))
        } else {
            error!("RootOfTrust not known!");
            Err(km_err!(HardwareNotYetAvailable, "root-of-trust unavailable"))
        }?;

        let mac0 = coset::CoseMac0Builder::new()
            .protected(
                coset::HeaderBuilder::new().algorithm(coset::iana::Algorithm::HMAC_256_256).build(),
            )
            .payload(payload)
            .try_create_tag(challenge, |data| self.device_hmac(data))?
            .build();
        mac0.to_tagged_vec()
            .map_err(|_e| km_err!(UnknownError, "Failed to CBOR-encode RootOfTrust"))
    }

    fn send_root_of_trust(&mut self, root_of_trust: &[u8]) -> Result<(), Error> {
        if !self.is_strongbox() {
            return Err(km_err!(Unimplemented, "root-of-trust delivery only for StrongBox"));
        }
        let mac0 = coset::CoseMac0::from_tagged_slice(root_of_trust)
            .map_err(|_e| km_err!(InvalidArgument, "Failed to CBOR-decode CoseMac0"))?;
        mac0.verify_tag(&self.rot_challenge, |tag, data| {
            match self.verify_device_hmac(data, tag) {
                Ok(true) => Ok(()),
                Ok(false) => {
                    Err(km_err!(VerificationFailed, "HMAC verification of RootOfTrust failed"))
                }
                Err(e) => Err(e),
            }
        })?;
        let payload =
            mac0.payload.ok_or_else(|| km_err!(InvalidArgument, "Missing payload in CoseMac0"))?;
        let boot_info = BootInfo::from_tagged_slice(&payload)
            .map_err(|_e| km_err!(InvalidArgument, "Failed to CBOR-decode RootOfTrust"))?;
        if self.boot_info.is_none() {
            info!("Setting boot_info to TEE-provided {:?}", boot_info);
            self.boot_info = Some(boot_info);
        } else {
            info!("Ignoring TEE-provided RootOfTrust {:?} as already set", boot_info);
        }
        Ok(())
    }

    fn convert_storage_key_to_ephemeral(&self, keyblob: &[u8]) -> Result<Vec<u8>, Error> {
        if let Some(sk_wrapper) = self.dev.sk_wrapper {
            // Parse and decrypt the keyblob. Note that there is no way to provide extra hidden
            // params on the API.
            let (keyblob, _) = self.keyblob_parse_decrypt(keyblob, &[])?;

            // Now that we've got the key material, use a device-specific method to re-wrap it
            // with an ephemeral key.
            sk_wrapper.ephemeral_wrap(&keyblob.key_material)
        } else {
            Err(km_err!(Unimplemented, "storage key wrapping unavailable"))
        }
    }

    fn get_key_characteristics(
        &self,
        key_blob: &[u8],
        app_id: Vec<u8>,
        app_data: Vec<u8>,
    ) -> Result<Vec<KeyCharacteristics>, Error> {
        // Parse and decrypt the keyblob, which requires extra hidden params.
        let mut params = vec_try_with_capacity!(2)?;
        if !app_id.is_empty() {
            params.push(KeyParam::ApplicationId(app_id)); // capacity enough
        }
        if !app_data.is_empty() {
            params.push(KeyParam::ApplicationData(app_data)); // capacity enough
        }
        let (keyblob, _) = self.keyblob_parse_decrypt(key_blob, &params)?;
        Ok(keyblob.characteristics)
    }

    /// Generate an HMAC-SHA256 value over the data using the device's HMAC key (if available).
    fn device_hmac(&self, data: &[u8]) -> Result<Vec<u8>, Error> {
        let hmac_key = match &self.hmac_key {
            Some(k) => k,
            None => {
                error!("HMAC requested but no key available!");
                return Err(km_err!(HardwareNotYetAvailable, "HMAC key not agreed"));
            }
        };
        let mut hmac_op =
            self.imp.hmac.begin(crypto::hmac::Key(hmac_key.clone()).into(), Digest::Sha256)?;
        hmac_op.update(data)?;
        hmac_op.finish()
    }

    /// Verify an HMAC-SHA256 value over the data using the device's HMAC key (if available).
    fn verify_device_hmac(&self, data: &[u8], mac: &[u8]) -> Result<bool, Error> {
        let remac = self.device_hmac(data)?;
        Ok(self.imp.compare.eq(mac, &remac))
    }

    /// Return the root of trust that is bound into keyblobs.
    fn root_of_trust(&self) -> Result<&[u8], Error> {
        match &self.rot_data {
            Some(data) => Ok(data),
            None => Err(km_err!(HardwareNotYetAvailable, "No root-of-trust info available")),
        }
    }

    /// Return the root key used for key encryption.
    fn root_kek(&self, context: &[u8]) -> Result<RawKeyMaterial, Error> {
        self.dev.keys.root_kek(context)
    }

    /// Add KeyMint-generated tags to the provided [`KeyCharacteristics`].
    fn add_keymint_tags(
        &self,
        chars: &mut Vec<KeyCharacteristics>,
        origin: KeyOrigin,
    ) -> Result<(), Error> {
        for kc in chars {
            if kc.security_level == self.hw_info.security_level {
                kc.authorizations.try_push(KeyParam::Origin(origin))?;
                if let Some(hal_info) = &self.hal_info {
                    kc.authorizations.try_extend_from_slice(&[
                        KeyParam::OsVersion(hal_info.os_version),
                        KeyParam::OsPatchlevel(hal_info.os_patchlevel),
                        KeyParam::VendorPatchlevel(hal_info.vendor_patchlevel),
                    ])?;
                }
                if let Some(boot_info) = &self.boot_info {
                    kc.authorizations
                        .try_push(KeyParam::BootPatchlevel(boot_info.boot_patchlevel))?;
                }
                return Ok(());
            }
        }
        Err(km_err!(
            UnknownError,
            "no characteristics at our security level {:?}",
            self.hw_info.security_level
        ))
    }
}

/// Create a response structure with the given error code.
fn error_rsp(err_code: ErrorCode) -> PerformOpResponse {
    PerformOpResponse { error_code: err_code, rsp: None }
}

/// Create a response structure with the given error.
fn op_error_rsp(op: KeyMintOperation, err: Error) -> PerformOpResponse {
    error!("failing {:?} request with error {:?}", op, err);
    error_rsp(err.into())
}

/// Hand-encoded [`PerformOpResponse`] data for [`ErrorCode::UNKNOWN_ERROR`].
/// Does not perform CBOR serialization (and so is suitable for error reporting if/when
/// CBOR serialization fails).
fn invalid_cbor_rsp_data() -> [u8; 5] {
    [
        0x82, // 2-arr
        0x39, // nint, len 2
        0x03, // 0x3e7(999)
        0xe7, // = -1000
        0x80, // 0-arr
    ]
}

/// Build the HMAC input for a [`HardwareAuthToken`]
pub fn hardware_auth_token_mac_input(token: &HardwareAuthToken) -> Result<Vec<u8>, Error> {
    let mut result = vec_try_with_capacity!(
        size_of::<u8>() + // version=0 (BE)
        size_of::<i64>() + // challenge (Host)
        size_of::<i64>() + // user_id (Host)
        size_of::<i64>() + // authenticator_id (Host)
        size_of::<i32>() + // authenticator_type (BE)
        size_of::<i64>() // timestamp (BE)
    )?;
    result.extend_from_slice(&0u8.to_be_bytes()[..]);
    result.extend_from_slice(&token.challenge.to_ne_bytes()[..]);
    result.extend_from_slice(&token.user_id.to_ne_bytes()[..]);
    result.extend_from_slice(&token.authenticator_id.to_ne_bytes()[..]);
    result.extend_from_slice(&(token.authenticator_type as i32).to_be_bytes()[..]);
    result.extend_from_slice(&token.timestamp.milliseconds.to_be_bytes()[..]);
    Ok(result)
}
