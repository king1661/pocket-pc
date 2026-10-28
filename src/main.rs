//! Lounge (internally "couchtv"): turns this computer into a TV. Runs Chrome full screen on a 10-foot home
//! screen and serves a phone remote over Wi-Fi. Remote presses are turned into trusted
//! input through the Chrome DevTools Protocol, so it works the same on Wayland and X11.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{
        ConnectInfo, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    process::{Child, Command},
    sync::{Mutex, broadcast},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as CdpMsg};

mod adblock;
mod outputs;
mod playing;

/// What people see. The binary, config paths and Android package keep the old
/// "couchtv" id so existing installs and paired phones keep working.
const APP_NAME: &str = "Lounge";

const COUCH_JS: &str = include_str!("../web/couch.js");
const TV_HTML: &str = include_str!("../web/tv.html");
const REMOTE_HTML: &str = include_str!("../web/remote.html");
const MANIFEST: &str = include_str!("../web/manifest.json");
const ICON: &str = include_str!("../web/icon.svg");
const SPLASH_HTML: &str = include_str!("../web/splash.html");

const SERVICES: &[(&str, &str)] = &[
    ("netflix", "https://www.netflix.com/browse"),
    ("hulu", "https://www.hulu.com/hub/home"),
    ("paramount", "https://www.paramountplus.com/home/"),
    ("disney", "https://www.disneyplus.com/home"),
    ("youtube", "https://www.youtube.com/"),
    // Free, legal, ad-supported. Useful for judging picture quality on a long stream.
    ("tubi", "https://tubitv.com/"),
    ("pluto", "https://pluto.tv/on-demand"),
    ("plex", "https://watch.plex.tv/"),
];

/// Host suffix for each service, so subdomains (watch.plex.tv) count as that app.
const SERVICE_HOSTS: &[(&str, &str)] = &[
    ("netflix.com", "netflix"),
    ("hulu.com", "hulu"),
    ("paramountplus.com", "paramount"),
    ("disneyplus.com", "disney"),
    ("youtube.com", "youtube"),
    ("tubitv.com", "tubi"),
    ("pluto.tv", "pluto"),
    ("plex.tv", "plex"),
];

fn url_host(url: &str) -> String {
    url.split('/')
        .nth(2)
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_ascii_lowercase()
}

fn service_key(host: &str) -> Option<&'static str> {
    SERVICE_HOSTS
        .iter()
        .find(|(suffix, _)| {
            host == *suffix
                || host
                    .strip_suffix(suffix)
                    .is_some_and(|rest| rest.ends_with('.'))
        })
        .map(|(_, key)| *key)
}

const HELP: &str = "\
couchtv - use this computer as a TV with your phone as the remote

USAGE: couchtv [options]

  --no-launch     start only the remote server; turn the TV on from the phone (power button)
  --port N        web/remote port (default 7777)
  --headless      run the TV browser off-screen (testing)
  --install       copy to ~/.local/bin and add a Lounge entry to the app menu
  --autostart     also start the remote server (TV off) when you log in
  -h, --help      show this help

Env: COUCHTV_BROWSER=/path/to/chrome to pick the browser.";

type CdpSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One DevTools session on the TV tab. Calls are sequential, so the next message
/// carrying our id is the answer (no domains are enabled, so no events arrive).
struct Cdp {
    ws: CdpSocket,
    id: u64,
    /// The page opened another tab; reattach to it before the next command.
    stale: bool,
}

impl Cdp {
    /// Attach to the tab `want` (the one on screen), or the first page if it's gone.
    /// Returns the session and the tab's id.
    async fn connect_page(port: u16, want: Option<&str>) -> Result<(Self, String)> {
        let (id, url) = pick_page(port, want).await?;
        let mut cdp = Self::connect(&url).await?;
        cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": COUCH_JS }),
        )
        .await?;
        // Page events let us see alert()/confirm()/"leave site?" dialogs, which would
        // otherwise freeze every command until someone clicks them with a mouse.
        cdp.call("Page.enable", json!({})).await?;
        Ok((cdp, id))
    }

    /// A second, read-only session on the visible tab (no script injection, no events),
    /// so background polling never waits behind, or holds up, remote presses.
    async fn connect_observer(port: u16, want: Option<&str>) -> Result<(Self, String)> {
        let (id, url) = pick_page(port, want).await?;
        Ok((Self::connect(&url).await?, id))
    }

    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .context("TV browser didn't accept the connection")??;
        Ok(Self {
            ws,
            id: 0,
            stale: false,
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        let id = self.id;
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        // One deadline for the whole call: page events keep arriving while we wait, so a
        // per-message timeout could wait forever for an answer that never comes.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        tokio::time::timeout_at(deadline, self.ws.send(CdpMsg::Text(msg.into())))
            .await
            .context("TV browser stopped answering")??;
        loop {
            let msg = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .context("TV browser stopped answering")?
                .context("TV browser closed the connection")??;
            let CdpMsg::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(&text)?;
            if v["method"] == "Page.windowOpen" {
                self.stale = true;
            }
            if v["method"] == "Page.javascriptDialogOpening" {
                self.id += 1;
                let dismiss = json!({ "id": self.id, "method": "Page.handleJavaScriptDialog",
                                      "params": { "accept": true } });
                self.ws
                    .send(CdpMsg::Text(dismiss.to_string().into()))
                    .await?;
                continue;
            }
            if v["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                bail!("{method}: {}", err["message"].as_str().unwrap_or("failed"));
            }
            return Ok(v["result"].clone());
        }
    }
}

/// The TV browser's tabs (pages only), newest first.
/// Install (or refresh) uBlock Origin Lite in the TV browser.
async fn load_adblock(port: u16) -> Result<()> {
    let path = adblock::ensure().await?;
    let v: Value = http_json(port, "/json/version").await?;
    let url = v["webSocketDebuggerUrl"]
        .as_str()
        .context("no browser endpoint")?;
    let mut cdp = Cdp::connect(url).await?;
    cdp.call("Extensions.loadUnpacked", json!({ "path": path }))
        .await?;
    Ok(())
}

async fn pages(port: u16) -> Result<Vec<Value>> {
    let targets: Vec<Value> = http_json(port, "/json/list").await?;
    Ok(targets
        .into_iter()
        .filter(|t| {
            t["type"] == "page" && !t["url"].as_str().unwrap_or("").starts_with("devtools://")
        })
        .collect())
}

async fn pick_page(port: u16, want: Option<&str>) -> Result<(String, String)> {
    let pages = pages(port).await?;
    let page = want
        .and_then(|w| pages.iter().find(|t| t["id"] == w))
        .or(pages.first())
        .context("TV browser has no open page")?;
    let ws = page["webSocketDebuggerUrl"]
        .as_str()
        .context("tab is already being debugged")?;
    Ok((
        page["id"].as_str().unwrap_or_default().to_string(),
        ws.to_string(),
    ))
}

/// DevTools HTTP endpoints that change tabs (Chrome requires PUT for these).
async fn devtools_put(port: u16, path: &str) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    let res = client
        .put(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await?;
    let text = res.text().await?;
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn http_json<T: serde::de::DeserializeOwned>(port: u16, path: &str) -> Result<T> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()?;
    Ok(client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await?
        .json()
        .await?)
}

