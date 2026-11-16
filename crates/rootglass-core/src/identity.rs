//! Shared recipient-identity and image-key types.
//!
//! Private recipient keys never cross the provider interface. Callers receive
//! public identity metadata and an [`ImageMasterKey`] only after unwrapping a
//! recipient from a verified envelope.

use rootglass_manifest::MinimumRecipientHardware;
use rootglass_signature::{RecipientProvider, RecipientWrappingSuite, VerifiedRecipientEnvelope};
use std::fmt;
use thiserror::Error;
use zeroize::Zeroizing;

pub const IMAGE_MASTER_KEY_BYTES: usize = 32;

/// Public, non-secret information used to identify a local recipient key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientIdentity {
    pub recipient_id: String,
    pub provider_id: RecipientProvider,
    pub wrapping_suite_id: RecipientWrappingSuite,
    /// Canonical public key encoding, never the private identity.
    pub public_key: String,
    /// Optional user-supplied display label. Never used as identity material.
    pub label: Option<String>,
}

/// Operations and security properties truthfully provided by an identity backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub provider_id: RecipientProvider,
    pub wrapping_suite_id: RecipientWrappingSuite,
    pub hardware_backed: bool,
    pub can_create: bool,
    pub can_unwrap: bool,
}

impl ProviderCapabilities {
    /// Whether this provider can satisfy a publisher-signed hardware policy.
    pub fn satisfies(&self, requirement: &MinimumRecipientHardware) -> bool {
        match requirement {
            MinimumRecipientHardware::SoftwareAllowed => true,
            MinimumRecipientHardware::HardwareBackedRequired => self.hardware_backed,
        }
    }
}

/// Safe errors returned by identity providers. These deliberately do not carry
/// backend errors, since some credential-store errors include the stored bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IdentityProviderError {
    #[error("the identity provider is not supported on this host")]
    UnsupportedPlatform,
    #[error("the operating-system credential store denied access")]
    CredentialAccessDenied,
    #[error("the operating-system credential store is unavailable")]
    CredentialStoreUnavailable,
    #[error("the identity is not present in the local credential store")]
    IdentityNotFound,
    #[error("the local identity index is invalid or unsupported")]
    CorruptIdentityIndex,
    #[error("the local identity does not match its public fingerprint")]
    IdentityMismatch,
    #[error("the requested recipient is not present in the verified envelope")]
    RecipientNotFound,
    #[error("the verified recipient uses a provider or wrapping suite this provider cannot open")]
    UnsupportedRecipient,
    #[error("the provider cannot satisfy the required hardware-backed identity policy")]
    HardwarePolicyUnsatisfied,
    #[error("the wrapped image key could not be decrypted")]
    UnwrapFailed,
    #[error("the unwrapped image key has an invalid length")]
    InvalidImageKeyLength,
    #[error("the provider already contains this identity")]
    IdentityAlreadyExists,
    #[error("the local credential store already contains the maximum supported identities")]
    IdentityLimitReached,
    #[error("the requested identity is not a valid Age identity")]
    InvalidIdentity,
    #[error("identity labels must be 1 to 64 printable characters")]
    InvalidLabel,
}

/// A 32-byte image master key kept in zeroizing memory and redacted in logs.
pub struct ImageMasterKey(Zeroizing<[u8; IMAGE_MASTER_KEY_BYTES]>);

impl ImageMasterKey {
    pub fn new(bytes: Zeroizing<[u8; IMAGE_MASTER_KEY_BYTES]>) -> Self {
        Self(bytes)
    }

    /// Borrow the key bytes for the immediate cryptographic operation that
    /// needs them. Callers must not copy or retain this reference's contents.
    pub fn as_bytes(&self) -> &[u8; IMAGE_MASTER_KEY_BYTES] {
        &self.0
    }
}

impl fmt::Debug for ImageMasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ImageMasterKey([REDACTED])")
    }
}

/// Common identity-provider API shared by CLI, TUI, desktop, and runtime code.
pub trait RecipientIdentityProvider: Send + Sync {
    fn capabilities(&self) -> ProviderCapabilities;

    fn list_identities(&self) -> Result<Vec<RecipientIdentity>, IdentityProviderError>;

    fn create_identity(&self) -> Result<RecipientIdentity, IdentityProviderError>;

    /// Create an identity with an optional local display label. Backends that
    /// do not support labels fail closed instead of silently discarding them.
    fn create_identity_with_label(
        &self,
        label: Option<&str>,
    ) -> Result<RecipientIdentity, IdentityProviderError> {
        if label.is_some() {
            return Err(IdentityProviderError::InvalidLabel);
        }
        self.create_identity()
    }

    /// Delete a local identity by its public recipient ID. User-facing callers
    /// are responsible for obtaining confirmation before invoking this method.
    fn delete_identity(&self, recipient_id: &str) -> Result<(), IdentityProviderError>;

    /// Unwrap only a recipient and hardware policy that came from a
    /// cryptographically verified sidecar and its publisher-signed manifest.
    fn unwrap_image_key(
        &self,
        envelope: &VerifiedRecipientEnvelope,
        recipient_id: &str,
    ) -> Result<ImageMasterKey, IdentityProviderError>;
}
