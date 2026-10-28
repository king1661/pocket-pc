//! Ad blocking for the TV browser: uBlock Origin Lite, fetched from its GitHub releases and
//! loaded into Chrome over DevTools (Chrome no longer accepts --load-extension).

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Players that stall or refuse to start when an ad or tracker request is blocked.
/// Paid apps, plus the free ones whose video is stitched to ads.
const NO_FILTERING: &[&str] = &[
    "netflix.com",
    "hulu.com",
    "disneyplus.com",
    "paramountplus.com",
    "tubitv.com",
    "pluto.tv",
    "plex.tv",
];

/// Re-download after this long to pick up new filter lists.
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

fn dir() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join("couchtv/ublock")
}

/// The unpacked extension, downloaded (or refreshed) when needed.
pub async fn ensure() -> Result<PathBuf> {
    let dir = dir();
    let fresh = std::fs::metadata(dir.join("manifest.json"))
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default() < MAX_AGE);
    if !fresh {
        if let Err(e) = download(&dir).await {
            if !dir.join("manifest.json").exists() {
                return Err(e);
            }
            // Offline: an older copy still blocks ads.
            crate::log(&format!(
                "ad blocker update failed, keeping the old one: {e:#}"
            ));
        }
    }
    // Re-apply on every launch. A fresh download resets the file, and an already
    // installed copy keeps its own saved host list until this union runs.
    patch_installed(&dir)?;
    Ok(dir)
}

async fn download(dir: &PathBuf) -> Result<()> {
    let run = |cmd: &str, args: Vec<String>| {
        let cmd = cmd.to_string();
        async move {
            let out = Command::new(&cmd)
                .args(&args)
                .output()
                .await
                .with_context(|| format!("{cmd} not found"))?;
            if !out.status.success() {
                bail!(
                    "{cmd} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(out.stdout)
        }
    };
    let api = "https://api.github.com/repos/uBlockOrigin/uBOL-home/releases/latest";
    let release: serde_json::Value = serde_json::from_slice(
        &run(
            "curl",
            vec!["-fsSL".into(), "--max-time".into(), "20".into(), api.into()],
        )
        .await?,
    )?;
    let url = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["browser_download_url"].as_str())
        .find(|u| u.ends_with(".chromium.zip"))
        .context("no Chromium build in the latest uBlock Origin Lite release")?
        .to_string();

    let tmp = dir.with_extension("new");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let zip = tmp.join("ubol.zip");
    run(
        "curl",
        vec![
            "-fsSL".into(),
            "--max-time".into(),
            "120".into(),
            "-o".into(),
            zip.display().to_string(),
            url,
        ],
    )
    .await?;
    let ext = tmp.join("ext");
    run(
        "unzip",
        vec![
            "-q".into(),
            zip.display().to_string(),
            "-d".into(),
            ext.display().to_string(),
        ],
    )
    .await?;

    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&ext, dir)?;
    let _ = std::fs::remove_dir_all(&tmp);
    crate::log(&format!(
        "ad blocker {} installed",
        release["tag_name"].as_str().unwrap_or("?")
    ));
    Ok(())
}

/// Write the current exempt hosts into uBlock Origin Lite, and make a saved
/// host list pick those hosts up too (the extension remembers the first list).
fn patch_installed(dir: &std::path::Path) -> Result<()> {
    let modes = dir.join("js/mode-manager.js");
    let src = std::fs::read_to_string(&modes).context("unexpected uBlock Origin Lite layout")?;
    let Some(patched) = patch_modes(&src) else {
        crate::log(
            "ad blocker: couldn't exempt streaming players (uBOL changed); filtering everywhere",
        );
        return Ok(());
    };
    if patched != src {
        std::fs::write(&modes, patched)?;
    }
    Ok(())
}

fn patch_modes(src: &str) -> Option<String> {
    let list = NO_FILTERING
        .iter()
        .map(|d| format!("'{d}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let marker = "none: [";
    let start = src.find(marker)? + marker.len();
    let end = start + src[start..].find(']')?;
    let mut patched = String::new();
    patched.push_str(&src[..start]);
    patched.push(' ');
    patched.push_str(&list);
    patched.push(' ');
    patched.push_str(&src[end..]);
    let hook = "    userModes = unserializeModeDetails(userModes);\n";
    let extra = "    userModes = unserializeModeDetails(userModes);\n    for (const hn of defaultFilteringModes.none) userModes.none.add(hn);\n";
    if !patched.contains("defaultFilteringModes.none) userModes.none.add") {
        patched = patched.replacen(hook, extra, 1);
        if !patched.contains("defaultFilteringModes.none) userModes.none.add") {
            return None;
        }
    }
    Some(patched)
}

#[cfg(test)]
mod tests {
    use super::patch_modes;

    #[test]
    fn exemptions_apply_once_and_stick() {
        let fresh = "export const defaultFilteringModes = {\n    none: [],\n};\n    userModes = unserializeModeDetails(userModes);\n";
        let patched = patch_modes(fresh).expect("fresh layout");
        assert!(patched.contains("'tubitv.com'"));
        assert!(patched.contains("'pluto.tv'"));
        assert!(patched.contains("'plex.tv'"));
        assert!(patched.contains("userModes.none.add(hn)"));
        assert_eq!(patch_modes(&patched).as_deref(), Some(patched.as_str()));

        let saved = patched.replace("'plex.tv'", "'old.example'");
        let updated = patch_modes(&saved).expect("saved layout");
        assert!(updated.contains("'plex.tv'"));
        assert!(!updated.contains("old.example"));
        assert_eq!(updated.matches("userModes.none.add").count(), 1);
    }
}
