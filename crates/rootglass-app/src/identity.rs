//! Portable Age identities backed by the current user's operating-system
//! credential store. Tests inject an in-memory store and never open a native
//! keychain, Credential Manager, or Secret Service session.

use age::secrecy::{ExposeSecret, SecretString};
use rootglass_core::identity::{
    IdentityProviderError, ImageMasterKey, ProviderCapabilities, RecipientIdentity,
    RecipientIdentityProvider, IMAGE_MASTER_KEY_BYTES,
};
use rootglass_manifest::MinimumRecipientHardware;
use rootglass_signature::{
    recipient_id_for_public_key, RecipientProvider, RecipientRecord, RecipientWrappingSuite,
    VerifiedRecipientEnvelope,
};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::str::FromStr;
use std::sync::Mutex;
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "dev.rootglass.identity.age.v1";
const INDEX_ACCOUNT: &str = "identity-index-v1";
const PRIVATE_ACCOUNT_PREFIX: &str = "private-v1:";
const IDENTITY_INDEX_VERSION: u8 = 1;
// Keep the index comfortably below Windows Credential Manager's 2,560-byte
// credential-blob limit; the index contains only public Age recipient strings.
const MAX_LOCAL_IDENTITIES: usize = 16;
const MAX_INDEX_BYTES: usize = 2048;
const MAX_WRAPPED_IMK_BYTES: usize = 16 * 1024;

/// Safe classification of credential-store errors. Platform messages are
/// intentionally discarded because some backends attach credential bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialStoreError {
    AccessDenied,
    Unavailable,
    UnsupportedPlatform,
}

/// Minimal secret-store surface used by the Age provider. Returned secrets are
/// zeroized when dropped; account names and index values contain public data.
trait CredentialStore: Send + Sync {
    fn get_secret(&self, account: &str) -> Result<Option<SecretString>, CredentialStoreError>;

    fn set_secret(&self, account: &str, secret: &str) -> Result<(), CredentialStoreError>;

    fn delete_secret(&self, account: &str) -> Result<(), CredentialStoreError>;
}

/// Credential store selected by `keyring` for macOS, Windows, or Linux.
#[derive(Debug, Default, Clone, Copy)]
struct KeyringCredentialStore;

impl CredentialStore for KeyringCredentialStore {
    fn get_secret(&self, account: &str) -> Result<Option<SecretString>, CredentialStoreError> {
        if !supported_host() {
            return Err(CredentialStoreError::UnsupportedPlatform);
        }
        let entry = keyring_entry(account)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(SecretString::from(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(classify_keyring_error(error)),
        }
    }

    fn set_secret(&self, account: &str, secret: &str) -> Result<(), CredentialStoreError> {
        if !supported_host() {
            return Err(CredentialStoreError::UnsupportedPlatform);
        }
        keyring_entry(account)?
            .set_password(secret)
            .map_err(classify_keyring_error)
    }

    fn delete_secret(&self, account: &str) -> Result<(), CredentialStoreError> {
        if !supported_host() {
            return Err(CredentialStoreError::UnsupportedPlatform);
        }
        match keyring_entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(classify_keyring_error(error)),
        }
    }
}

/// Age X25519 provider with an injectable store for deterministic tests.
pub struct AgeIdentityProvider {
    store: Box<dyn CredentialStore>,
    mutation_lock: Mutex<()>,
}

impl AgeIdentityProvider {
    fn with_store<S: CredentialStore + 'static>(store: S) -> Self {
        Self {
            store: Box::new(store),
            mutation_lock: Mutex::new(()),
        }
    }

    /// Use the current user's login Keychain, Windows Credential Manager, or
    /// Linux Secret Service. Other hosts fail closed.
    pub fn system() -> Result<Self, IdentityProviderError> {
        if supported_host() {
            Ok(Self::with_store(KeyringCredentialStore))
        } else {
            Err(IdentityProviderError::UnsupportedPlatform)
        }
    }
}

impl AgeIdentityProvider {
    fn load_index(&self) -> Result<IdentityIndex, IdentityProviderError> {
        let Some(index_text) = self
            .store
            .get_secret(INDEX_ACCOUNT)
            .map_err(map_store_error)?
        else {
            return Ok(IdentityIndex::default());
        };
        if index_text.expose_secret().len() > MAX_INDEX_BYTES {
            return Err(IdentityProviderError::CorruptIdentityIndex);
        }
        let index: IdentityIndex = serde_json::from_str(index_text.expose_secret())
            .map_err(|_| IdentityProviderError::CorruptIdentityIndex)?;
        if index.version != IDENTITY_INDEX_VERSION || index.public_keys.len() > MAX_LOCAL_IDENTITIES
        {
            return Err(IdentityProviderError::CorruptIdentityIndex);
        }

        let mut previous: Option<&str> = None;
        for public_key in &index.public_keys {
            let identity = identity_for_public_key(public_key)
                .map_err(|_| IdentityProviderError::CorruptIdentityIndex)?;
            if identity.public_key != *public_key
                || previous.is_some_and(|last| last >= public_key.as_str())
            {
                return Err(IdentityProviderError::CorruptIdentityIndex);
            }
            previous = Some(public_key);
        }
        Ok(index)
    }