struct App {
    token: String,
    port: u16,
    cdp_port: u16,
    lan_addr: String,
    remote_url: String,
    headless: bool,
    chrome: Mutex<Option<Child>>,
    cdp: Mutex<Option<Cdp>>,
    /// Where the (invisible) pointer is, and the TV page's viewport size.
    mouse: Mutex<(f64, f64)>,
    /// The trackpad's circle on the TV. Kept apart from `mouse`, which D-pad hovering moves.
    cursor: Mutex<(f64, f64)>,
    view: Mutex<(f64, f64)>,
    /// Last answer from the page script: mode, site, whether a text field is focused.
    last: Mutex<Value>,
    vol: Mutex<String>,
    phones: AtomicUsize,
    events: broadcast::Sender<String>,
    /// When the phone trackpad last moved the pointer; taps and scrolls go to the pointer
    /// only while it's in use, otherwise to the focused item / middle of the screen.
    pointer_used: Mutex<Option<Instant>>,
    /// Changes every server start; open remotes reload when it changes so they pick up new UI.
    build: u64,
    /// 6-digit pairing code shown on the TV (for phones that can't scan), and when it was made.
    pin: Mutex<(String, Instant)>,
    /// Wrong pairing codes in the current minute (brute-force guard).
    pin_fails: Mutex<(u32, Instant)>,
    /// Turning the TV off takes two presses; this is when the first one happened.
    power_armed: Mutex<Option<Instant>>,
    /// Screen picked on the phone this session.
    screen: Mutex<Option<String>>,
    /// What the TV tab is playing (see playing::PROBE); null on the home screen.
    np: Mutex<Value>,
    /// The tab on screen, which remote presses drive.
    active: Mutex<Option<String>>,
    /// The home screen's tab, and the last app's tab kept open (frozen while hidden), so
    /// going Home and back is instant and resumes where you left it.
    home_tab: Mutex<Option<String>>,
    app_tabs: Mutex<Vec<(String, String)>>,
    /// Press ids already run (the phone may resend after a reconnect).
    seen: Mutex<HashMap<String, Instant>>,
    /// "Continue watching", newest first.
    recent: Mutex<Vec<playing::Recent>>,
}

const POINTER_FRESH: Duration = Duration::from_secs(8);
const POWER_CONFIRM: Duration = Duration::from_secs(4);
const PIN_LIFETIME: Duration = Duration::from_secs(600);
/// Phones ping every second; this much silence means the phone (or the connection) is gone.
const PHONE_IDLE: Duration = Duration::from_secs(8);
/// No single remote command may hold up the ones behind it for longer than this.
const COMMAND_LIMIT: Duration = Duration::from_secs(9);
/// A press that waited this long behind a stuck command is dropped, not replayed late.
const STALE_PRESS: Duration = Duration::from_secs(5);

fn new_pin() -> String {
    use rand::Rng;
    format!("{:06}", rand::rng().random_range(0..1_000_000))
}

impl App {
    fn home_url(&self) -> String {
        format!("http://127.0.0.1:{}/tv", self.port)
    }

