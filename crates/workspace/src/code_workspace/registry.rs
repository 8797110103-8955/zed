//! Remembers which workspace files were opened with which folders, so that a
//! window with the same folders is recognized as that workspace again.

use db::kvp::KeyValueStore;
use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use util::ResultExt as _;

const KNOWN_CODE_WORKSPACES_KEY: &str = "known_code_workspaces";
const MAX_KNOWN_CODE_WORKSPACES: usize = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct KnownCodeWorkspace {
    path: PathBuf,
    folders: Vec<PathBuf>,
}

/// Most recently used first.
#[derive(Default)]
struct KnownCodeWorkspaces(Vec<KnownCodeWorkspace>);

impl Global for KnownCodeWorkspaces {}

pub(super) fn init(cx: &mut App) {
    let known = KeyValueStore::global(cx)
        .read_kvp(KNOWN_CODE_WORKSPACES_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default();
    cx.set_global(KnownCodeWorkspaces(known));
}

/// The workspace file that was last used with exactly these folders.
pub fn code_workspace_for_folders(folders: &[PathBuf], cx: &App) -> Option<PathBuf> {
    if folders.is_empty() {
        return None;
    }
    cx.try_global::<KnownCodeWorkspaces>()?
        .0
        .iter()
        .find(|known| same_folder_set(&known.folders, folders))
        .map(|known| known.path.clone())
}

pub(super) fn record(path: PathBuf, folders: Vec<PathBuf>, cx: &mut App) {
    if !cx.has_global::<KnownCodeWorkspaces>() {
        return;
    }
    let entry = KnownCodeWorkspace { path, folders };
    let known = &mut cx.global_mut::<KnownCodeWorkspaces>().0;
    if known.first() == Some(&entry) {
        return;
    }
    known.retain(|known| known.path != entry.path);
    known.insert(0, entry);
    known.truncate(MAX_KNOWN_CODE_WORKSPACES);
    persist(cx);
}

pub(super) fn forget(path: &Path, cx: &mut App) {
    if !cx.has_global::<KnownCodeWorkspaces>() {
        return;
    }
    let known = &mut cx.global_mut::<KnownCodeWorkspaces>().0;
    let previous_len = known.len();
    known.retain(|known| known.path != path);
    if known.len() != previous_len {
        persist(cx);
    }
}

fn persist(cx: &mut App) {
    let Some(json) = serde_json::to_string(&cx.global::<KnownCodeWorkspaces>().0).log_err() else {
        return;
    };
    let kvp = KeyValueStore::global(cx);
    cx.background_executor()
        .spawn(async move {
            kvp.write_kvp(KNOWN_CODE_WORKSPACES_KEY.to_string(), json)
                .await
                .log_err();
        })
        .detach();
}

fn same_folder_set(left: &[PathBuf], right: &[PathBuf]) -> bool {
    left.iter().all(|path| right.contains(path)) && right.iter().all(|path| left.contains(path))
}
