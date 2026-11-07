use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::SigningKey;
use rootglass_manifest::Glass;
use rootglass_signature::{
    load_signing_key, package_glass_directory, package_glass_directory_at, sign_glass_directory,
    sign_glass_directory_at, verify_glass_archive, verify_glass_dir, TrustStore, TrustedPublisher,
};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeveloperAction {
    SignDirectory,
    PackageArchive,
}

impl DeveloperAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::SignDirectory => "Sign .glass directory",
            Self::PackageArchive => "Create .glass package",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeveloperRequest {
    pub action: DeveloperAction,
    pub source: PathBuf,
    pub key_file: PathBuf,
    pub output: Option<PathBuf>,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeveloperResult {
    pub action: DeveloperAction,
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub subject_count: usize,
    pub output_path: PathBuf,
    pub verification: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeveloperPanel {
    Closed,
    Menu,
    Working(DeveloperAction),
    Complete(DeveloperResult),
    Failed {
        action: DeveloperAction,
        detail: String,
    },
}

pub fn execute_developer_request(request: DeveloperRequest) -> Result<DeveloperResult, String> {
    let glass = Glass::load(&request.source).map_err(|error| error.to_string())?;
    glass
        .validate_manifest_only()
        .map_err(|error| error.to_string())?;
    let signature_path = PathBuf::from(&glass.manifest.publisher.signature);

    // Keep private material on this worker's stack. The UI model and completion only
    // receive public signing metadata and the external file path is never logged.
    let signing_key = load_signing_key(&request.key_file).map_err(|error| error.to_string())?;
    let report = sign(&request, &signing_key)?;
    let trust_store = signer_trust_store(
        &signing_key,
        &report.publisher_name,
        &report.publisher_identity,
        &report.key_id,
    );
    let (output_path, verification) = match request.action {
        DeveloperAction::SignDirectory => {
            let output_path = request.source.join(signature_path);
            let verified = verify_glass_dir(
                &request.source,
                &glass.manifest.publisher.signature,
                &trust_store,
            )
            .map_err(|error| error.to_string())?;
            ensure_report_matches(&report, &verified)?;
            (
                output_path,
                "Passed with the Rootglass directory verifier".to_string(),
            )
        }
        DeveloperAction::PackageArchive => {
            let output_path = request
                .output
                .ok_or_else(|| "an output .glass path is required".to_string())?;
            let verified = verify_glass_archive(
                &output_path,
                &glass.manifest.publisher.signature,
                &trust_store,
            )
            .map_err(|error| error.to_string())?;
            ensure_report_matches(&report, &verified)?;
            (
                output_path,
                "Passed with the Rootglass archive verifier".to_string(),
            )
        }
    };

    Ok(DeveloperResult {
        action: request.action,
        publisher_name: report.publisher_name,
        publisher_identity: report.publisher_identity,
        key_id: report.key_id,
        subject_count: report.subject_count,
        output_path,
        verification,
    })
}

fn sign(
    request: &DeveloperRequest,
    signing_key: &SigningKey,
) -> Result<rootglass_signature::SigningReport, String> {
    match request.action {
        DeveloperAction::SignDirectory => match request.created_at.as_deref() {
            Some(created_at) => sign_glass_directory_at(&request.source, signing_key, created_at),
            None => sign_glass_directory(&request.source, signing_key),
        },
        DeveloperAction::PackageArchive => {
            let output = request
                .output
                .as_ref()
                .ok_or_else(|| "an output .glass path is required".to_string())?;
            match request.created_at.as_deref() {
                Some(created_at) => {
                    package_glass_directory_at(&request.source, output, signing_key, created_at)
                }
                None => package_glass_directory(&request.source, output, signing_key),
            }
        }
    }
    .map_err(|error| error.to_string())
}

fn signer_trust_store(
    signing_key: &SigningKey,
    publisher_name: &str,
    publisher_identity: &str,
    key_id: &str,
) -> TrustStore {
    TrustStore::new([TrustedPublisher {
        name: publisher_name.to_string(),
        identity: publisher_identity.to_string(),
        key_id: key_id.to_string(),
        public_key_base64url: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes()),
    }])
}

fn ensure_report_matches(
    signed: &rootglass_signature::SigningReport,
    verified: &rootglass_signature::VerificationReport,
) -> Result<(), String> {
    if signed.publisher_name != verified.publisher_name
        || signed.publisher_identity != verified.publisher_identity
        || signed.key_id != verified.key_id
        || signed.subject_count != verified.subject_count
    {
        return Err("the generated signature did not match the runtime verification report".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use std::fs::{self, File};
    use std::io::Read;
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::ZipArchive;

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        key_file: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time is after unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "rootglass-tui-developer-{}-{nonce}",
                std::process::id()
            ));
            let source = root.join("example.glass");
            fs::create_dir_all(source.join("metadata")).expect("create metadata directory");
            fs::create_dir_all(source.join("signatures")).expect("create signatures directory");
            fs::create_dir_all(source.join("assets")).expect("create generated assets directory");
            let example =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
            for name in [
                "README.md",
                "metadata/README.md",
                "rootglass.toml",
                "signatures/glass.dsse.json",
            ] {
                fs::copy(example.join(name), source.join(name)).expect("copy fixture file");
            }
            fs::write(source.join("rootfs.img"), b"test root filesystem")
                .expect("create rootfs fixture");
            fs::write(
                source.join("assets/generated.img"),
                b"generated image asset",
            )
            .expect("create generated image fixture");

            let key_file = root.join("signing-key.seed");
            let seed: [u8; 32] = Sha256::digest(nonce.to_le_bytes()).into();
            fs::write(&key_file, seed).expect("write ephemeral test signing key");
            Self {
                root,
                source,
                key_file,
            }
        }

        fn request(&self, action: DeveloperAction, output: Option<PathBuf>) -> DeveloperRequest {
            DeveloperRequest {
                action,
                source: self.source.clone(),
                key_file: self.key_file.clone(),
                output,
                created_at: Some("2026-01-02T03:04:05Z".into()),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn signs_directory_with_external_key_and_deterministic_timestamp() {
        let fixture = Fixture::new();
        let result =
            execute_developer_request(fixture.request(DeveloperAction::SignDirectory, None))
                .expect("directory is signed and verified");

        assert_eq!(result.publisher_name, "Rootglass Examples");
        assert_eq!(result.publisher_identity, "rootglass.dev/examples");
        assert!(result.key_id.starts_with("ed25519:"));
        assert_eq!(result.subject_count, 5);
        assert!(result.output_path.ends_with("signatures/glass.dsse.json"));
        assert!(result.verification.contains("directory verifier"));

        let envelope: serde_json::Value = serde_json::from_slice(
            &fs::read(&result.output_path).expect("signature envelope is written"),
        )
        .expect("signature envelope is JSON");
        let payload = URL_SAFE_NO_PAD
            .decode(envelope["payload"].as_str().expect("encoded payload"))
            .expect("payload uses base64url");
        let statement: serde_json::Value =
            serde_json::from_slice(&payload).expect("statement is JSON");
        assert_eq!(statement["predicate"]["createdAt"], "2026-01-02T03:04:05Z");
    }

    #[test]
    fn production_package_verifies_every_subject_including_generated_img() {
        let fixture = Fixture::new();
        let output = fixture.root.join("release.glass");
        let result = execute_developer_request(
            fixture.request(DeveloperAction::PackageArchive, Some(output.clone())),
        )
        .expect("canonical archive is signed and verified");

        assert_eq!(result.output_path, output);
        assert_eq!(result.subject_count, 5);
        assert!(result.verification.contains("archive verifier"));

        let mut archive = ZipArchive::new(File::open(&output).expect("open package"))
            .expect("package is a ZIP archive");
        let mut envelope_bytes = Vec::new();
        archive
            .by_name("signatures/glass.dsse.json")
            .expect("package has a DSSE envelope")
            .read_to_end(&mut envelope_bytes)
            .expect("read envelope");
        let envelope: serde_json::Value =
            serde_json::from_slice(&envelope_bytes).expect("envelope is JSON");
        let payload = URL_SAFE_NO_PAD
            .decode(envelope["payload"].as_str().expect("encoded payload"))
            .expect("payload uses base64url");
        let statement: serde_json::Value =
            serde_json::from_slice(&payload).expect("statement is JSON");
        let subjects = statement["subject"]
            .as_array()
            .expect("subjects are an array");
        assert!(subjects
            .iter()
            .any(|subject| { subject["name"] == "assets/generated.img" }));
    }

    #[test]
    fn missing_and_invalid_keys_fail_without_exposing_key_bytes() {
        let fixture = Fixture::new();
        let missing = DeveloperRequest {
            key_file: fixture.root.join("missing.seed"),
            ..fixture.request(DeveloperAction::SignDirectory, None)
        };
        let missing_error = execute_developer_request(missing).unwrap_err();
        assert!(missing_error.contains("failed to read signing key"));

        let invalid_material = b"PRIVATE-KEY-MATERIAL-MUST-NOT-BE-PRINTED";
        fs::write(&fixture.key_file, invalid_material).expect("write invalid test key");
        let invalid_error =
            execute_developer_request(fixture.request(DeveloperAction::SignDirectory, None))
                .unwrap_err();
        assert!(invalid_error.contains("must contain a 32-byte"));
        assert!(!invalid_error.contains(std::str::from_utf8(invalid_material).unwrap()));
    }

    #[test]
    fn existing_package_output_and_signing_failures_are_reported() {
        let fixture = Fixture::new();
        let output = fixture.root.join("existing.glass");
        fs::write(&output, b"preserve existing output").expect("create existing output");
        let error = execute_developer_request(
            fixture.request(DeveloperAction::PackageArchive, Some(output.clone())),
        )
        .unwrap_err();
        assert!(error.contains("output already exists"));
        assert_eq!(fs::read(output).unwrap(), b"preserve existing output");

        let invalid_source = DeveloperRequest {
            source: fixture.root.join("missing.glass"),
            ..fixture.request(DeveloperAction::SignDirectory, None)
        };
        assert!(execute_developer_request(invalid_source)
            .unwrap_err()
            .contains("failed to read manifest"));
    }
}
