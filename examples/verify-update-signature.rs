//! Offline verification of a Tauri updater artifact against this application's public key.
use anyhow::{Context, Result};
use base64::Engine;

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let artifact = args
        .next()
        .context("usage: verify-update-signature <artifact> <artifact.sig>")?;
    let signature = args.next().context("missing signature file")?;
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../src-tauri/tauri.conf.json"))?;
    let decode = |value: &str| -> Result<String> {
        Ok(String::from_utf8(
            base64::engine::general_purpose::STANDARD.decode(value.trim())?,
        )?)
    };
    let key = minisign_verify::PublicKey::decode(&decode(
        config["plugins"]["updater"]["pubkey"]
            .as_str()
            .context("missing public key")?,
    )?)?;
    let signature =
        minisign_verify::Signature::decode(&decode(&std::fs::read_to_string(signature)?)?)?;
    key.verify(&std::fs::read(artifact)?, &signature, true)?;
    println!("Updater artifact signature verified with the configured public key.");
    Ok(())
}