    async fn cdp(&self, method: &str, params: Value) -> Result<Value> {
        let mut guard = self.cdp.lock().await;
        let mut last_err = None;
        for _ in 0..2 {
            if guard.as_ref().is_some_and(|c| c.stale) {
                // The page opened a new tab (it's on screen now): drive that one.
                *guard = None;
                let known = self.known_tabs().await;
                if let Ok(pages) = pages(self.cdp_port).await
                    && let Some(new) = pages
                        .iter()
                        .filter_map(|t| t["id"].as_str())
                        .find(|id| !known.iter().any(|k| k == id))
                {
                    *self.active.lock().await = Some(new.to_string());
                }
            }
            if guard.is_none() {
                let want = self.active.lock().await.clone();
                let (cdp, id) = Cdp::connect_page(self.cdp_port, want.as_deref())
                    .await
                    .context("TV is off or still starting")?;
                *self.active.lock().await = Some(id);
                *guard = Some(cdp);
            }
            match guard.as_mut().unwrap().call(method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // The tab may have crashed or been replaced; reconnect once.
                    *guard = None;
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap())
    }

    async fn tv_running(&self) -> bool {
        http_json::<Value>(self.cdp_port, "/json/version")
            .await
            .is_ok()
    }

    async fn start_tv(&self) -> Result<()> {
        if self.tv_running().await {
            return Ok(());
        }
        let profile = dirs::data_dir()
            .context("no data dir")?
            .join(if self.headless {
                "couchtv/chrome-test"
            } else {
                "couchtv/chrome"
            });
        std::fs::create_dir_all(&profile)?;
        close_stale_browser(&profile).await;
        quiet_profile(&profile);
        let mut cmd = Command::new(find_browser()?);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--remote-debugging-port={}", self.cdp_port))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--password-store=basic",
                "--autoplay-policy=no-user-gesture-required",
                // Sideways trackpad scrolling must scroll rows, not swipe back/forward in history.
                "--disable-features=Translate,MediaRouter,TouchpadOverscrollHistoryNavigation,OverscrollHistoryNavigation",
                "--overscroll-history-navigation=0",
                // Decode video on the GPU (VA-API): smooth full-screen playback, less CPU.
                "--enable-features=AcceleratedVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxZeroCopyGL",
                "--hide-crash-restore-bubble",
                "--noerrdialogs",
                "--disable-session-crashed-bubble",
                // No remote can answer "Allow notifications / location?" bubbles: deny them.
                "--deny-permission-prompts",
                "--disable-notifications",
                "--disable-sync",
                "--disable-search-engine-choice-screen",
                // Lets us install the ad blocker over DevTools (see adblock.rs).
                "--enable-unsafe-extension-debugging",
            ]);
        if self.headless {
            cmd.args(["--headless=new", "--window-size=1920,1080"])
                .arg(self.home_url());
        } else {
            // Plain kiosk (not --app) so apps can live in their own background tabs.
            cmd.arg("--kiosk").arg(self.home_url());
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = cmd.spawn().context("could not start the browser")?;
        let pid = child.id();
        *self.chrome.lock().await = Some(child);
        *self.cdp.lock().await = None;
        self.app_tabs.lock().await.clear();
        for _ in 0..50 {
            if self.tv_running().await {
                let first = pages(self.cdp_port)
                    .await
                    .ok()
                    .and_then(|p| p.first()?["id"].as_str().map(String::from));
                *self.home_tab.lock().await = first.clone();
                *self.active.lock().await = first;
                let port = self.cdp_port;
                tokio::spawn(async move {
                    if let Err(e) = load_adblock(port).await {
                        log(&format!("ad blocker: {e:#}"));
                    }
                });
                if let (Some(pid), Some(screen), false) =
                    (pid, outputs::saved_screen(), self.headless)
                {
                    // The window appears a moment after DevTools is up; place it twice to be sure.
                    tokio::spawn(async move {
                        for wait in [700, 1800] {
                            tokio::time::sleep(Duration::from_millis(wait)).await;
                            if let Err(e) = outputs::move_to_screen(pid, &screen).await {
                                log(&format!("move to {screen}: {e:#}"));
                            }
                        }
                    });
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("browser started but never became ready")
    }

    async fn stop_tv(&self) {
        *self.cdp.lock().await = None;
        *self.active.lock().await = None;
        *self.home_tab.lock().await = None;
        self.app_tabs.lock().await.clear();
        if let Ok(v) = http_json::<Value>(self.cdp_port, "/json/version").await
            && let Some(url) = v["webSocketDebuggerUrl"].as_str()
            && let Ok(mut browser) = Cdp::connect(url).await
        {
            let _ = browser.call("Browser.close", json!({})).await;
        }
        if let Some(mut child) = self.chrome.lock().await.take() {
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }

    async fn known_tabs(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .app_tabs
            .lock()
            .await
            .iter()
            .map(|(_, id)| id.clone())
            .collect();
        ids.extend(self.home_tab.lock().await.clone());
        ids.extend(self.active.lock().await.clone());
        ids
    }

    async fn tab_alive(&self, id: &str) -> bool {
        pages(self.cdp_port)
            .await
            .is_ok_and(|p| p.iter().any(|t| t["id"] == id))
    }

    /// Put another tab on screen: pause whatever plays in the current one, bring the
    /// other to the front, and point remote presses at it.
    async fn show_tab(&self, id: &str, site: &str) -> Result<()> {
        if self.active.lock().await.as_deref() == Some(id) {
            return Ok(());
        }
        // Pause and freeze the tab going to the background so it uses no CPU while hidden.
        let _ = tokio::time::timeout(Duration::from_millis(1500), async {
            let _ = self
                .cdp("Runtime.evaluate", json!({ "expression": "document.querySelectorAll('video,audio').forEach(m => m.pause())" }))
                .await;
            let _ = self.cdp("Page.setWebLifecycleState", json!({ "state": "frozen" })).await;
        })
        .await;
        devtools_put(self.cdp_port, &format!("/json/activate/{id}")).await?;
        *self.cdp.lock().await = None;
        *self.active.lock().await = Some(id.to_string());
        let _ = self
            .cdp("Page.setWebLifecycleState", json!({ "state": "active" }))
            .await;
        *self.last.lock().await = json!({ "mode": "nav", "site": site, "input": false });
        Ok(())
    }

    /// Open an app: switch to its tab if it's still open (instant, resumes where you left
    /// it), else open a new tab with a loading screen and load it there. `deep` is a link
    /// inside the app (e.g. a show to resume).
    async fn open_app(&self, key: &str, deep: Option<&str>) -> Result<()> {
        let home = SERVICES
            .iter()
            .find(|(id, _)| *id == key)
            .context("unknown service")?
            .1;
        let site = home
            .split('/')
            .nth(2)
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_string();
        self.start_tv().await?;
        let existing = self
            .app_tabs
            .lock()
            .await
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, id)| id.clone());
        if let Some(id) = existing {
            if self.tab_alive(&id).await {
                let already_here = self.active.lock().await.as_deref() == Some(id.as_str());
                self.show_tab(&id, &site).await?;
                let mut tabs = self.app_tabs.lock().await;
                tabs.retain(|(k, _)| k != key);
                tabs.push((key.to_string(), id));
                drop(tabs);
                // Pressing the app you're already in takes you to its start page.
                if let Some(url) = deep.or(already_here.then_some(home)) {
                    self.navigate(url).await?;
                }
                return Ok(());
            }
            self.app_tabs.lock().await.retain(|(k, _)| k != key);
        }
        let splash = format!("http://127.0.0.1:{}/splash?s={key}", self.port);
        let tab = devtools_put(self.cdp_port, &format!("/json/new?{splash}")).await?;
        let id = tab["id"]
            .as_str()
            .context("couldn't open a tab")?
            .to_string();
        self.show_tab(&id, &site).await?;
        let evict = {
            let mut tabs = self.app_tabs.lock().await;
            tabs.push((key.to_string(), id));
            // Keep only the latest app open (instant Home -> back, little memory): opening a
            // different app closes the previous one.
            if tabs.len() > 1 {
                Some(tabs.remove(0).1)
            } else {
                None
            }
        };
        if let Some(old) = evict {
            let _ = devtools_put(self.cdp_port, &format!("/json/close/{old}")).await;
        }
        self.navigate(deep.unwrap_or(home)).await
    }

    async fn go_home(&self) -> Result<()> {
        self.start_tv().await?;
        let home = self.home_tab.lock().await.clone();
        if let Some(id) = home
            && self.tab_alive(&id).await
        {
            return self.show_tab(&id, "home").await;
        }
        // The home tab is gone (e.g. it was navigated away): make the current tab home.
        let cur = self.active.lock().await.clone();
        *self.home_tab.lock().await = cur.clone();
        self.app_tabs
            .lock()
            .await
            .retain(|(_, id)| Some(id) != cur.as_ref());
        self.navigate(&self.home_url()).await
    }

    /// A streaming link from the TV home screen or Continue watching: open it in its app's tab.
    async fn open_link(&self, url: &str) -> Result<()> {
        let Some(key) = service_key(&url_host(url)).filter(|_| url.starts_with("https://")) else {
            bail!("not a streaming service link");
        };
        let is_home = SERVICES.iter().any(|(k, u)| *k == key && *u == url);
        self.open_app(key, (!is_home).then_some(url)).await
    }

    async fn navigate(&self, url: &str) -> Result<()> {
        self.start_tv().await?;
        self.cdp("Page.navigate", json!({ "url": url })).await?;
        let site = if url == self.home_url() {
            "home".to_string()
        } else {
            url.split('/')
                .nth(2)
                .unwrap_or("")
                .trim_start_matches("www.")
                .to_string()
        };
        *self.last.lock().await = json!({ "mode": "nav", "site": site, "input": false });
        Ok(())
    }

    /// Run a D-pad/media command through the page script, then send the trusted
    /// input it asks for.
    async fn couch(&self, k: &str) -> Result<()> {
        let mut r = self.page_cmd(k).await?;
        if r["act"] == "wake" {
            // Player controls only show after pointer movement; reveal them, then navigate.
            let (w, h) = *self.view.lock().await;
            self.mouse_to(w / 2.0, h / 2.0).await?;
            tokio::time::sleep(Duration::from_millis(350)).await;
            r = self.page_cmd(k).await?;
        }
        let num = |key: &str| r[key].as_f64().unwrap_or(0.0);
        match r["act"].as_str().unwrap_or("") {
            "key" => self.press(r["key"].as_str().unwrap_or("")).await?,
            "click" => self.click(num("x"), num("y")).await?,
            "hover" => self.mouse_to(num("x"), num("y")).await?,
            "wheel" => self.wheel(num("x"), num("y"), 0.0, num("dy")).await?,
            "park" => {
                let (w, h) = *self.view.lock().await;
                self.mouse_to(w - 2.0, h - 2.0).await?;
            }
            _ => {}
        }
        *self.last.lock().await = r;
        Ok(())
    }

    async fn page_cmd(&self, k: &str) -> Result<Value> {
        let expr = format!("{COUCH_JS}\n;window.__couch.cmd({})", json!(k));
        let res = self
            .cdp(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true, "userGesture": true }),
            )
            .await?;
        if let Some(ex) = res.get("exceptionDetails") {
            bail!(
                "page script: {}",
                ex["exception"]["description"].as_str().unwrap_or("error")
            );
        }
        let v = res["result"]["value"].clone();
        if let (Some(w), Some(h)) = (v["vw"].as_f64(), v["vh"].as_f64()) {
            *self.view.lock().await = (w, h);
        }
        Ok(v)
    }