    fn store_index(&self, index: &IdentityIndex) -> Result<(), IdentityProviderError> {
        let serialized = Zeroizing::new(
            serde_json::to_string(index)
                .map_err(|_| IdentityProviderError::CorruptIdentityIndex)?,
        );
        if serialized.len() > MAX_INDEX_BYTES {
            return Err(IdentityProviderError::IdentityLimitReached);
        }
        self.store
            .set_secret(INDEX_ACCOUNT, serialized.as_str())
            .map_err(map_store_error)
    }

    fn unwrap_recipient(
        &self,
        recipient: &RecipientRecord,
    ) -> Result<ImageMasterKey, IdentityProviderError> {
        if recipient.provider_id != RecipientProvider::AgeX25519
            || recipient.wrapping_suite_id != RecipientWrappingSuite::AgeV1X25519
            || recipient.wrapped_imk.is_empty()
            || recipient.wrapped_imk.len() > MAX_WRAPPED_IMK_BYTES
        {
            return Err(IdentityProviderError::UnsupportedRecipient);
        }

        let index = self.load_index()?;
        let public_key = index
            .public_keys
            .iter()
            .find(|public_key| {
                identity_for_public_key(public_key)
                    .is_ok_and(|identity| identity.recipient_id == recipient.recipient_id)
            })
            .ok_or(IdentityProviderError::IdentityNotFound)?;

        let account = private_account(&recipient.recipient_id);
        let secret = self
            .store
            .get_secret(&account)
            .map_err(map_store_error)?
            .ok_or(IdentityProviderError::IdentityNotFound)?;
        let identity = age::x25519::Identity::from_str(secret.expose_secret())
            .map_err(|_| IdentityProviderError::InvalidIdentity)?;
        let derived = identity_for_public_key(&identity.to_public().to_string())?;
        if derived.recipient_id != recipient.recipient_id || derived.public_key != *public_key {
            return Err(IdentityProviderError::IdentityMismatch);
        }

        // Read through our own zeroizing buffer rather than age's convenience
        // decrypt function, so partial plaintext is cleared on every error path.
        let decryptor = age::Decryptor::new_buffered(recipient.wrapped_imk.as_slice())
            .map_err(|_| IdentityProviderError::UnwrapFailed)?;
        let reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(|_| IdentityProviderError::UnwrapFailed)?;
        let mut plaintext = Zeroizing::new(Vec::with_capacity(IMAGE_MASTER_KEY_BYTES));
        reader
            .take((IMAGE_MASTER_KEY_BYTES + 1) as u64)
            .read_to_end(&mut plaintext)
            .map_err(|_| IdentityProviderError::UnwrapFailed)?;
        if plaintext.len() != IMAGE_MASTER_KEY_BYTES {
            return Err(IdentityProviderError::InvalidImageKeyLength);
        }
        let mut key = Zeroizing::new([0; IMAGE_MASTER_KEY_BYTES]);
        key.copy_from_slice(&plaintext);
        Ok(ImageMasterKey::new(key))
    }

    fn check_hardware_policy(
        &self,
        minimum_hardware: &MinimumRecipientHardware,
    ) -> Result<(), IdentityProviderError> {
        if self.capabilities().satisfies(minimum_hardware) {
            Ok(())
        } else {
            Err(IdentityProviderError::HardwarePolicyUnsatisfied)
        }
    }
}

