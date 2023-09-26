//! Rust types definitions for BoringSSL objects.
use bssl_ffi as ffi;

/// New type for `*mut ffi::CMAC_CTX` to implement `Send` for it. This allow us to still check if a
/// `!Send` item is added to `BoringAesCmacOperation`
pub(crate) struct CmacCtx(pub(crate) *mut ffi::CMAC_CTX);

// Safety: Checked CMAC_CTX allocation, initialization and destruction code to insure that it is
//         safe to share it between threads.
unsafe impl Send for CmacCtx {}

/// New type for `*mut ffi::EVP_MD_CTX` to implement `Send` for it. This allow us to still check if
/// a `!Send` item is added to `BoringEcDigestSignOperation`
pub(crate) struct EvpMdCtx(pub(crate) *mut ffi::EVP_MD_CTX);

// Safety: Checked EVP_MD_CTX allocation, initialization and destruction code to insure that it is
//         safe to share it between threads.
unsafe impl Send for EvpMdCtx {}

/// New type for `*mut ffi::EVP_PKEY_CTX` to implement `Send` for it. This allow us to still check
/// if a `!Send` item is added to `BoringEcDigestSignOperation`
pub(crate) struct EvpPkeyCtx(pub(crate) *mut ffi::EVP_PKEY_CTX);

// Safety: Checked EVP_MD_CTX allocation, initialization and destruction code to insure that it is
//         safe to share it between threads.
unsafe impl Send for EvpPkeyCtx {}

/// New type for `*mut ffi::HMAC_CTX` to implement `Send` for it. This allow us to still check if
/// a `!Send` item is added to `BoringHmacOperation`
pub(crate) struct HmacCtx(pub(crate) *mut ffi::HMAC_CTX);

// Safety: Checked EVP_MD_CTX allocation, initialization and destruction code to insure that it is
//         safe to share it between threads.
unsafe impl Send for HmacCtx {}