    async fn press(&self, name: &str) -> Result<()> {
        let (key, code, vk, text) = match name {
            "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, None),
            "ArrowUp" => ("ArrowUp", "ArrowUp", 38, None),
            "ArrowRight" => ("ArrowRight", "ArrowRight", 39, None),
            "ArrowDown" => ("ArrowDown", "ArrowDown", 40, None),
            "Enter" => ("Enter", "Enter", 13, Some("\r")),
            "Escape" => ("Escape", "Escape", 27, None),
            "Backspace" => ("Backspace", "Backspace", 8, None),
            "Tab" => ("Tab", "Tab", 9, None),
            "Space" => (" ", "Space", 32, Some(" ")),
            "f" => ("f", "KeyF", 70, Some("f")),
            "m" => ("m", "KeyM", 77, Some("m")),
            _ => bail!("unknown key {name}"),
        };
        let mut down = json!({
            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key, "code": code,
            "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
        });
        if let Some(t) = text {
            down["text"] = json!(t);
        }
        self.cdp("Input.dispatchKeyEvent", down).await?;
        self.cdp(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": code,
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
        )
        .await?;
        Ok(())
    }

    async fn mouse_to(&self, x: f64, y: f64) -> Result<()> {
        *self.mouse.lock().await = (x, y);
        self.cdp(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y }),
        )
        .await?;
        Ok(())
    }

    async fn click(&self, x: f64, y: f64) -> Result<()> {
        self.mouse_to(x, y).await?;
        for kind in ["mousePressed", "mouseReleased"] {
            self.cdp(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            )
            .await?;
        }
        Ok(())
    }

    async fn wheel(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        self.cdp(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy }),
        )
        .await?;
        Ok(())
    }

    async fn current_pin(&self) -> String {
        let mut pin = self.pin.lock().await;
        if pin.1.elapsed() > PIN_LIFETIME {
            *pin = (new_pin(), Instant::now());
        }
        pin.0.clone()
    }

    fn pointer_fresh(&self, used: Option<Instant>) -> bool {
        used.is_some_and(|t| t.elapsed() < POINTER_FRESH)
    }

    /// First press arms (and says so on the TV), a second press within a few seconds
    /// turns the TV off. Turning on is a single press.
    async fn power(&self) -> Result<()> {
        if !self.tv_running().await {
            *self.power_armed.lock().await = None;
            return self.start_tv().await;
        }
        let mut armed = self.power_armed.lock().await;
        if armed.is_some_and(|t| t.elapsed() < POWER_CONFIRM) {
            *armed = None;
            drop(armed);
            self.stop_tv().await;
        } else {
            *armed = Some(Instant::now());
            drop(armed);
            let _ = self.toast("Press power again to turn off").await;
        }
        Ok(())
    }

    async fn toast(&self, msg: &str) -> Result<()> {
        let expr = format!("{COUCH_JS}\n;window.__couch.toast({})", json!(msg));
        self.cdp("Runtime.evaluate", json!({ "expression": expr }))
            .await?;
        Ok(())
    }

    async fn publish(&self, err: Option<String>) {
        let last = self.last.lock().await.clone();
        let armed = self
            .power_armed
            .lock()
            .await
            .is_some_and(|t| t.elapsed() < POWER_CONFIRM);
        let msg = json!({
            "build": self.build,
            "armed": armed,
            "t": "status",
            "tv": self.tv_running().await,
            "mode": last["mode"],
            "site": last["site"],
            "input": last["input"],
            "vol": *self.vol.lock().await,
            "np": *self.np.lock().await,
            "err": err,
        });
        let _ = self.events.send(msg.to_string());
    }
}

