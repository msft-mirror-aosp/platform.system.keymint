//! Functionality for remote key provisioning

use super::KeyMintTa;
use crate::RpcInfo;
use alloc::string::{String, ToString};
use alloc::{vec, vec::Vec};
use kmr_common::{km_err, try_to_vec, Error};
use kmr_wire::{
    cbor,
    cbor::cbor,
    keymint::{SecurityLevel, VerifiedBootState},
    rpc::{
        DeviceInfo, EekCurve, HardwareInfo, MacedPublicKey, ProtectedData,
        MINIMUM_SUPPORTED_KEYS_IN_CSR,
    },
    CborError,
};

impl<'a> KeyMintTa<'a> {
    pub(crate) fn rpc_device_info(&self) -> Result<Vec<u8>, Error> {
        // First make sure all the relevant info is available.
        let ids = self
            .get_attestation_ids()
            .ok_or_else(|| km_err!(UnknownError, "attestation ID info not available"))?;
        let boot_info = self
            .boot_info
            .as_ref()
            .ok_or_else(|| km_err!(UnknownError, "boot info not available"))?;
        let hal_info = self
            .hal_info
            .as_ref()
            .ok_or_else(|| km_err!(UnknownError, "HAL info not available"))?;

        let brand = String::from_utf8_lossy(&ids.brand);
        let manufacturer = String::from_utf8_lossy(&ids.manufacturer);
        let product = String::from_utf8_lossy(&ids.product);
        let model = String::from_utf8_lossy(&ids.model);
        let device = String::from_utf8_lossy(&ids.device);

        let bootloader_state = if boot_info.device_boot_locked { "locked" } else { "unlocked" };
        let vbmeta_digest = cbor::value::Value::Bytes(try_to_vec(&boot_info.verified_boot_hash)?);
        let vb_state = match boot_info.verified_boot_state {
            VerifiedBootState::Verified => "green",
            VerifiedBootState::SelfSigned => "yellow",
            VerifiedBootState::Unverified => "orange",
            VerifiedBootState::Failed => "red",
        };
        let security_level = match self.hw_info.security_level {
            SecurityLevel::TrustedEnvironment => "tee",
            SecurityLevel::Strongbox => "strongbox",
            l => return Err(km_err!(UnknownError, "security level {:?} not supported", l)),
        };

        let (version, fused) = match &self.rpc_info {
            RpcInfo::V2(rpc_info_v2) => (2, rpc_info_v2.fused),
            RpcInfo::V3(rpc_info_v3) => (3, rpc_info_v3.fused),
        };
        // The DeviceInfo.aidl file specifies that map keys should be ordered according
        // to RFC 7049 canonicalization rules, which are:
        // - shorter-encoded key < longer-encoded key
        // - lexicographic comparison for same-length keys
        // Note that this is *different* than the ordering required in RFC 8949 s4.2.1.
        let info = cbor!({
            "brand" => brand,
            "fused" => i32::from(fused),
            "model" => model,
            "device" => device,
            "product" => product,
            "version" => version,
            "vb_state" => vb_state,
            "os_version" => hal_info.os_version,
            "manufacturer" => manufacturer,
            "vbmeta_digest" => vbmeta_digest,
            "security_level" => security_level,
            "boot_patch_level" => boot_info.boot_patchlevel,
            "bootloader_state" => bootloader_state,
            "system_patch_level" => hal_info.os_patchlevel,
            "vendor_patch_level" => hal_info.vendor_patchlevel,
        })?;

        let mut data = Vec::new();
        cbor::ser::into_writer(&info, &mut data)
            .map_err(|_e| Error::Cbor(CborError::EncodeFailed))?;
        Ok(data)
    }

    pub(crate) fn get_rpc_hardware_info(&self) -> Result<HardwareInfo, Error> {
        match &self.rpc_info {
            RpcInfo::V2(rpc_info_v2) => Ok(HardwareInfo {
                version_number: 2,
                rpc_author_name: rpc_info_v2.author_name.to_string(),
                supported_eek_curve: rpc_info_v2.supported_eek_curve,
                unique_id: Some(rpc_info_v2.unique_id.to_string()),
                supported_num_keys_in_csr: 20,
            }),
            RpcInfo::V3(rpc_info_v3) => Ok(HardwareInfo {
                version_number: 3,
                rpc_author_name: rpc_info_v3.author_name.to_string(),
                supported_eek_curve: EekCurve::None,
                unique_id: Some(rpc_info_v3.unique_id.to_string()),
                supported_num_keys_in_csr: rpc_info_v3.supported_num_of_keys_in_csr,
            }),
        }
    }

    pub(crate) fn generate_ecdsa_p256_keypair(
        &self,
        _test_mode: bool,
    ) -> Result<(MacedPublicKey, Vec<u8>), Error> {
        Err(km_err!(Unimplemented, "TODO: GenerateEcdsaP256KeyPair"))
    }

    pub(crate) fn generate_cert_req(
        &self,
        _test_mode: bool,
        _keys_to_sign: Vec<MacedPublicKey>,
        _eek_chain: &[u8],
        _challenge: &[u8],
    ) -> Result<(DeviceInfo, ProtectedData, Vec<u8>), Error> {
        let _device_info = self.rpc_device_info()?;
        Err(km_err!(Unimplemented, "TODO: GenerateCertificateRequest"))
    }

    pub(crate) fn generate_cert_req_v2(
        &self,
        _keys_to_sign: Vec<MacedPublicKey>,
        _challenge: &[u8],
    ) -> Result<Vec<u8>, Error> {
        Err(km_err!(Unimplemented, "TODO: GenerateCertificateRequestV2"))
    }
}
