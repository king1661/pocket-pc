//! What's on: polls the TV tab for the playing title, artwork and progress, and keeps a
//! short "continue watching" list (with a captured frame when the site has no artwork).

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Runs in the TV page. Prefers Media Session metadata, then per-site player DOM, then
/// Open Graph tags, and reads progress from the biggest <video>.
pub const PROBE: &str = r#"(() => {
  const host = location.hostname.replace(/^www\./, '');
  if (host === '127.0.0.1' || host === 'localhost') return { home: true };
  const vids = [...document.querySelectorAll('video')].filter(v => v.duration > 0);
  const v = vids.sort((a, b) => b.clientWidth * b.clientHeight - a.clientWidth * a.clientHeight)[0];
  const m = navigator.mediaSession && navigator.mediaSession.metadata;
  const meta = p => document.querySelector(`meta[property="${p}"],meta[name="${p}"]`)?.content || '';
  const text = s => { const e = document.querySelector(s); return e ? e.innerText.trim().replace(/\s+/g, ' ') : ''; };
  let title = m && m.title || '', sub = m && (m.artist || m.album) || '';
  if (!title) {
    if (host.endsWith('netflix.com')) {
      const t = document.querySelector('[data-uia="video-title"]');
      if (t) { const parts = [...t.querySelectorAll('h4,span')].map(e => e.innerText.trim()).filter(Boolean);
        title = parts[0] || t.innerText.trim(); sub = parts.slice(1).join(' · '); }
    } else if (host.endsWith('youtube.com')) {
      title = text('h1.ytd-watch-metadata, #title h1'); sub = text('#owner #channel-name a');
    } else if (host.endsWith('hulu.com')) {
      title = text('.PlayerMetadata__titleText, [data-testid="player-title"]'); sub = text('.PlayerMetadata__subTitle');
    } else if (host.endsWith('disneyplus.com')) {
      title = text('.title-field, [data-testid="title-field"]'); sub = text('.subtitle-field, [data-testid="subtitle-field"]');
    } else if (host.endsWith('paramountplus.com')) {
      title = text('.title-wrapper .title, [class*="PlayerTitle"]');
    }
  }
  if (!title) title = meta('og:title') || document.title;
  // Page titles like "Watch South Park Season 8 Episode 11: Quest for Ratings - Paramount+".
  title = title.replace(/^Watch /, '').replace(/\s+[-|–]\s+(Paramount\+|Hulu|Netflix|Disney\+|YouTube|Tubi|Pluto TV|Plex)[^]*$/i, '');
  const ep = title.match(/^(.*?),?\s+Season (\d+),? Episode (\d+):?\s*(.*)$/i);
  if (ep) { title = ep[1]; sub = sub || ['S' + ep[2], 'E' + ep[3], ep[4]].filter(Boolean).join(' · '); }
  const art = m && m.artwork && m.artwork.length ? m.artwork[m.artwork.length - 1].src : meta('og:image');
  return { home: false, site: host, url: location.href, title: title.slice(0, 120), sub: sub.slice(0, 120),
           art: art || '', t: v ? v.currentTime : 0, d: v && isFinite(v.duration) ? v.duration : 0,
           paused: v ? v.paused : true, video: !!v, w: v ? v.videoWidth : 0, h: v ? v.videoHeight : 0,
           fs: !!document.fullscreenElement };
})()"#;

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Recent {
    pub key: String,
    pub site: String,
    pub title: String,
    pub sub: String,
    pub url: String,
    /// Artwork URL from the site, or our captured frame ("/art/<key>.jpg").
    pub art: String,
    pub t: f64,
    pub d: f64,
    /// Unix seconds when last watched.
    pub at: u64,
}

fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join("couchtv")
}

pub fn art_path(key: &str) -> Option<PathBuf> {
    key.chars()
        .all(|c| c.is_ascii_alphanumeric())
        .then(|| data_dir().join("art").join(format!("{key}.jpg")))
}

pub fn load() -> Vec<Recent> {
    std::fs::read(data_dir().join("recent.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(list: &[Recent]) {
    let _ = std::fs::create_dir_all(data_dir());
    if let Ok(b) = serde_json::to_vec_pretty(list) {
        let _ = std::fs::write(data_dir().join("recent.json"), b);
    }
}

/// Same show on the same site = same entry, so episodes update in place.
pub fn key_for(site: &str, title: &str) -> String {
    let mut h = DefaultHasher::new();
    (site, title.to_lowercase()).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Something worth remembering: a real video (not a trailer or preview) with a title.
pub fn worth_keeping(np: &Value) -> bool {
    np["video"] == true
        && np["d"].as_f64().unwrap_or(0.0) > 300.0
        && np["t"].as_f64().unwrap_or(0.0) > 20.0
        && !np["title"].as_str().unwrap_or("").is_empty()
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fold the latest reading into the list (newest first, at most 12).
pub fn remember(list: &mut Vec<Recent>, np: &Value) -> String {
    let s = |k: &str| np[k].as_str().unwrap_or("").to_string();
    let key = key_for(&s("site"), &s("title"));
    let old = list
        .iter()
        .position(|r| {
            r.key == key || (r.site == s("site") && r.title.eq_ignore_ascii_case(&s("title")))
        })
        .map(|i| list.remove(i));
    let art = if !s("art").is_empty() {
        s("art")
    } else {
        old.as_ref().map(|o| o.art.clone()).unwrap_or_default()
    };
    list.insert(
        0,
        Recent {
            key: key.clone(),
            site: s("site"),
            title: s("title"),
            sub: s("sub"),
            url: s("url"),
            art,
            t: np["t"].as_f64().unwrap_or(0.0),
            d: np["d"].as_f64().unwrap_or(0.0),
            at: now(),
        },
    );
    list.truncate(12);
    key
}

/// A captured frame is useless if the player drew black (DRM) or nothing.
pub fn mostly_black(jpeg: &[u8]) -> bool {
    // Cheap check without decoding: real frames compress to much larger files.
    jpeg.len() < 12_000
}
