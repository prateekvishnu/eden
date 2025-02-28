/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::anyhow;
use byteorder::{BigEndian, ReadBytesExt};
use edenfs_error::{EdenFsError, Result, ResultExt};
use edenfs_utils::path_from_bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use thrift_types::edenfs::types::{MountInfo, MountState};
use toml::value::Value;
use uuid::Uuid;

use crate::redirect::{deserialize_redirections, RedirectionType};
use crate::EdenFsInstance;

// files in the client directory (aka data_dir aka state_dir)
const MOUNT_CONFIG: &str = "config.toml";
const SNAPSHOT: &str = "SNAPSHOT";

// Magical snapshot strings
const SNAPSHOT_MAGIC_1: &[u8] = b"eden\x00\x00\x00\x01";
const SNAPSHOT_MAGIC_2: &[u8] = b"eden\x00\x00\x00\x02";
const SNAPSHOT_MAGIC_3: &[u8] = b"eden\x00\x00\x00\x03";
const SNAPSHOT_MAGIC_4: &[u8] = b"eden\x00\x00\x00\x04";

const SUPPORTED_REPOS: &[&str] = &["git", "hg", "recas"];
const SUPPORTED_MOUNT_PROTOCOLS: &[&str] = &["fuse", "nfs", "prjfs"];

#[derive(Deserialize, Debug)]
struct Repository {
    path: PathBuf,

    #[serde(rename = "type", deserialize_with = "deserialize_repo_type")]
    repo_type: String,

    #[serde(
        deserialize_with = "deserialize_protocol",
        default = "default_protocol"
    )]
    protocol: String,

    #[serde(default = "default_guid")]
    guid: Uuid,

    #[serde(rename = "case-sensitive", default = "default_case_sensitive")]
    case_sensitive: bool,

    #[serde(rename = "require-utf8-path", default = "default_require_utf8_path")]
    require_utf8_path: bool,

    #[serde(rename = "enable-tree-overlay", default)]
    enable_tree_overlay: bool,
}

fn deserialize_repo_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;

    if SUPPORTED_REPOS.iter().any(|v| v == &s) {
        Ok(s)
    } else {
        Err(serde::de::Error::custom(format!(
            "Unsupported value: `{}`. Must be one of: {}",
            s,
            SUPPORTED_REPOS.join(", ")
        )))
    }
}

fn deserialize_protocol<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;

    if SUPPORTED_MOUNT_PROTOCOLS.iter().any(|v| v == &s) {
        Ok(s)
    } else {
        Err(serde::de::Error::custom(format!(
            "Unsupported value: `{}`. Must be one of: {}",
            s,
            SUPPORTED_MOUNT_PROTOCOLS.join(", ")
        )))
    }
}

fn default_protocol() -> String {
    if cfg!(windows) {
        "prjfs".to_string()
    } else {
        "fuse".to_string()
    }
}

fn default_guid() -> Uuid {
    Uuid::new_v4()
}

fn default_case_sensitive() -> bool {
    cfg!(target_os = "linux")
}

fn default_require_utf8_path() -> bool {
    // Existing repositories may have non-utf8 files, thus allow them by default
    true
}

#[derive(Deserialize, Debug)]
struct PrefetchProfiles {
    #[serde(deserialize_with = "deserialize_active", default)]
    active: Vec<String>,
}

fn deserialize_active<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let unvalidated_arr: Vec<Value> = Vec::deserialize(deserializer)?;
    let mut arr = Vec::new();
    for val in unvalidated_arr {
        if let Some(s) = val.as_str() {
            arr.push(s.to_string());
        } else {
            return Err(serde::de::Error::custom(format!(
                "Unsupported [profiles] active type {}. Must be string.",
                val
            )));
        }
    }

    Ok(arr)
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct PredictivePrefetch {
    #[serde(default)]
    predictive_prefetch_active: bool,

    #[serde(default)]
    predictive_prefetch_num_dirs: u32,
}

#[derive(Deserialize, Debug)]
struct CheckoutConfig {
    repository: Repository,