async fn handle(app: &App, v: &Value) -> Result<()> {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let n = |k: &str| v[k].as_f64().unwrap_or(0.0);
    match s("t").as_str() {
        "nav" => app.couch(&s("k")).await?,
        "open" => app.open_app(&s("s"), None).await?,
        "home" => app.go_home().await?,
        "text" => {
            app.cdp("Input.insertText", json!({ "text": s("v") }))
                .await?;
        }
        "key" => app.press(&s("k")).await?,
        "vol" => {
            set_volume(&s("v")).await?;
            *app.vol.lock().await = get_volume().await;
        }
        "move" => {
            *app.pointer_used.lock().await = Some(Instant::now());
            let (w, h) = *app.view.lock().await;
            let (x, y) = {
                let mut c = app.cursor.lock().await;
                *c = (
                    (c.0 + n("dx")).clamp(0.0, w - 1.0),
                    (c.1 + n("dy")).clamp(0.0, h - 1.0),
                );
                *c
            };
            app.mouse_to(x, y).await?;
        }
        // Trackpad tap: click under the circle.
        "tap" | "click" => {
            *app.pointer_used.lock().await = Some(Instant::now());
            let (x, y) = *app.cursor.lock().await;
            app.click(x, y).await?;
        }
        "scroll" => {
            let used = *app.pointer_used.lock().await;
            let (x, y) = if app.pointer_fresh(used) {
                *app.cursor.lock().await
            } else {
                let (w, h) = *app.view.lock().await;
                (w / 2.0, h / 2.0)
            };
            app.wheel(x, y, n("dx"), n("dy")).await?;
        }
        "power" => app.power().await?,
        // Continue watching: reopen a remembered title (only on the known services).
        "resume" => {
            let url = app
                .recent
                .lock()
                .await
                .iter()
                .find(|r| r.key == s("key"))
                .map(|r| r.url.clone());
            let url = url.context("that title isn't in Continue watching any more")?;
            app.open_link(&url).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn set_volume(dir: &str) -> Result<()> {
    let args: &[&str] = match dir {
        "up" => &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@", "5%+"],
        "down" => &["set-volume", "@DEFAULT_AUDIO_SINK@", "5%-"],
        "mute" => &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"],
        _ => bail!("unknown volume action"),
    };
    Command::new("wpctl")
        .args(args)
        .status()
        .await
        .context("wpctl not found")?;
    Ok(())
}

/// "Volume: 0.45 [MUTED]" -> "45% muted"
async fn get_volume() -> String {
    let Ok(out) = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .await
    else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let pct = text
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_default();
    if text.contains("MUTED") {
        format!("{pct} muted")
    } else {
        pct
    }
}

/// Turn off the TV browser's own bubbles before it starts: "Restore pages?" after an
/// unclean exit, "Save password?", Google sign-in promos, autofill offers, and site
/// permission prompts. Chrome must not be running (it rewrites this file on exit).
fn quiet_profile(profile: &std::path::Path) {
    let path = profile.join("Default/Preferences");
    let mut prefs: Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({}));
    let set = |prefs: &mut Value, keys: &[&str], val: Value| {
        let mut cur = prefs;
        for k in &keys[..keys.len() - 1] {
            if !cur[*k].is_object() {
                cur[*k] = json!({});
            }
            cur = &mut cur[*k];
        }
        cur[keys[keys.len() - 1]] = val;
    };
    set(&mut prefs, &["profile", "exit_type"], json!("Normal"));
    set(&mut prefs, &["profile", "exited_cleanly"], json!(true));
    set(&mut prefs, &["credentials_enable_service"], json!(false));
    set(
        &mut prefs,
        &["profile", "password_manager_enabled"],
        json!(false),
    );
    set(&mut prefs, &["autofill", "profile_enabled"], json!(false));
    set(
        &mut prefs,
        &["autofill", "credit_card_enabled"],
        json!(false),
    );
    set(&mut prefs, &["signin", "allowed"], json!(false));
    set(
        &mut prefs,
        &["browser", "check_default_browser"],
        json!(false),
    );
    set(&mut prefs, &["translate", "enabled"], json!(false));
    set(
        &mut prefs,
        &["download", "prompt_for_download"],
        json!(false),
    );
    for perm in [
        "notifications",
        "geolocation",
        "media_stream_camera",
        "media_stream_mic",
        "local_network_access",
        "clipboard",
    ] {
        set(
            &mut prefs,
            &["profile", "default_content_setting_values", perm],
            json!(2),
        );
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Ok(b) = serde_json::to_vec(&prefs) {
        let _ = std::fs::write(&path, b);
    }
}

/// A browser left over from an earlier run (e.g. CouchTV was killed) still owns the
/// profile; a new launch would just hand off to it and exit. Close it first.
async fn close_stale_browser(profile: &std::path::Path) {
    let Ok(target) = std::fs::read_link(profile.join("SingletonLock")) else {
        return;
    };
    let Some(pid) = target
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|p| p.parse::<u32>().ok())
    else {
        return;
    };
    let alive = || std::path::Path::new(&format!("/proc/{pid}")).exists();
    let cmdline = std::fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if !alive() || !cmdline.contains(&*profile.to_string_lossy()) {
        return;
    }
    let _ = Command::new("kill").arg(pid.to_string()).status().await;
    for _ in 0..30 {
        if !alive() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .await;
}

/// Launched from the app menu there is no terminal, so surface errors as a notification.
fn notify(msg: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", APP_NAME, APP_NAME, msg])
        .status();
}

/// Append to ~/.local/state/couchtv/couchtv.log (kept short) and stderr, so a freeze
/// can be looked into afterwards even when started from the app menu.
fn log(line: &str) {
    use std::io::Write;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format!(
        "{:02}:{:02}:{:02}",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
    );
    eprintln!("[{stamp} UTC] {line}");
    let Some(dir) = dirs::state_dir().map(|d| d.join("couchtv")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("couchtv.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
        let _ = std::fs::rename(&path, dir.join("couchtv.log.old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "[{stamp} UTC] {line}");
    }
}

/// Every 2 s while the TV is on: read what's playing, tell the phones when it changes,
/// and remember real viewing for Continue watching (with a captured frame if needed).
async fn watch_playing(app: Arc<App>) {
    let mut last_save = Instant::now();
    let mut last_shot: HashMap<String, Instant> = HashMap::new();
    let mut observer: Option<(Cdp, String)> = None;
    let mut polls = 0u32;
    // The page we last put in full screen. Moving to the next episode loads a new page,
    // which always drops full screen; leaving it on purpose (Back) keeps it off for that page.
    let mut fs_url = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !app.tv_running().await {
            observer = None;
            *app.np.lock().await = Value::Null;
            continue;
        }
        // Re-attach now and then in case the site opened a new tab.
        polls += 1;
        if polls % 15 == 0 {
            observer = None;
        }
        let want = app.active.lock().await.clone();
        if observer
            .as_ref()
            .is_some_and(|(_, id)| Some(id) != want.as_ref())
        {
            observer = None;
        }
        if observer.is_none() {
            observer = Cdp::connect_observer(app.cdp_port, want.as_deref())
                .await
                .ok();
        }
        let Some((obs, _)) = observer.as_mut() else {
            continue;
        };
        let Ok(res) = obs
            .call(
                "Runtime.evaluate",
                json!({ "expression": playing::PROBE, "returnByValue": true }),
            )
            .await
        else {
            observer = None;
            continue;
        };
        let v = res["result"]["value"].clone();
        let np = if v["home"] == true || v.is_null() {
            Value::Null
        } else {
            v
        };
        let changed = {
            let mut cur = app.np.lock().await;
            let differs = |k: &str| cur[k] != np[k];
            if differs("h") && np["h"].as_u64().unwrap_or(0) > 0 {
                log(&format!(
                    "playing {}x{} on {}",
                    np["w"],
                    np["h"],
                    np["site"].as_str().unwrap_or("?")
                ));
            }
            // Progress ticks every poll; only push those to phones that can show them.
            let c = differs("title")
                || differs("sub")
                || differs("art")
                || differs("paused")
                || differs("site")
                || differs("h")
                || (np["t"].as_f64().unwrap_or(0.0) - cur["t"].as_f64().unwrap_or(0.0)).abs() > 1.5;
            *cur = np.clone();
            c
        };
        if changed && app.phones.load(Ordering::Relaxed) > 0 {
            app.publish(None).await;
        }
        let url = np["url"].as_str().unwrap_or("");
        if np["video"] == true && np["paused"] == false && np["fs"] == false && !url.is_empty() && url != fs_url {
            fs_url = url.to_string();
            let ok = obs
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": "window.__couch && __couch.fullscreen()", "userGesture": true, "returnByValue": true }),
                )
                .await;
            if ok.is_err() {
                observer = None;
                continue;
            }
        }
        if !playing::worth_keeping(&np) {
            continue;
        }
        let key = playing::remember(&mut *app.recent.lock().await, &np);
        let playing_now = np["paused"] == false;
        let needs_art = np["art"].as_str().unwrap_or("").is_empty();
        if needs_art
            && playing_now
            && last_shot
                .get(&key)
                .is_none_or(|t| t.elapsed() > Duration::from_secs(90))
        {
            last_shot.insert(key.clone(), Instant::now());
            let (w, h) = *app.view.lock().await;
            let shot = obs
                .call(
                    "Page.captureScreenshot",
                    json!({ "format": "jpeg", "quality": 72,
                            "clip": { "x": 0, "y": 0, "width": w, "height": h, "scale": 0.5 } }),
                )
                .await;
            let jpeg = shot.ok().and_then(|v| {
                base64::engine::general_purpose::STANDARD
                    .decode(v["data"].as_str()?)
                    .ok()
            });
            if let (Some(jpeg), Some(path)) = (jpeg, playing::art_path(&key))
                && !playing::mostly_black(&jpeg)
            {
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                if std::fs::write(&path, jpeg).is_ok()
                    && let Some(r) = app.recent.lock().await.iter_mut().find(|r| r.key == key)
                {
                    r.art = format!("/art/{key}.jpg");
                }
            }
        }
        if last_save.elapsed() > Duration::from_secs(15) {
            last_save = Instant::now();
            playing::save(&app.recent.lock().await);
        }
    }
}

/// TV home screen tiles and Continue watching go through here so each app opens in (or
/// switches back to) its own tab. Local only.
async fn open_api(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !loopback(&addr) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let res = match (q.get("s"), q.get("url")) {
        (Some(key), _) => app.open_app(key, None).await,
        (_, Some(url)) => app.open_link(url).await,
        _ => Err(anyhow::anyhow!("nothing to open")),
    };
    app.publish(None).await;
    match res {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// Shown instantly in a new app tab while the real app loads behind it.
async fn splash(Query(q): Query<HashMap<String, String>>) -> Response {
    let key = q.get("s").map(String::as_str).unwrap_or("");
    let (name, icon) = match key {
        "netflix" => ("Netflix", "netflix"),
        "hulu" => ("Hulu", "hulu"),
        "paramount" => ("Paramount+", "paramount"),
        "disney" => ("Disney+", "disney"),
        "youtube" => ("YouTube", "youtube"),
        "tubi" => ("Tubi", "tubi"),
        "pluto" => ("Pluto TV", "pluto"),
        "plex" => ("Plex", "plex"),
        _ => ("", ""),
    };
    Html(
        SPLASH_HTML
            .replace("{{NAME}}", name)
            .replace("{{ICON}}", icon),
    )
    .into_response()
}

/// Captured frames for Continue watching. The TV reads them directly; phones pass the token.
async fn art(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::extract::Path(file): axum::extract::Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !loopback(&addr) && q.get("t") != Some(&app.token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(path) = file.strip_suffix(".jpg").and_then(playing::art_path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(path).await {
        Ok(b) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "max-age=300"),
            ],
            b,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn saver_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join("couchtv/saver")
}

/// Screensaver photos: any .jpg/.jpeg/.png/.webp dropped into ~/.local/share/couchtv/saver.
async fn saver_list() -> Response {
    let mut names: Vec<String> = std::fs::read_dir(saver_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| {
            let l = n.to_lowercase();
            [".jpg", ".jpeg", ".png", ".webp"]
                .iter()
                .any(|x| l.ends_with(x))
        })
        .collect();
    names.sort();
    let urls: Vec<String> = names.iter().map(|n| format!("/saver/{n}")).collect();
    ([(header::CACHE_CONTROL, "no-cache")], axum::Json(urls)).into_response()
}

async fn saver_photo(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    if name.contains('/') || name.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let kind = match name.rsplit('.').next().map(str::to_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    };
    match tokio::fs::read(saver_dir().join(&name)).await {
        Ok(b) => (
            [
                (header::CONTENT_TYPE, kind),
                (header::CACHE_CONTROL, "max-age=86400"),
            ],
            b,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Send the screen and sound choices to one phone.
async fn send_outputs(app: &App, to: &tokio::sync::mpsc::UnboundedSender<String>) -> Result<()> {
    let (sinks, screens) = tokio::join!(outputs::sinks(), outputs::screens());
    let current = app
        .screen
        .lock()
        .await
        .clone()
        .or_else(outputs::saved_screen);
    let msg = outputs::to_json(
        &sinks.unwrap_or_default(),
        &screens.unwrap_or_default(),
        current.as_deref(),
    );
    let _ = to.send(msg.to_string());
    Ok(())
}

async fn set_output(app: &App, kind: &str, v: &Value) -> Result<()> {
    if kind == "audio" {
        let id = v["id"].as_u64().context("which output?")? as u32;
        outputs::set_sink(id).await?;
        *app.vol.lock().await = get_volume().await;
        return Ok(());
    }
    let name = v["name"].as_str().context("which screen?")?.to_string();
    outputs::save_screen(&name)?;
    *app.screen.lock().await = Some(name.clone());
    let pid = app.chrome.lock().await.as_ref().and_then(|c| c.id());
    if let Some(pid) = pid {
        outputs::move_to_screen(pid, &name).await?;
    }
    Ok(())
}

fn find_browser() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("COUCHTV_BROWSER") {
        return Ok(p.into());
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for name in [
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "chromium",
        "brave-browser",
    ] {
        for dir in path.split(':') {
            let p = PathBuf::from(dir).join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    bail!("no Chrome/Chromium found; install Google Chrome (needed for Netflix/Disney+ DRM)")
}

fn load_token() -> Result<String> {
    let path = dirs::config_dir()
        .context("no config dir")?
        .join("couchtv/token");
    if let Ok(t) = std::fs::read_to_string(&path)
        && t.trim().len() >= 16
    {
        return Ok(t.trim().to_string());
    }
    use rand::{Rng, distr::Alphanumeric};
    let token: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, &token)?;
    Ok(token)
}

fn qr_svg(data: &str) -> String {
    use qrcode::{QrCode, render::svg};
    let svg = QrCode::new(data.as_bytes())
        .map(|c| {
            c.render::<svg::Color>()
                .quiet_zone(false)
                .min_dimensions(240, 240)
                .build()
        })
        .unwrap_or_default();
    // Drop the XML prolog so it can sit inline in HTML.
    svg.find("<svg")
        .map(|i| svg[i..].to_string())
        .unwrap_or(svg)
}

fn install(autostart: bool) -> Result<()> {
    let home = dirs::home_dir().context("no home dir")?;
    let bin = home.join(".local/bin/couchtv");
    std::fs::create_dir_all(bin.parent().unwrap())?;
    let exe = std::env::current_exe()?;
    if exe != bin {
        std::fs::copy(&exe, &bin)?;
    }
    let apps = dirs::data_dir()
        .context("no data dir")?
        .join("applications");
    std::fs::create_dir_all(&apps)?;
    std::fs::write(
        apps.join("couchtv.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName={APP_NAME}\nComment=Use this computer as a TV with your phone as the remote\n\
             Exec={} \nIcon=video-display\nCategories=AudioVideo;Video;\nTerminal=false\n",
            bin.display()
        ),
    )?;
    println!(
        "Installed {} and added {APP_NAME} to the app menu.",
        bin.display()
    );
    if autostart {
        let dir = dirs::config_dir()
            .context("no config dir")?
            .join("autostart");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("couchtv-remote.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName={APP_NAME} remote server\n\
                 Exec={} --no-launch\nTerminal=false\nX-KDE-autostart-after=panel\n",
                bin.display()
            ),
        )?;
        println!(
            "The remote server now starts at login (TV off until you press power on the phone)."
        );
    }
    Ok(())
}

// ---------- HTTP ----------

fn loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

async fn root(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Redirect {
    Redirect::temporary(if loopback(&addr) { "/tv" } else { "/remote" })
}

async fn tv_page(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    // The TV page shows the pairing QR (it contains the token), so only this computer gets it.
    if !loopback(&addr) {
        return (
            StatusCode::FORBIDDEN,
            "The TV screen is only shown on the TV computer.",
        )
            .into_response();
    }
    Html(
        TV_HTML
            .replace("{{QR}}", &qr_svg(&app.remote_url))
            .replace("{{ADDR}}", &app.lan_addr)
            .replace("{{IP}}", app.lan_addr.split(':').next().unwrap_or_default()),
    )
    .into_response()
}

async fn tv_api(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !loopback(&addr) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let armed = app
        .power_armed
        .lock()
        .await
        .is_some_and(|t| t.elapsed() < POWER_CONFIRM);
    let pin = app.current_pin().await;
    let recent = app.recent.lock().await.clone();
    axum::Json(json!({ "phones": app.phones.load(Ordering::Relaxed), "armed": armed, "pin": pin, "recent": recent }))
        .into_response()
}

/// The Power tile on the TV home screen (same two-press rule as the phone).
async fn power_api(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !loopback(&addr) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let res = app.power().await;
    app.publish(None).await;
    match res {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// Pair by typing the code shown on the TV: returns the remote URL (with token).
async fn pair_api(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    {
        let mut fails = app.pin_fails.lock().await;
        if fails.1.elapsed() > Duration::from_secs(60) {
            *fails = (0, Instant::now());
        }
        if fails.0 >= 8 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many wrong codes. Wait a minute.",
            )
                .into_response();
        }
    }
    let given: String = q
        .get("pin")
        .map(|p| p.chars().filter(char::is_ascii_digit).collect())
        .unwrap_or_default();
    if given.len() == 6 && given == app.current_pin().await {
        // One code, one phone: show a fresh code for the next pairing.
        *app.pin.lock().await = (new_pin(), Instant::now());
        return axum::Json(json!({ "url": app.remote_url })).into_response();
    }
    app.pin_fails.lock().await.0 += 1;
    (
        StatusCode::UNAUTHORIZED,
        "That code doesn't match the one on the TV.",
    )
        .into_response()
}

/// Opening CouchTV again from the app menu lands here: turn the TV on (never off).
async fn on_api(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !loopback(&addr) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let res = app.start_tv().await;
    app.publish(None).await;
    match res {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// What the TV is showing right now (handy for debugging), local only.
async fn screenshot(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !loopback(&addr) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let shot = app
        .cdp("Page.captureScreenshot", json!({ "format": "png" }))
        .await;
    match shot.ok().and_then(|v| {
        base64::engine::general_purpose::STANDARD
            .decode(v["data"].as_str()?)
            .ok()
    }) {
        Some(png) => ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "TV is off").into_response(),
    }
}

/// The Android remote app, built by `android/build.sh` into the data dir.
async fn apk() -> Response {
    let path = dirs::data_dir()
        .unwrap_or_default()
        .join("couchtv/couchtv-remote.apk");
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (
                    header::CONTENT_TYPE,
                    "application/vnd.android.package-archive",
                ),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"Lounge-Remote.apk\"",
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            "The Android app hasn't been built on this PC yet.",
        )
            .into_response(),
    }
}

/// Official app icons for the services (from their App Store listings).
async fn service_icon(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let bytes: &'static [u8] = match name.as_str() {
        "netflix.jpg" => include_bytes!("../web/icons/netflix.jpg"),
        "hulu.jpg" => include_bytes!("../web/icons/hulu.jpg"),
        "paramount.jpg" => include_bytes!("../web/icons/paramount.jpg"),
        "disney.jpg" => include_bytes!("../web/icons/disney.jpg"),
        "youtube.jpg" => include_bytes!("../web/icons/youtube.jpg"),
        "tubi.jpg" => include_bytes!("../web/icons/tubi.jpg"),
        "pluto.jpg" => include_bytes!("../web/icons/pluto.jpg"),
        "plex.jpg" => include_bytes!("../web/icons/plex.jpg"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        bytes,
    )
        .into_response()
}

/// Inter for the phone remote (the TV uses the installed copy directly).
async fn font(axum::extract::Path(weight): axum::extract::Path<String>) -> Response {
    let file = match weight.as_str() {
        "400" => "Inter-Regular.ttf",
        "500" => "Inter-Medium.ttf",
        "600" => "Inter-SemiBold.ttf",
        "700" => "Inter-Bold.ttf",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match tokio::fs::read(format!("/usr/share/fonts/rsms-inter-fonts/{file}")).await {
        Ok(b) => (
            [
                (header::CONTENT_TYPE, "font/ttf"),
                (header::CACHE_CONTROL, "max-age=604800"),
            ],
            b,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn asset(body: &'static str, kind: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn ws_upgrade(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let ok = q.get("t").is_some_and(|t| *t == app.token);
    ws.on_upgrade(move |socket| phone(app, socket, ok, peer))
}

async fn phone(app: Arc<App>, socket: WebSocket, ok: bool, peer: SocketAddr) {
    let (mut tx, mut rx) = socket.split();
    if !ok {
        let _ = tx
            .send(Message::Text(json!({ "t": "denied" }).to_string().into()))
            .await;
        return;
    }
    let n = app.phones.fetch_add(1, Ordering::Relaxed) + 1;
    log(&format!("phone {peer} connected ({n} open)"));
    let mut events = app.events.subscribe();
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let forward = tokio::spawn(async move {
        loop {
            let m = tokio::select! {
                e = events.recv() => match e {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                r = reply_rx.recv() => match r { Some(m) => m, None => break },
            };
            if tx.send(Message::Text(m.into())).await.is_err() {
                break;
            }
        }
    });

    // Read the socket on its own task so heartbeats are answered even while a slow
    // command runs, and so a burst of trackpad moves can be merged into one. A phone that
    // goes quiet (it pings every 2 s) has left without closing: drop it.
    let (work_tx, mut work) = tokio::sync::mpsc::unbounded_channel::<(Instant, Value)>();
    let replies = reply_tx.clone();
    let seen_app = app.clone();
    let reader = tokio::spawn(async move {
        let mut heard = Instant::now();
        loop {
            let msg = match tokio::time::timeout(PHONE_IDLE, rx.next()).await {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    log(&format!("phone {peer} socket error: {e}"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    log(&format!(
                        "phone {peer} went silent for {} s; dropping it",
                        PHONE_IDLE.as_secs()
                    ));
                    break;
                }
            };
            // The phone pings every second; a long gap means its messages are being held up.
            if heard.elapsed() > Duration::from_millis(2500) {
                log(&format!(
                    "phone {peer}: nothing for {} ms",
                    heard.elapsed().as_millis()
                ));
            }
            heard = Instant::now();
            let Message::Text(text) = msg else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if v["t"] == "diag" {
                log(&format!("phone {peer} reconnected because: {v}"));
                continue;
            }
            if v["t"] == "ping" {
                let _ = reply_tx.send(json!({ "t": "pong" }).to_string());
                continue;
            }
            // Presses carry an id: ack at once, and run each id only once, since the phone
            // resends unacked presses on a fresh connection when the old one died.
            if let Some(id) = v["rid"].as_str() {
                let _ = reply_tx.send(json!({ "t": "ack", "rid": id }).to_string());
                let mut seen = seen_app.seen.lock().await;
                seen.retain(|_, at| at.elapsed() < Duration::from_secs(30));
                if seen.insert(id.to_string(), Instant::now()).is_some() {
                    continue;
                }
            }
            if work_tx.send((Instant::now(), v)).is_err() {
                break;
            }
        }
    });

    *app.vol.lock().await = get_volume().await;
    app.publish(None).await;

    let mut next: Option<(Instant, Value)> = None;
    loop {
        let (at, mut v) = match next.take() {
            Some(m) => m,
            None => match work.recv().await {
                Some(m) => m,
                None => break,
            },
        };
        if v["t"] == "move" {
            while let Ok(more) = work.try_recv() {
                if more.1["t"] != "move" {
                    next = Some(more);
                    break;
                }
                let sum =
                    |k: &str| v[k].as_f64().unwrap_or(0.0) + more.1[k].as_f64().unwrap_or(0.0);
                v = json!({ "t": "move", "dx": sum("dx"), "dy": sum("dy") });
            }
        }
        let kind = v["t"].as_str().unwrap_or("?").to_string();
        let quiet = matches!(kind.as_str(), "move" | "scroll");
        // If the TV was stuck, don't replay a pile of old presses once it recovers.
        let waited = at.elapsed();
        if waited
            > if quiet {
                Duration::from_secs(1)
            } else {
                STALE_PRESS
            }
        {
            log(&format!(
                "dropped stale {kind} ({} ms old)",
                waited.as_millis()
            ));
            continue;
        }
        let started = Instant::now();
        let res = match kind.as_str() {
            "outputs" => send_outputs(&app, &replies).await,
            "audio" | "screen" => {
                let r = set_output(&app, &kind, &v).await;
                let _ = send_outputs(&app, &replies).await;
                r
            }
            _ => match tokio::time::timeout(COMMAND_LIMIT, handle(&app, &v)).await {
                Ok(r) => r,
                Err(_) => {
                    // Something (usually the TV page) hung: start from a fresh connection.
                    *app.cdp.lock().await = None;
                    Err(anyhow::anyhow!("The TV took too long to respond"))
                }
            },
        };
        let took = started.elapsed();
        if took > Duration::from_millis(800) || (res.is_err() && !quiet) {
            log(&format!(
                "{kind} took {} ms{}",
                took.as_millis(),
                res.as_ref()
                    .err()
                    .map(|e| format!(": {e:#}"))
                    .unwrap_or_default()
            ));
        }
        // Trackpad moves are high-frequency; only report their failures.
        if !quiet || res.is_err() {
            app.publish(res.err().map(|e| format!("{e:#}"))).await;
        }
    }
    reader.abort();
    forward.abort();
    let n = app.phones.fetch_sub(1, Ordering::Relaxed) - 1;
    log(&format!("phone {peer} disconnected ({n} open)"));
}

#[tokio::main]
async fn main() -> Result<()> {
    let res = run().await;
    if let Err(e) = &res {
        notify(&format!("{e:#}"));
    }
    res
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    if flag("-h") || flag("--help") {
        println!("{HELP}");
        return Ok(());
    }
    if flag("--install") || flag("--autostart") {
        return install(flag("--autostart"));
    }
    let port: u16 = match args.iter().position(|a| a == "--port") {
        Some(i) => args.get(i + 1).context("--port needs a number")?.parse()?,
        None => 7777,
    };

    let token = load_token()?;
    let ip = local_ip_address::local_ip()
        .map(|i| i.to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());
    let lan_addr = format!("{ip}:{port}");
    let remote_url = format!("http://{lan_addr}/remote#t={token}");
    let app = Arc::new(App {
        token,
        port,
        cdp_port: port + 2000,
        lan_addr,
        remote_url: remote_url.clone(),
        headless: flag("--headless"),
        chrome: Mutex::new(None),
        cdp: Mutex::new(None),
        mouse: Mutex::new((960.0, 540.0)),
        cursor: Mutex::new((960.0, 540.0)),
        view: Mutex::new((1920.0, 1080.0)),
        last: Mutex::new(json!({ "mode": "nav", "site": "home" })),
        vol: Mutex::new(String::new()),
        phones: AtomicUsize::new(0),
        pointer_used: Mutex::new(None),
        build: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        power_armed: Mutex::new(None),
        screen: Mutex::new(None),
        np: Mutex::new(Value::Null),
        seen: Mutex::new(HashMap::new()),
        active: Mutex::new(None),
        home_tab: Mutex::new(None),
        app_tabs: Mutex::new(Vec::new()),
        recent: Mutex::new(playing::load()),
        pin: Mutex::new((new_pin(), Instant::now())),
        pin_fails: Mutex::new((0, Instant::now())),
        events: broadcast::channel(64).0,
    });

    let router = Router::new()
        .route("/", get(root))
        .route("/tv", get(tv_page))
        .route("/api/tv", get(tv_api))
        .route("/api/power", axum::routing::post(power_api))
        .route("/api/on", axum::routing::post(on_api))
        .route("/api/pair", get(pair_api))
        .route("/api/screenshot", get(screenshot))
        .route(
            "/remote",
            get(|| async { asset(REMOTE_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/couch.js",
            get(|| async { asset(COUCH_JS, "text/javascript") }),
        )
        .route(
            "/manifest.json",
            get(|| async { asset(MANIFEST, "application/manifest+json") }),
        )
        .route("/icon.svg", get(|| async { asset(ICON, "image/svg+xml") }))
        .route("/couchtv.apk", get(apk))
        .route("/icons/{name}", get(service_icon))
        .route("/font/{weight}", get(font))
        .route("/art/{file}", get(art))
        .route("/api/saver", get(saver_list))
        .route("/api/open", axum::routing::post(open_api))
        .route("/splash", get(splash))
        .route("/saver/{name}", get(saver_photo))
        .route("/ws", get(ws_upgrade))
        .with_state(app.clone());
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Already running in the background (TV off or on): just ask it to show the TV.
            let ok = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/api/on"))
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if ok {
                println!("{APP_NAME} was already running; turned the TV on.");
                return Ok(());
            }
            bail!("port {port} is busy with something that isn't CouchTV")
        }
        Err(e) => return Err(e.into()),
    };

    if let Ok(code) = qrcode::QrCode::new(remote_url.as_bytes()) {
        use qrcode::render::unicode::Dense1x2;
        let qr = code
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .build();
        println!("{qr}");
    }
    println!("{APP_NAME} remote: scan the QR (here or on the TV) or open {remote_url}");

    if !flag("--no-launch") {
        app.start_tv().await?;
    }

    // Tell phones when the TV is closed or opened some other way.
    let watcher = app.clone();
    tokio::spawn(async move {
        let mut was = None;
        loop {
            let now = watcher.tv_running().await;
            if was != Some(now) {
                was = Some(now);
                watcher.publish(None).await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    // Now playing / continue watching.
    let poller = app.clone();
    tokio::spawn(async move { watch_playing(poller).await });

    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = server => r?,
        _ = tokio::signal::ctrl_c() => app.stop_tv().await,
        _ = term.recv() => app.stop_tv().await,
    }
    Ok(())
}