impl RecipientIdentityProvider for AgeIdentityProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            provider_id: RecipientProvider::AgeX25519,
            wrapping_suite_id: RecipientWrappingSuite::AgeV1X25519,
            hardware_backed: false,
            can_create: true,
            can_unwrap: true,
        }
    }

    fn list_identities(&self) -> Result<Vec<RecipientIdentity>, IdentityProviderError> {
        self.load_index()?
            .public_keys
            .iter()
            .map(|public_key| identity_for_public_key(public_key))
            .collect()
    }

    fn create_identity(&self) -> Result<RecipientIdentity, IdentityProviderError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| IdentityProviderError::CredentialStoreUnavailable)?;
        let mut index = self.load_index()?;
        if index.public_keys.len() >= MAX_LOCAL_IDENTITIES {
            return Err(IdentityProviderError::IdentityLimitReached);
        }

        let identity = age::x25519::Identity::generate();
        let private_key = identity.to_string();
        let public = identity.to_public().to_string();
        let public_identity = identity_for_public_key(&public)?;
        if index.public_keys.contains(&public) {
            return Err(IdentityProviderError::IdentityAlreadyExists);
        }

        let private_account = private_account(&public_identity.recipient_id);
        if self
            .store
            .get_secret(&private_account)
            .map_err(map_store_error)?
            .is_some()
        {
            return Err(IdentityProviderError::IdentityAlreadyExists);
        }
        self.store
            .set_secret(&private_account, private_key.expose_secret())
            .map_err(map_store_error)?;

        index.public_keys.push(public);
        index.public_keys.sort_unstable();
        if let Err(error) = self.store_index(&index) {
            if self.store.delete_secret(&private_account).is_err() {
                return Err(IdentityProviderError::CredentialStoreUnavailable);
            }
            return Err(error);
        }
        Ok(public_identity)
    }

    fn delete_identity(&self, recipient_id: &str) -> Result<(), IdentityProviderError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| IdentityProviderError::CredentialStoreUnavailable)?;
        let old_index = self.load_index()?;
        let identity = old_index
            .public_keys
            .iter()
            .find_map(|public_key| {
                identity_for_public_key(public_key)
                    .ok()
                    .filter(|identity| identity.recipient_id == recipient_id)
            })
            .ok_or(IdentityProviderError::IdentityNotFound)?;
        let private_account = private_account(&identity.recipient_id);
        if self
            .store
            .get_secret(&private_account)
            .map_err(map_store_error)?
            .is_none()
        {
            return Err(IdentityProviderError::IdentityNotFound);
        }

        let mut new_index = old_index.clone();
        new_index
            .public_keys
            .retain(|public_key| public_key != &identity.public_key);
        self.store_index(&new_index)?;
        if self.store.delete_secret(&private_account).is_err() {
            if self.store_index(&old_index).is_err() {
                return Err(IdentityProviderError::CredentialStoreUnavailable);
            }
            return Err(IdentityProviderError::CredentialStoreUnavailable);
        }
        Ok(())
    }

    fn unwrap_image_key(
        &self,
        envelope: &VerifiedRecipientEnvelope,
        recipient_id: &str,
    ) -> Result<ImageMasterKey, IdentityProviderError> {
        self.check_hardware_policy(envelope.minimum_recipient_hardware())?;
        let recipient = envelope
            .recipient(recipient_id)
            .ok_or(IdentityProviderError::RecipientNotFound)?;
        self.unwrap_recipient(recipient)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityIndex {
    version: u8,
    public_keys: Vec<String>,
}

impl Default for IdentityIndex {
    fn default() -> Self {
        Self {
            version: IDENTITY_INDEX_VERSION,
            public_keys: Vec::new(),
        }
    }
}

fn identity_for_public_key(public_key: &str) -> Result<RecipientIdentity, IdentityProviderError> {
    let parsed = age::x25519::Recipient::from_str(public_key)
        .map_err(|_| IdentityProviderError::InvalidIdentity)?;
    let canonical_public_key = parsed.to_string();
    if canonical_public_key != public_key {
        return Err(IdentityProviderError::InvalidIdentity);
    }
    Ok(RecipientIdentity {
        recipient_id: recipient_id_for_public_key(
            RecipientWrappingSuite::AgeV1X25519,
            canonical_public_key.as_bytes(),
        ),
        provider_id: RecipientProvider::AgeX25519,
        wrapping_suite_id: RecipientWrappingSuite::AgeV1X25519,
        public_key: canonical_public_key,
    })
}

fn private_account(recipient_id: &str) -> String {
    format!("{PRIVATE_ACCOUNT_PREFIX}{recipient_id}")
}

fn map_store_error(error: CredentialStoreError) -> IdentityProviderError {
    match error {
        CredentialStoreError::AccessDenied => IdentityProviderError::CredentialAccessDenied,
        CredentialStoreError::Unavailable => IdentityProviderError::CredentialStoreUnavailable,
        CredentialStoreError::UnsupportedPlatform => IdentityProviderError::UnsupportedPlatform,
    }
}

fn supported_host() -> bool {
    cfg!(any(
        target_os = "macos",
        target_os = "windows",
        target_os = "linux"
    ))
}

fn keyring_entry(account: &str) -> Result<keyring::Entry, CredentialStoreError> {
    keyring::Entry::new(KEYRING_SERVICE, account).map_err(classify_keyring_error)
}

fn classify_keyring_error(error: keyring::Error) -> CredentialStoreError {
    match error {
        keyring::Error::NoStorageAccess(_) => CredentialStoreError::AccessDenied,
        _ => CredentialStoreError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::secrecy::ExposeSecret;
    use rootglass_signature::RecipientRecord;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default, Clone)]
    struct FakeStore {
        values: Arc<Mutex<BTreeMap<String, Zeroizing<String>>>>,
        reads: Arc<AtomicUsize>,
        fail_writes: Arc<AtomicBool>,
        deny_access: Arc<AtomicBool>,
    }

    impl FakeStore {
        fn has_secret(&self, account: &str) -> bool {
            self.values.lock().unwrap().contains_key(account)
        }
    }

    impl CredentialStore for FakeStore {
        fn get_secret(&self, account: &str) -> Result<Option<SecretString>, CredentialStoreError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.deny_access.load(Ordering::SeqCst) {
                return Err(CredentialStoreError::AccessDenied);
            }
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(account)
                .map(|secret| SecretString::from(secret.as_str().to_owned())))
        }

        fn set_secret(&self, account: &str, secret: &str) -> Result<(), CredentialStoreError> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(CredentialStoreError::Unavailable);
            }
            self.values
                .lock()
                .unwrap()
                .insert(account.to_owned(), Zeroizing::new(secret.to_owned()));
            Ok(())
        }

        fn delete_secret(&self, account: &str) -> Result<(), CredentialStoreError> {
            self.values.lock().unwrap().remove(account);
            Ok(())
        }
    }

    fn encrypt_to(public_key: &str, plaintext: &[u8]) -> Vec<u8> {
        let recipient = age::x25519::Recipient::from_str(public_key).unwrap();
        age::encrypt(&recipient, plaintext).unwrap()
    }

    fn test_record(identity: &RecipientIdentity, plaintext: &[u8]) -> RecipientRecord {
        RecipientRecord {
            recipient_id: identity.recipient_id.clone(),
            provider_id: RecipientProvider::AgeX25519,
            wrapping_suite_id: RecipientWrappingSuite::AgeV1X25519,
            wrapped_imk: encrypt_to(&identity.public_key, plaintext),
        }
    }

    #[test]
    fn create_and_list_exposes_only_public_identity_metadata() {
        let store = FakeStore::default();
        let provider = AgeIdentityProvider::with_store(store.clone());
        let created = provider.create_identity().unwrap();
        let listed = provider.list_identities().unwrap();

        assert_eq!(listed, vec![created.clone()]);
        assert!(created.public_key.starts_with("age1"));
        assert!(created.recipient_id.starts_with("age-v1-x25519:"));
        assert!(!created.public_key.contains("AGE-SECRET-KEY"));
        let private_key = store
            .get_secret(&private_account(&created.recipient_id))
            .unwrap()
            .unwrap();
        let debug = format!("{private_key:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("AGE-SECRET-KEY-"));
    }

    #[test]
    fn unwraps_age_ciphertext_and_redacts_the_image_key() {
        let provider = AgeIdentityProvider::with_store(FakeStore::default());
        let identity = provider.create_identity().unwrap();
        let key = [0x5a; IMAGE_MASTER_KEY_BYTES];
        let record = test_record(&identity, &key);

        let unwrapped = provider.unwrap_recipient(&record).unwrap();

        assert_eq!(unwrapped.as_bytes(), &key);
        let debug = format!("{unwrapped:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("5a"));
    }

    #[test]
    fn wrong_age_identity_and_wrong_key_length_fail_closed() {
        let provider = AgeIdentityProvider::with_store(FakeStore::default());
        let local = provider.create_identity().unwrap();
        let foreign_provider = AgeIdentityProvider::with_store(FakeStore::default());
        let foreign = foreign_provider.create_identity().unwrap();

        assert!(matches!(
            provider.unwrap_recipient(&test_record(&foreign, &[1; 32])),
            Err(IdentityProviderError::IdentityNotFound)
        ));
        assert!(matches!(
            provider.unwrap_recipient(&test_record(&local, &[1; 31])),
            Err(IdentityProviderError::InvalidImageKeyLength)
        ));
        let mut wrong_ciphertext = test_record(&local, &[1; 32]);
        let last_byte = wrong_ciphertext.wrapped_imk.len() - 1;
        wrong_ciphertext.wrapped_imk[last_byte] ^= 1;
        assert!(matches!(
            provider.unwrap_recipient(&wrong_ciphertext),
            Err(IdentityProviderError::UnwrapFailed)
        ));
    }

    #[test]
    fn provider_is_software_only_and_hardware_policy_is_unsatisfied() {
        let provider = AgeIdentityProvider::with_store(FakeStore::default());
        let capabilities = provider.capabilities();
        assert!(!capabilities.hardware_backed);
        assert!(capabilities.satisfies(&MinimumRecipientHardware::SoftwareAllowed));
        assert!(!capabilities.satisfies(&MinimumRecipientHardware::HardwareBackedRequired));
    }

    #[test]
    fn corrupt_private_identity_is_detected_against_the_index_fingerprint() {
        let store = FakeStore::default();
        let provider = AgeIdentityProvider::with_store(store.clone());
        let local = provider.create_identity().unwrap();
        let account = private_account(&local.recipient_id);
        let wrong_identity = age::x25519::Identity::generate();
        let wrong_secret = wrong_identity.to_string();
        store
            .set_secret(&account, wrong_secret.expose_secret())
            .unwrap();

        assert!(matches!(
            provider.unwrap_recipient(&test_record(&local, &[1; 32])),
            Err(IdentityProviderError::IdentityMismatch)
        ));
    }

    #[test]
    fn credential_failures_are_explicit_and_never_replace_existing_identity() {
        let store = FakeStore::default();
        let provider = AgeIdentityProvider::with_store(store.clone());
        let identity = provider.create_identity().unwrap();
        store.deny_access.store(true, Ordering::SeqCst);
        assert_eq!(
            provider.list_identities(),
            Err(IdentityProviderError::CredentialAccessDenied)
        );
        store.deny_access.store(false, Ordering::SeqCst);

        store.fail_writes.store(true, Ordering::SeqCst);
        assert_eq!(
            provider.create_identity(),
            Err(IdentityProviderError::CredentialStoreUnavailable)
        );
        assert_eq!(
            provider.list_identities().unwrap(),
            vec![identity],
            "failed creation must leave the existing identity available"
        );
    }

    #[test]
    fn corrupt_or_oversized_public_index_fails_without_discarding_it() {
        let store = FakeStore::default();
        let provider = AgeIdentityProvider::with_store(store.clone());
        store
            .set_secret(INDEX_ACCOUNT, "{\"version\":2,\"public_keys\":[]}")
            .unwrap();
        assert_eq!(
            provider.list_identities(),
            Err(IdentityProviderError::CorruptIdentityIndex)
        );
        store
            .set_secret(INDEX_ACCOUNT, &"x".repeat(MAX_INDEX_BYTES + 1))
            .unwrap();
        assert_eq!(
            provider.list_identities(),
            Err(IdentityProviderError::CorruptIdentityIndex)
        );
    }

    #[test]
    fn identity_delete_removes_private_and_public_index_entries() {
        let store = FakeStore::default();
        let provider = AgeIdentityProvider::with_store(store.clone());
        let identity = provider.create_identity().unwrap();
        let account = private_account(&identity.recipient_id);
        assert!(store.has_secret(&account));

        provider.delete_identity(&identity.recipient_id).unwrap();

        assert!(provider.list_identities().unwrap().is_empty());
        assert!(!store.has_secret(&account));
    }

    #[test]
    fn hardware_policy_denial_happens_before_credential_store_access() {
        // The verified envelope carries the signed hardware requirement into
        // the public unwrap operation; the software Age capability rejects it.
        let store = FakeStore::default();
        let provider = AgeIdentityProvider::with_store(store.clone());
        let before = store.reads.load(Ordering::SeqCst);
        assert_eq!(
            provider.check_hardware_policy(&MinimumRecipientHardware::HardwareBackedRequired),
            Err(IdentityProviderError::HardwarePolicyUnsatisfied)
        );
        assert_eq!(store.reads.load(Ordering::SeqCst), before);
    }

    #[test]
    fn invalid_provider_suite_and_ciphertext_bounds_are_rejected() {
        let provider = AgeIdentityProvider::with_store(FakeStore::default());
        let local = provider.create_identity().unwrap();
        let mut mismatched = test_record(&local, &[1; 32]);
        mismatched.provider_id = RecipientProvider::WindowsTpmRsa3072;
        assert!(matches!(
            provider.unwrap_recipient(&mismatched),
            Err(IdentityProviderError::UnsupportedRecipient)
        ));
        let mut oversized = test_record(&local, &[1; 32]);
        oversized.wrapped_imk.resize(MAX_WRAPPED_IMK_BYTES + 1, 0);
        assert!(matches!(
            provider.unwrap_recipient(&oversized),
            Err(IdentityProviderError::UnsupportedRecipient)
        ));
    }
}
