//! Ad blocking for the TV browser: uBlock Origin Lite, fetched from its GitHub releases and
//! loaded into Chrome over DevTools (Chrome no longer accepts --load-extension).

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Paid services stay unfiltered so nothing can break their players or sign-in.
const NO_FILTERING: &[&str] = &["netflix.com", "hulu.com", "disneyplus.com", "paramountplus.com"];

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
    if fresh {
        return Ok(dir);
    }
    match download(&dir).await {
        Ok(()) => Ok(dir),
        // Offline: an older copy still blocks ads.
        Err(e) if dir.join("manifest.json").exists() => {
            crate::log(&format!("ad blocker update failed, keeping the old one: {e:#}"));
            Ok(dir)
        }
        Err(e) => Err(e),
    }
}

async fn download(dir: &PathBuf) -> Result<()> {
    let run = |cmd: &str, args: Vec<String>| {
        let cmd = cmd.to_string();
        async move {
            let out = Command::new(&cmd).args(&args).output().await.with_context(|| format!("{cmd} not found"))?;
            if !out.status.success() {
                bail!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(out.stdout)
        }
    };
    let api = "https://api.github.com/repos/uBlockOrigin/uBOL-home/releases/latest";
    let release: serde_json::Value =
        serde_json::from_slice(&run("curl", vec!["-fsSL".into(), "--max-time".into(), "20".into(), api.into()]).await?)?;
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
    run("curl", vec!["-fsSL".into(), "--max-time".into(), "120".into(), "-o".into(), zip.display().to_string(), url])
        .await?;
    let ext = tmp.join("ext");
    run("unzip", vec!["-q".into(), zip.display().to_string(), "-d".into(), ext.display().to_string()]).await?;

    // Start with the paid services in "no filtering" mode.
    let modes = ext.join("js/mode-manager.js");
    let src = std::fs::read_to_string(&modes).context("unexpected uBlock Origin Lite layout")?;
    let list = NO_FILTERING.iter().map(|d| format!("'{d}'")).collect::<Vec<_>>().join(", ");
    let patched = src.replacen(
        "export const defaultFilteringModes = {\n    none: [],",
        &format!("export const defaultFilteringModes = {{\n    none: [ {list} ],"),
        1,
    );
    if patched == src {
        crate::log("ad blocker: couldn't exempt the paid services (uBOL changed); filtering everywhere");
    }
    std::fs::write(&modes, patched)?;

    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&ext, dir)?;
    let _ = std::fs::remove_dir_all(&tmp);
    crate::log(&format!("ad blocker {} installed", release["tag_name"].as_str().unwrap_or("?")));
    Ok(())
}
