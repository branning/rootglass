//! Rebuild the checked-in development fixture's DSSE envelope.
//!
//! This deterministic, public test key is only for the sample glass. Never use
//! it to sign a production artifact.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

const KEY_ID: &str = "ed25519:rootglass-example-fixture";
const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
const PREDICATE_TYPE: &str = "https://rootglass.dev/predicate/glass/v1";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let glass = root.join("examples/hello-world.glass");

    let mut subjects = Vec::new();
    for name in ["README.md", "metadata/README.md", "rootglass.toml"] {
        let digest = Sha256::digest(fs::read(glass.join(name))?);
        let hex_digest = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        subjects.push(json!({
            "name": name,
            "digest": {"sha256": hex_digest}
        }));
    }

    let statement = json!({
        "_type": STATEMENT_TYPE,
        "predicate": {
            "createdAt": "2026-11-06T00:00:00Z",
            "glassIdentifier": "io.rootglass.examples.hello-world",
            "glassVersion": "0.1.0",
            "publisher": {
                "identity": "rootglass.dev/examples",
                "keyId": KEY_ID,
                "name": "Rootglass Examples"
            },
            "schemaVersion": 1
        },
        "predicateType": PREDICATE_TYPE,
        "subject": subjects
    });
    let payload = serde_json::to_vec(&statement)?;
    let pae = dsse_pae(PAYLOAD_TYPE.as_bytes(), &payload);

    // Public deterministic fixture key; intentionally not a secret.
    let seed: [u8; 32] =
        Sha256::digest(b"Rootglass public development fixture signing key v1").into();
    let signing_key = SigningKey::from_bytes(&seed);
    let signature = signing_key.sign(&pae);
    let envelope = json!({
        "payloadType": PAYLOAD_TYPE,
        "payload": URL_SAFE_NO_PAD.encode(payload),
        "signatures": [{
            "keyid": KEY_ID,
            "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes())
        }]
    });
    let signature_path = glass.join("signatures/glass.dsse.json");
    fs::write(
        &signature_path,
        format!("{}\n", serde_json::to_string_pretty(&envelope)?),
    )?;

    println!(
        "fixture public key (base64url): {}",
        URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes())
    );
    println!("updated: {}", signature_path.strip_prefix(&root)?.display());
    Ok(())
}

fn dsse_pae(payload_type: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = b"DSSEv1 ".to_vec();
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type);
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}
