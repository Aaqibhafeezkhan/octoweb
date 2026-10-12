//! Installed browser extensions on disk, and the places they install from.
//!
//! `extensions.json` lists what is installed. An unpacked folder is loaded
//! where it lives, so editing it and reloading works the way "Load unpacked"
//! does in Chrome. Archives are copied under `extensions/`: a .zip or .xpi
//! as-is, a .crx (or a Chrome Web Store item) as the ZIP inside it — WebKit
//! loads a ZIP directly, so nothing is ever unpacked.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Chrome version the Web Store update service is told it serves. Items set
/// a `minimum_chrome_version`; an old value gets them refused.
const STORE_PRODVERSION: &str = "140.0";

/// Give up on a Web Store download after this many seconds.
const DOWNLOAD_TIMEOUT_SECS: &str = "120";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    /// Host of the extension's `webkit-extension://` pages and the key of its
    /// storage, so it must never change once installed.
    pub id: String,
    /// Folder or ZIP archive WebKit loads.
    pub path: PathBuf,
    pub enabled: bool,
    /// Permissions and match patterns the user granted, restored at load.
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub scope: Scope,
}

/// The workspaces an extension runs in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Every workspace, including ones created later. Also what a record
    /// written before workspace scoping means: those ran everywhere.
    #[default]
    All,
    /// These workspace ids only.
    Only(Vec<String>),
}

impl Scope {
    pub fn includes(&self, workspace: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Only(ids) => ids.iter().any(|id| id == workspace),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    /// Workspace id → the identifier of that workspace's extension controller,
    /// which names its WebKit storage. One per workspace keeps extension data
    /// as isolated as cookies are; random per profile keeps side-by-side
    /// profiles (`OCTOWEB_CONFIG_DIR`) apart.
    #[serde(default)]
    pub controllers: BTreeMap<String, [u8; 16]>,
    /// The single, profile-wide controller of the first extensions build. Its
    /// data stays reachable from the default workspace.
    #[serde(default, rename = "controller_id", skip_serializing)]
    shared_controller: Option<[u8; 16]>,
    pub extensions: Vec<Record>,
}

impl Store {
    pub fn new(controllers: BTreeMap<String, [u8; 16]>, extensions: Vec<Record>) -> Self {
        Self {
            controllers,
            shared_controller: None,
            extensions,
        }
    }
}

/// The installed list. An unreadable file is an error, not an empty list —
/// overwriting it would lose every install.
pub fn load() -> Result<Store, String> {
    let path = records_path();
    let mut store: Store = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if let Some(id) = store.shared_controller.take() {
        store
            .controllers
            .entry(crate::workspace::DEFAULT_WORKSPACE_ID.to_string())
            .or_insert(id);
    }
    Ok(store)
}

pub fn save(store: &Store) {
    let path = records_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(store) {
        Ok(s) => {
            if let Err(e) = fs::write(&path, s) {
                tracing::warn!(error = %e, "Failed to write extensions.json");
            }
        }
        Err(e) => tracing::warn!(error = %e, "Failed to serialize extensions"),
    }
}

fn records_path() -> PathBuf {
    crate::config::base_dir()
        .join("octoweb")
        .join("extensions.json")
}

fn archive_dir() -> PathBuf {
    crate::config::base_dir().join("octoweb").join("extensions")
}

/// A fresh id in Chrome's alphabet: 32 letters a–p.
pub fn new_id() -> String {
    crate::workspace::random_bytes16()
        .iter()
        .flat_map(|b| [b >> 4, b & 0x0f])
        .map(|n| char::from(b'a' + n))
        .collect()
}

/// Chrome Web Store item id from a store URL or a bare id.
pub fn store_id(input: &str) -> Option<String> {
    input
        .trim()
        .split(['/', '?', '#'])
        .find(|part| part.len() == 32 && part.bytes().all(|b| (b'a'..=b'p').contains(&b)))
        .map(str::to_string)
}

/// Download a Web Store item and store the ZIP inside its CRX. Blocking.
pub fn download_store_item(id: &str) -> Result<PathBuf, String> {
    let dir = archive_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let crx = dir.join(format!("{id}.crx.download"));
    let url = format!(
        "https://clients2.google.com/service/update2/crx?response=redirect&prodversion={STORE_PRODVERSION}&acceptformat=crx2,crx3&x=id%3D{id}%26uc"
    );
    let output = std::process::Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", DOWNLOAD_TIMEOUT_SECS, "-o"])
        .arg(&crx)
        .arg(&url)
        .output()
        .map_err(|e| format!("Couldn't run curl: {e}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(&crx);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Chrome Web Store download failed: {}",
            stderr.trim()
        ));
    }
    let bytes = fs::read(&crx).map_err(|e| format!("Couldn't read the download: {e}"));
    let _ = fs::remove_file(&crx);
    write_archive(id, crx_zip(&bytes?)?)
}

