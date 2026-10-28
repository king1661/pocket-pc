//! Where the TV goes: which screen shows it (moved with a KWin script) and which
//! speakers play it (PipeWire default sink).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::process::Command;

pub struct Sink {
    pub id: u32,
    pub name: String,
    pub default: bool,
}

/// Audio outputs from PipeWire, with names people recognise.
pub async fn sinks() -> Result<Vec<Sink>> {
    let out = Command::new("pw-dump")
        .output()
        .await
        .context("pw-dump not found")?;
    let nodes: Vec<Value> = serde_json::from_slice(&out.stdout).context("pw-dump output")?;
    let default = default_sink().await;
    let mut sinks: Vec<Sink> = nodes
        .iter()
        .filter_map(|o| {
            let p = &o["info"]["props"];
            if p["media.class"] != "Audio/Sink" {
                return None;
            }
            let id = o["id"].as_u64()? as u32;
            let desc = p["node.description"].as_str().unwrap_or("Speakers");
            Some(Sink {
                id,
                name: friendly_sink(desc),
                default: Some(id) == default,
            })
        })
        .collect();
    sinks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sinks)
}

/// "Navi 31 HDMI/DP Audio Digital Stereo (HDMI) [AW2726DM]" -> "AW2726DM speakers",
/// "Razer Leviathan V2 X Analog Stereo" -> "Razer Leviathan V2 X".
fn friendly_sink(desc: &str) -> String {
    if let (Some(a), Some(b)) = (desc.rfind('['), desc.rfind(']'))
        && a < b
    {
        return format!("{} speakers", &desc[a + 1..b]);
    }
    let mut name = desc.to_string();
    for junk in [" Analog Stereo", " Digital Stereo", " Output", " Sink"] {
        name = name.replace(junk, "");
    }
    name.trim().to_string()
}

async fn default_sink() -> Option<u32> {
    let out = Command::new("wpctl")
        .args(["inspect", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()?
        .strip_prefix("id ")?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

pub async fn set_sink(id: u32) -> Result<()> {
    let ok = Command::new("wpctl")
        .args(["set-default", &id.to_string()])
        .status()
        .await?
        .success();
    if !ok {
        bail!("couldn't switch the sound output");
    }
    Ok(())
}

pub struct Screen {
    /// Connector name, e.g. "DP-2".
    pub name: String,
    pub label: String,
}

/// Enabled monitors, left to right, labelled with their model when the EDID has one.
pub async fn screens() -> Result<Vec<Screen>> {
    let out = Command::new("kscreen-doctor")
        .arg("-j")
        .output()
        .await
        .context("kscreen-doctor not found")?;
    let v: Value = serde_json::from_slice(&out.stdout).context("kscreen-doctor output")?;
    let mut list: Vec<(i64, Screen)> = v["outputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["enabled"] == true && o["connected"] == true)
        .filter_map(|o| {
            let name = o["name"].as_str()?.to_string();
            let (w, h) = (o["size"]["width"].as_i64()?, o["size"]["height"].as_i64()?);
            let model = edid_model(&name).unwrap_or_else(|| name.clone());
            Some((
                o["pos"]["x"].as_i64().unwrap_or(0),
                Screen {
                    label: format!("{model} · {w}×{h}"),
                    name,
                },
            ))
        })
        .collect();
    list.sort_by_key(|(x, _)| *x);
    Ok(list.into_iter().map(|(_, s)| s).collect())
}

/// Monitor model name from the EDID "display product name" descriptor (tag 0xFC).
fn edid_model(connector: &str) -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in dir.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if !file.ends_with(&format!("-{connector}")) {
            continue;
        }
        let edid = std::fs::read(entry.path().join("edid")).ok()?;
        for off in [54, 72, 90, 108] {
            let d = edid.get(off..off + 18)?;
            if d[0] == 0 && d[1] == 0 && d[3] == 0xFC {
                let name: String = d[5..]
                    .iter()
                    .take_while(|&&b| b != b'\n')
                    .map(|&b| b as char)
                    .collect();
                let name = name.trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn pref_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("couchtv/screen"))
}

/// The screen picked on the phone, if any (otherwise the desktop decides).
pub fn saved_screen() -> Option<String> {
    let s = std::fs::read_to_string(pref_path()?).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn save_screen(name: &str) -> Result<()> {
    let path = pref_path().context("no config dir")?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, name)?;
    Ok(())
}

/// Move the TV browser's window (found by process id) to a screen and keep it full screen.
/// Wayland doesn't let apps place their own windows, so ask KWin to do it.
pub async fn move_to_screen(pid: u32, screen: &str) -> Result<()> {
    if !screen
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!("bad screen name");
    }
    let script = format!(
        r#"const out = workspace.screens.find(s => s.name === "{screen}");
if (out) for (const w of workspace.windowList()) {{
    if (w.pid !== {pid} || !w.normalWindow) continue;
    w.fullScreen = false;
    workspace.sendClientToScreen(w, out);
    w.fullScreen = true;
    workspace.activeWindow = w;
}}
"#
    );
    let dir = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    let path = dir.join("couchtv-move.js");
    tokio::fs::write(&path, script).await?;
    let plugin = "couchtv-move";
    let gdbus = |args: Vec<String>| async move {
        let out = Command::new("gdbus")
            .args(["call", "--session", "--dest", "org.kde.KWin"])
            .args(&args)
            .output()
            .await
            .context("gdbus not found")?;
        if !out.status.success() {
            bail!("KWin: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok::<String, anyhow::Error>(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let unload = || {
        gdbus(vec![
            "--object-path".into(),
            "/Scripting".into(),
            "--method".into(),
            "org.kde.kwin.Scripting.unloadScript".into(),
            plugin.into(),
        ])
    };
    let _ = unload().await;
    let loaded = gdbus(vec![
        "--object-path".into(),
        "/Scripting".into(),
        "--method".into(),
        "org.kde.kwin.Scripting.loadScript".into(),
        path.display().to_string(),
        plugin.into(),
    ])
    .await?;
    let id: String = loaded.chars().filter(char::is_ascii_digit).collect();
    gdbus(vec![
        "--object-path".into(),
        format!("/Scripting/Script{id}"),
        "--method".into(),
        "org.kde.kwin.Script.run".into(),
    ])
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = unload().await;
    Ok(())
}

pub fn to_json(sinks: &[Sink], screens: &[Screen], current_screen: Option<&str>) -> Value {
    json!({
        "t": "outputs",
        "audio": sinks.iter().map(|s| json!({ "id": s.id, "name": s.name, "on": s.default })).collect::<Vec<_>>(),
        "screens": screens.iter().map(|s| json!({
            "name": s.name,
            "label": s.label,
            "on": current_screen == Some(s.name.as_str()),
        })).collect::<Vec<_>>(),
    })
}