    #[serde(deserialize_with = "deserialize_redirections", default)]
    redirections: BTreeMap<PathBuf, RedirectionType>,

    profiles: Option<PrefetchProfiles>,

    #[serde(rename = "predictive-prefetch", default)]
    predictive_prefetch: Option<PredictivePrefetch>,
}

impl CheckoutConfig {
    /// Reads checkout config information from config.toml and
    /// returns an Err if it is not properly formatted or does not exist.
    fn parse_config(state_dir: PathBuf) -> Result<CheckoutConfig> {
        let config_path = state_dir.join(MOUNT_CONFIG);
        let content = String::from_utf8(std::fs::read(config_path).from_err()?).from_err()?;
        let config: CheckoutConfig = toml::from_str(&content).from_err()?;
        Ok(config)
    }
}

pub struct SnapshotState {
    pub working_copy_parent: String,
    pub last_checkout_hash: String,
}

impl SnapshotState {
    fn new(working_copy_parent: String, last_checkout_hash: String) -> Self {
        Self {
            working_copy_parent,
            last_checkout_hash,
        }
    }
}

/// Represents an edenfs checkout with mount information as well as information from configuration
#[derive(Serialize)]
pub struct EdenFsCheckout {
    /// E.g., /data/sandcastle/boxes/fbsource
    #[serde(skip)]
    path: PathBuf,
    /// E.g., /home/unixname/local/.eden/clients/fbsource
    data_dir: PathBuf,
    /// This is None when it's just configured but not actively mounted in eden
    #[serde(serialize_with = "serialize_state")]
    state: Option<MountState>,
    /// If this is false, that means this model is only populated with mount info from edenfs
    /// As opposed to being populated with information from the configuration & live mount info.
    configured: bool,
    backing_repo: Option<PathBuf>,
    #[serde(skip)]
    pub(crate) redirections: Option<BTreeMap<PathBuf, RedirectionType>>,
}

impl EdenFsCheckout {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    pub fn fsck_dir(&self) -> PathBuf {
        self.data_dir.join("fsck")
    }

    fn encode_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            write!(&mut s, "{:02x}", b).unwrap();
        }
        s
    }

    /// Returns a SnapshotState representing EdenFS working copy parent as well as the last checked
    /// out revision.
    pub fn get_snapshot(&self) -> Result<SnapshotState> {
        let snapshot_path = self.data_dir.join(SNAPSHOT);
        let mut f = File::open(&snapshot_path).from_err()?;
        let mut header = [0u8; 8];
        f.read(&mut header).from_err()?;
        if header == SNAPSHOT_MAGIC_1 {
            let mut snapshot = [0u8; 20];
            f.read(&mut snapshot).from_err()?;
            let decoded = EdenFsCheckout::encode_hex(&snapshot);
            Ok(SnapshotState::new(decoded.clone(), decoded))
        } else if header == SNAPSHOT_MAGIC_2 {
            let body_length = f.read_u32::<BigEndian>().from_err()?;
            let mut buf = vec![0u8; body_length as usize];
            f.read_exact(&mut buf).from_err()?;
            let decoded = std::str::from_utf8(&buf).from_err()?.to_string();
            Ok(SnapshotState::new(decoded.clone(), decoded))
        } else if header == SNAPSHOT_MAGIC_3 {
            let _pid = f.read_u32::<BigEndian>().from_err()?;

            let from_length = f.read_u32::<BigEndian>().from_err()?;
            let mut from_buf = vec![0u8; from_length as usize];
            f.read_exact(&mut from_buf).from_err()?;

            let to_length = f.read_u32::<BigEndian>().from_err()?;
            let mut to_buf = vec![0u8; to_length as usize];
            f.read_exact(&mut to_buf).from_err()?;

            // TODO(xavierd): return a proper object that the caller could use.
            Err(EdenFsError::Other(anyhow!(
                "A checkout operation is ongoing from {} to {}",
                std::str::from_utf8(&from_buf).from_err()?,
                std::str::from_utf8(&to_buf).from_err()?
            )))
        } else if header == SNAPSHOT_MAGIC_4 {
            let working_copy_parent_length = f.read_u32::<BigEndian>().from_err()?;
            let mut working_copy_parent_buf = vec![0u8; working_copy_parent_length as usize];
            f.read_exact(&mut working_copy_parent_buf).from_err()?;

            let checked_out_length = f.read_u32::<BigEndian>().from_err()?;
            let mut checked_out_buf = vec![0u8; checked_out_length as usize];
            f.read_exact(&mut checked_out_buf).from_err()?;

            Ok(SnapshotState::new(
                std::str::from_utf8(&working_copy_parent_buf)
                    .from_err()?
                    .to_string(),
                std::str::from_utf8(&checked_out_buf)
                    .from_err()?
                    .to_string(),
            ))
        } else {
            Err(EdenFsError::Other(anyhow!(
                "SNAPSHOT file has invalid header"
            )))
        }
    }

    pub fn backing_repo(&self) -> Option<PathBuf> {
        self.backing_repo.clone()
    }

    fn from_mount_info(path: PathBuf, thrift_mount: MountInfo) -> Result<EdenFsCheckout> {
        Ok(EdenFsCheckout {
            path,
            data_dir: path_from_bytes(&thrift_mount.edenClientPath)?,
            state: Some(thrift_mount.state),
            configured: false,
            backing_repo: match thrift_mount.backingRepoPath {
                Some(path_string) => Some(path_from_bytes(&path_string)?),
                None => None,
            },
            redirections: None,
        })
    }

    fn from_config(path: PathBuf, data_dir: PathBuf, config: CheckoutConfig) -> EdenFsCheckout {
        EdenFsCheckout {
            path,
            data_dir,
            state: None,
            configured: true,
            backing_repo: Some(config.repository.path.clone()),
            redirections: Some(config.redirections),
        }
    }

    fn update_with_config(&mut self, config: CheckoutConfig) {
        if self.backing_repo.is_none() {
            self.backing_repo = Some(config.repository.path.clone());
        }
        self.configured = true;
    }
}