/// Where an install source lives once installed: a folder stays where it is,
/// an archive is copied in (a CRX unwrapped to its ZIP).
pub fn import(source: &Path, id: &str) -> Result<PathBuf, String> {
    if source.is_dir() {
        if !source.join("manifest.json").is_file() {
            return Err(format!("{} has no manifest.json", source.display()));
        }
        return Ok(source.to_path_buf());
    }
    let bytes = fs::read(source).map_err(|e| format!("Couldn't read {}: {e}", source.display()))?;
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("crx") => write_archive(id, crx_zip(&bytes)?),
        Some("zip" | "xpi") => write_archive(id, &bytes),
        _ => Err("Choose an extension folder, or a .zip, .xpi or .crx file.".into()),
    }
}

/// Every install gets its own archive name, so a pending update never
/// overwrites the archive the installed version is still running from.
fn write_archive(id: &str, zip: &[u8]) -> Result<PathBuf, String> {
    let dir = archive_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let dest = dir.join(format!("{id}-{}.zip", &new_id()[..8]));
    fs::write(&dest, zip).map_err(|e| format!("Couldn't write {}: {e}", dest.display()))?;
    Ok(dest)
}

/// Delete what [`import`] copied in. A folder loaded in place is the user's.
pub fn discard(path: &Path) {
    if path.starts_with(archive_dir()) {
        if let Err(e) = fs::remove_file(path) {
            tracing::warn!(error = %e, path = %path.display(), "Failed to delete extension archive");
        }
    }
}

/// The ZIP payload of a CRX2 or CRX3 package.
fn crx_zip(bytes: &[u8]) -> Result<&[u8], String> {
    let le32 = |at: usize| -> Option<usize> {
        let b = bytes.get(at..at + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    if !bytes.starts_with(b"Cr24") {
        return Err("Not a CRX package".into());
    }
    // CRX3: magic, version, header length, header. CRX2: magic, version,
    // public key length, signature length, key, signature.
    let start = match le32(4) {
        Some(3) => le32(8).map(|header| 12 + header),
        Some(2) => le32(8).zip(le32(12)).map(|(key, sig)| 16 + key + sig),
        Some(v) => return Err(format!("Unsupported CRX version {v}")),
        None => None,
    };
    start
        .and_then(|s| bytes.get(s..))
        .filter(|zip| zip.starts_with(b"PK"))
        .ok_or_else(|| "The CRX package holds no ZIP archive".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_ids_come_from_urls_or_bare_ids() {
        let id = "nngceckbapebfimnlniiiahkandclblb";
        assert_eq!(store_id(id).as_deref(), Some(id));
        assert_eq!(
            store_id(&format!(
                "https://chromewebstore.google.com/detail/bitwarden/{id}?hl=en"
            ))
            .as_deref(),
            Some(id)
        );
        assert_eq!(
            store_id(&format!("https://chrome.google.com/webstore/detail/{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(store_id("https://example.com/extension"), None);
        assert_eq!(store_id("nngceckbapebfimnlniiiahkandclblz"), None);
    }

    #[test]
    fn scopes_round_trip_and_old_records_run_everywhere() {
        let old = r#"{"controller_id":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16],
            "extensions":[{"id":"x","path":"/x","enabled":true}]}"#;
        let store: Store = serde_json::from_str(old).unwrap();
        assert_eq!(
            store.shared_controller,
            Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        );
        assert_eq!(store.extensions[0].scope, Scope::All);

        let mut record = store.extensions[0].clone();
        record.scope = Scope::Only(vec!["work".into()]);
        let saved = serde_json::to_string(&Store::new(BTreeMap::new(), vec![record])).unwrap();
        assert!(!saved.contains("controller_id"));
        let back: Store = serde_json::from_str(&saved).unwrap();
        assert!(back.extensions[0].scope.includes("work"));
        assert!(!back.extensions[0].scope.includes("home"));
    }

    #[test]
    fn new_ids_are_in_the_store_alphabet() {
        let id = new_id();
        assert_eq!(store_id(&id).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn crx_payloads_are_unwrapped() {
        let mut crx3 = b"Cr24".to_vec();
        crx3.extend(3u32.to_le_bytes());
        crx3.extend(2u32.to_le_bytes());
        crx3.extend(b"hhPKzip");
        assert_eq!(crx_zip(&crx3).unwrap(), b"PKzip");

        let mut crx2 = b"Cr24".to_vec();
        crx2.extend(2u32.to_le_bytes());
        crx2.extend(1u32.to_le_bytes());
        crx2.extend(2u32.to_le_bytes());
        crx2.extend(b"kssPKzip");
        assert_eq!(crx_zip(&crx2).unwrap(), b"PKzip");

        assert!(crx_zip(b"PKzip").is_err());
        let mut truncated = b"Cr24".to_vec();
        truncated.extend(3u32.to_le_bytes());
        truncated.extend(99u32.to_le_bytes());
        assert!(crx_zip(&truncated).is_err());
    }
}
