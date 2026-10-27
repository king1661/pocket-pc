package dev.couchtv.remote;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.graphics.Color;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.VibrationEffect;
import android.os.Vibrator;
import android.view.KeyEvent;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.webkit.JavascriptInterface;
import android.webkit.PermissionRequest;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import android.window.OnBackInvokedDispatcher;

import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;

/**
 * Full-screen shell around the CouchTV web remote served by the PC. The remote page lives on
 * the PC, so redesigns ship without reinstalling. Native extras: hardware volume keys drive the
 * PC volume, the back gesture is TV "Back", crisp haptics, and the screen stays on.
 *
 * Pairing screens are bundled and served from https://couchtv.app/ (intercepted, never hits the
 * network) so the in-app QR scanner gets a secure context for the camera.
 */
public class MainActivity extends Activity {
    private static final String ASSET_HOST = "couchtv.app";
    private static final String START = "https://" + ASSET_HOST + "/start.html";
    private static final int CAMERA_REQUEST = 1;

    private WebView web;
    private SharedPreferences prefs;
    private Vibrator vibrator;
    private PermissionRequest pendingCamera;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        prefs = getSharedPreferences("couchtv", MODE_PRIVATE);
        vibrator = getSystemService(Vibrator.class);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        getWindow().setStatusBarColor(Color.TRANSPARENT);
        getWindow().setNavigationBarColor(Color.TRANSPARENT);

        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Color.parseColor("#06070B"));
        // Edge-to-edge: keep the remote clear of the status bar, gesture bar and keyboard.
        root.setOnApplyWindowInsetsListener((v, insets) -> {
            Insets i = insets.getInsets(WindowInsets.Type.systemBars()
                    | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime());
            v.setPadding(i.left, i.top, i.right, i.bottom);
            return WindowInsets.CONSUMED;
        });

        web = new WebView(this);
        web.setBackgroundColor(Color.parseColor("#06070B"));
        web.setOverScrollMode(WebView.OVER_SCROLL_NEVER);
        WebSettings s = web.getSettings();
        s.setJavaScriptEnabled(true);
        s.setDomStorageEnabled(true);
        s.setMediaPlaybackRequiresUserGesture(false);
        s.setCacheMode(WebSettings.LOAD_NO_CACHE);
        web.addJavascriptInterface(new Bridge(), "CouchApp");
        web.setWebChromeClient(new WebChromeClient() {
            @Override
            public void onPermissionRequest(PermissionRequest request) {
                boolean wantsCamera = false;
                for (String r : request.getResources()) {
                    if (PermissionRequest.RESOURCE_VIDEO_CAPTURE.equals(r)) wantsCamera = true;
                }
                if (!wantsCamera || !ASSET_HOST.equals(request.getOrigin().getHost())) {
                    request.deny();
                } else if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
                    request.grant(new String[] {PermissionRequest.RESOURCE_VIDEO_CAPTURE});
                } else {
                    pendingCamera = request;
                    requestPermissions(new String[] {Manifest.permission.CAMERA}, CAMERA_REQUEST);
                }
            }
        });
        web.setWebViewClient(new WebViewClient() {
            @Override
            public WebResourceResponse shouldInterceptRequest(WebView view, WebResourceRequest req) {
                Uri u = req.getUrl();
                if (!ASSET_HOST.equals(u.getHost())) return null;
                String path = u.getPath() == null || u.getPath().equals("/") ? "start.html" : u.getPath().substring(1);
                try {
                    InputStream in = getAssets().open(path);
                    String mime = path.endsWith(".js") ? "text/javascript" : path.endsWith(".html") ? "text/html" : "application/octet-stream";
                    return new WebResourceResponse(mime, "utf-8", in);
                } catch (IOException e) {
                    return new WebResourceResponse("text/plain", "utf-8", 404, "Not Found", null, null);
                }
            }

            @Override
            public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest req) {
                Uri u = req.getUrl();
                if ("couchtv".equals(u.getScheme())) {
                    pairFrom(u);
                    return true;
                }
                return false;
            }

            @Override
            public void onReceivedError(WebView view, WebResourceRequest req, WebResourceError err) {
                if (req.isForMainFrame()) view.loadUrl(START + "#offline");
            }
        });
        root.addView(web);
        setContentView(root);

        if (Build.VERSION.SDK_INT >= 33) {
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                    OnBackInvokedDispatcher.PRIORITY_DEFAULT, this::onBack);
        }
        handle(getIntent());
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] perms, int[] results) {
        if (code != CAMERA_REQUEST || pendingCamera == null) return;
        if (results.length > 0 && results[0] == PackageManager.PERMISSION_GRANTED) {
            pendingCamera.grant(new String[] {PermissionRequest.RESOURCE_VIDEO_CAPTURE});
        } else {
            pendingCamera.deny();
        }
        pendingCamera = null;
    }

    @Override
    protected void onResume() {
        super.onResume();
        // Back from the background / screen on: make the remote re-check its connection now.
        if (web != null) web.evaluateJavascript("window.couchResume && couchResume()", null);
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        handle(intent);
    }

    private void handle(Intent intent) {
        Uri data = intent == null ? null : intent.getData();
        if (data != null && "couchtv".equals(data.getScheme())) pairFrom(data);
        else load();
    }

    /** couchtv://pair?u=http://pc:7777/remote#t=TOKEN */
    private void pairFrom(Uri uri) {
        savePairing(uri.getQueryParameter("u"));
        load();
    }

    private boolean savePairing(String url) {
        if (url == null || !url.matches("^https?://[^/]+/remote#t=[\\w-]+$")) return false;
        prefs.edit().putString("url", url).putString("host", Uri.parse(url).getAuthority()).apply();
        return true;
    }

    private void load() {
        String url = prefs.getString("url", null);
        web.loadUrl(url != null ? url : START);
    }

    private boolean onRemote() {
        String u = web.getUrl();
        return u != null && u.startsWith("http") && !u.contains(ASSET_HOST);
    }

    private void onBack() {
        if (onRemote()) web.evaluateJavascript("window.couchKey && couchKey('back')", null);
        else finish();
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        onBack();
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent e) {
        int k = e.getKeyCode();
        if ((k == KeyEvent.KEYCODE_VOLUME_UP || k == KeyEvent.KEYCODE_VOLUME_DOWN) && onRemote()) {
            if (e.getAction() == KeyEvent.ACTION_DOWN) {
                String dir = k == KeyEvent.KEYCODE_VOLUME_UP ? "volup" : "voldown";
                web.evaluateJavascript("window.couchKey && couchKey('" + dir + "')", null);
            }
            return true;
        }
        return super.dispatchKeyEvent(e);
    }

    private void pairError(String msg) {
        String js = "window.onPairError && onPairError(" + JSONObject.quote(msg) + ")";
        web.post(() -> web.evaluateJavascript(js, null));
    }

    /** Called from page JavaScript as window.CouchApp.*; runs on a binder thread. */
    private class Bridge {
        @JavascriptInterface
        public void haptic() {
            if (vibrator != null) vibrator.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_TICK));
        }

        @JavascriptInterface
        public void hapticStrong() {
            if (vibrator != null) vibrator.vibrate(VibrationEffect.createPredefined(VibrationEffect.EFFECT_HEAVY_CLICK));
        }

        @JavascriptInterface
        public void pairUrl(String url) {
            if (savePairing(url)) web.post(MainActivity.this::load);
        }

        /** Exchange the 6-digit code shown on the TV for the remote URL. */
        @JavascriptInterface
        public void pairWithCode(String addr, String pin) {
            new Thread(() -> {
                String host = addr.trim().replaceFirst("^https?://", "").replaceAll("/.*$", "");
                if (!host.contains(":")) host += ":7777";
                try {
                    URL url = new URL("http://" + host + "/api/pair?pin=" + URLEncoder.encode(pin, "UTF-8"));
                    HttpURLConnection c = (HttpURLConnection) url.openConnection();
                    c.setConnectTimeout(4000);
                    c.setReadTimeout(4000);
                    int status = c.getResponseCode();
                    InputStream in = status < 400 ? c.getInputStream() : c.getErrorStream();
                    ByteArrayOutputStream buf = new ByteArrayOutputStream();
                    byte[] chunk = new byte[4096];
                    for (int n; in != null && (n = in.read(chunk)) > 0; ) buf.write(chunk, 0, n);
                    String body = buf.toString(StandardCharsets.UTF_8.name());
                    if (status == 200) {
                        String remote = new JSONObject(body).getString("url");
                        // The server reports its LAN address; trust the one the user reached it on.
                        remote = remote.replaceFirst("^http://[^/]+", "http://" + host);
                        if (savePairing(remote)) {
                            web.post(MainActivity.this::load);
                            return;
                        }
                    }
                    pairError(body.isEmpty() ? "Pairing failed (" + status + ")" : body);
                } catch (Exception e) {
                    pairError("Can't reach " + host + ". Check the address and that CouchTV is running.");
                }
            }).start();
        }

        @JavascriptInterface
        public String lastHost() {
            String h = prefs.getString("host", "");
            return h.endsWith(":7777") ? h.substring(0, h.length() - 5) : h;
        }

        @JavascriptInterface
        public void reset() {
            prefs.edit().remove("url").apply();
            web.post(() -> web.loadUrl(START));
        }

        @JavascriptInterface
        public void retry() {
            web.post(MainActivity.this::load);
        }

        @JavascriptInterface
        public boolean paired() {
            return prefs.getString("url", null) != null;
        }
    }
}