fn serialize_state<S>(field: &Option<MountState>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(&match *field {
        Some(state) => {
            format!("{}", state)
        }
        None => "NOT_RUNNING".to_string(),
    })
}

impl fmt::Display for EdenFsCheckout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let suffix = if self.configured {
            ""
        } else {
            " (unconfigured)"
        };

        let state_str = match self.state {
            Some(state) => {
                if state == MountState::RUNNING {
                    String::new()
                } else {
                    format!(" ({})", state)
                }
            }
            None => " (not mounted)".to_string(),
        };

        write!(f, "{}{}{}", self.path.display(), state_str, suffix)
    }
}

fn config_directory(instance: &EdenFsInstance, client_name: &String) -> PathBuf {
    instance.clients_dir().join(client_name.clone())
}

/// Return information about all checkouts defined in EdenFS's configuration files
/// and all information about mounted checkouts from the eden daemon
pub async fn get_mounts(instance: &EdenFsInstance) -> Result<BTreeMap<PathBuf, EdenFsCheckout>> {
    // Get all configured checkout info (including not mounted / not active ones) from configs
    let mut configs: Vec<(PathBuf, PathBuf, CheckoutConfig)> = Vec::new();
    for (mount_path, client_name) in instance.get_configured_mounts_map()? {
        configs.push((
            mount_path,
            config_directory(instance, &client_name),
            CheckoutConfig::parse_config(config_directory(instance, &client_name))?,
        ));
    }

    // Get active mounted checkouts info from eden daemon
    let client = instance.connect(Some(Duration::from_secs(3))).await;
    let mounted_checkouts = match client {
        Ok(client) => Some(client.listMounts().await.from_err()?),
        Err(_) => None, // eden daemon not running
    };

    // Combine mount info from active mounts and mount info from config files
    let mut mount_points = BTreeMap::new();
    if let Some(mounts) = mounted_checkouts {
        for thrift_mount in mounts {
            let path = path_from_bytes(&thrift_mount.mountPoint)?;
            mount_points.insert(
                path.clone(),
                EdenFsCheckout::from_mount_info(path.clone(), thrift_mount)?,
            );
        }
    }

    for (path, data_dir, config) in configs {
        match mount_points.get_mut(&path) {
            Some(mount_info) => {
                mount_info.update_with_config(config);
            }
            None => {
                mount_points.insert(
                    path.clone(),
                    EdenFsCheckout::from_config(path.clone(), data_dir, config),
                );
            }
        };
    }

    Ok(mount_points)
}

#[cfg(windows)]
#[derive(Deserialize)]
struct WindowsEdenConfigInner {
    socket: PathBuf,
    root: PathBuf,
    client: PathBuf,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct WindowsEdenConfig {
    #[serde(rename = "Config")]
    config: WindowsEdenConfigInner,
}

#[cfg(windows)]
fn get_checkout_root_state(path: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let mut checkout_root = None;
    let mut checkout_state_dir = None;

    // On Windows, walk backwards through the path until you find the `.eden` folder
    let mut curr_dir = Some(path.clone());
    while let Some(candidate_dir) = curr_dir {
        if candidate_dir.join(".eden").exists() {
            let config_file = candidate_dir.join(".eden").join("config");
            let config = std::fs::read_to_string(config_file).from_err()?;
            let config = toml::from_str::<WindowsEdenConfig>(&config).from_err()?;
            checkout_root = Some(config.config.root);
            checkout_state_dir = Some(config.config.client);
            break;
        } else {
            curr_dir = candidate_dir.parent();
        }
    }
    Ok((checkout_root, checkout_state_dir))
}

#[cfg(not(windows))]
fn get_checkout_root_state(path: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    // We will get an error if any of these symlinks do not exist
    let eden_socket_path = fs::read_link(path.join(".eden").join("socket"));
    if eden_socket_path.is_ok() {
        let checkout_root = fs::read_link(path.join(".eden").join("root")).ok();
        let checkout_state_dir = fs::read_link(path.join(".eden").join("client")).ok();
        Ok((checkout_root, checkout_state_dir))
    } else {
        Ok((None, None))
    }
}

/// If the path provided is an eden checkout, this returns an object representing that checkout.
/// Otherwise, if the path provided is not an eden checkout, this returns None.
pub fn find_checkout(instance: &EdenFsInstance, path: &Path) -> Result<EdenFsCheckout> {
    // Resolve symlinks and get absolute path
    let path = path.canonicalize().from_err()?;

    // Check if it is a mounted checkout
    let (checkout_root, checkout_state_dir) = get_checkout_root_state(&path)?;

    if checkout_root.is_none() {
        // Find `checkout_path` that `path` is a sub path of
        let all_checkouts = instance.get_configured_mounts_map()?;
        if let Some(item) = all_checkouts
            .iter()
            .find(|&(checkout_path, _)| path.starts_with(checkout_path))
        {
            let (checkout_path, checkout_name) = item;
            let checkout_state_dir = config_directory(instance, checkout_name);
            Ok(EdenFsCheckout::from_config(
                PathBuf::from(checkout_path),
                checkout_state_dir.clone(),
                CheckoutConfig::parse_config(checkout_state_dir)?,
            ))
        } else {
            Err(EdenFsError::Other(anyhow!(
                "Checkout path {} is not handled by EdenFS",
                path.display()
            )))
        }
    } else if checkout_state_dir.is_none() {
        let all_checkouts = instance.get_configured_mounts_map()?;
        let checkout_path = checkout_root.unwrap();
        if let Some(checkout_name) = all_checkouts.get(&checkout_path) {
            let checkout_state_dir = config_directory(instance, checkout_name);
            Ok(EdenFsCheckout::from_config(
                checkout_path,
                checkout_state_dir.clone(),
                CheckoutConfig::parse_config(checkout_state_dir)?,
            ))
        } else {
            Err(EdenFsError::Other(anyhow!(
                "unknown checkout {}",
                checkout_path.display()
            )))
        }
    } else {
        Ok(EdenFsCheckout::from_config(
            checkout_root.unwrap(),
            checkout_state_dir.as_ref().unwrap().clone(),
            CheckoutConfig::parse_config(checkout_state_dir.unwrap())?,
        ))
    }
}
