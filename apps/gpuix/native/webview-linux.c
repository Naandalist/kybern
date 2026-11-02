// Linux companion of `src/native/webview.ts` (WP10, WEBVIEW-PLATFORMS.md "Linux"): `libkybern-webview.so`.
//
// The same JSON channel as `webview.m` (`kybern_web_init`, `kybern_web_command`, `kybern_web_next`), backed by WPE
// WebKit rendering offscreen. Nothing here owns a window: each document is a WebKitWebView on Kybern's own WPEDisplay,
// whose views keep the last frame in memory. GPUIX's `websurface` element (fork `custom_elements/websurface.rs`)
// paints that frame as a texture inside the element tree, so z-order, rounded clips, popups above the page, fades and
// scaling all come from GPUI's scene, and forwards pointer and key input back through `kybern_web_surface_*`.
//
// Threads: WebKit's UI-process API is single-threaded GLib. `kybern_web_init` starts one thread that iterates the
// default GMainContext and owns every WebKit object (it is WebKit's "main thread"). Commands from JS and input from
// GPUI are queued to it. Frames are copied under `frames_lock`, which the GPUI thread takes while uploading.
//
// Security (same as macOS): the wrapper page is served once from `tauri://localhost/embedded/<id>`, only it may be the
// top level and it never loads in a frame, frames stay on the ticket's origins, no popups, downloads, page dialogs or
// other schemes, an ephemeral network session, no inspector. The `kybern` bridge is reachable only from the top frame:
// its handler lives in the isolated world "KybernHost", and a top-frame-only main-world shim gives the wrapper
// `window.webkit.messageHandlers.kybern`, relaying through a DOM event the child (cross-origin, opaque origin) cannot
// dispatch on the wrapper's document. Kybern's own
// child-frame scripts run in the isolated world "KybernPreview", as with WKContentWorld.
#define _GNU_SOURCE // dladdr
#include <wpe/webkit.h>
#include <wpe/wpe-platform.h>
#include <wpe/headless/wpe-headless.h>
#include <glib/gstdio.h>
#include <zlib.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

// ── Shared with the GPUIX element (keep in step with `websurface.rs`) ─────────────────────────────────────────────

typedef struct {
  uint32_t width, height, stride; // device pixels; BGRA8 premultiplied (WPE's ARGB8888 little endian)
  const uint8_t *pixels;
  uint64_t serial;                // bumps on every new frame
  int32_t dirty_x, dirty_y, dirty_width, dirty_height; // device pixels changed since the last `unlock(consume)`
  double frame_x, frame_y, frame_width, frame_height;  // where the page sits in its slot, logical pixels (`place`)
  int32_t visible;
  const char *slot;
} KybernWebFrame;

enum { INPUT_DOWN = 1, INPUT_UP, INPUT_MOVE, INPUT_LEAVE, INPUT_SCROLL, INPUT_KEY_DOWN, INPUT_KEY_UP, INPUT_FOCUS,
       INPUT_BLUR, INPUT_SCALE, INPUT_OUTSIDE_PRESS };
typedef struct {
  int32_t kind;
  double x, y;          // logical pixels in the page (window pixels for INPUT_OUTSIDE_PRESS)
  double dx, dy;        // scroll delta in logical pixels (INPUT_SCALE: dx is the window's scale factor)
  uint32_t button;      // 1 primary, 2 middle, 3 secondary
  uint32_t press_count;
  uint32_t modifiers;   // bit 0 shift, 1 control, 2 alt, 3 super/meta
  uint32_t keyval, keycode;
  int32_t precise;      // scroll: 1 pixel deltas (touchpad), 0 wheel lines already scaled to pixels
} KybernWebInput;

enum { NOTIFY_FRAME = 1, NOTIFY_CURSOR, NOTIFY_BLUR, NOTIFY_FOCUS, NOTIFY_GONE, NOTIFY_PRESENT };
typedef void (*KybernNotify)(void *context, const char *doc, int32_t kind, const char *arg);

// ── Events to JS ───────────────────────────────────────────────────────────────────────────────────────────────────

static gboolean debug_enabled(void) { static int on = -1; if (on < 0) on = g_getenv("KYBERN_WEBVIEW_DEBUG") != NULL; return on; }
#define DEBUG(...) do { if (debug_enabled()) { fprintf(stderr, "[kybern-webview] " __VA_ARGS__); fputc('\n', stderr); } } while (0)

static void (*signal_js)(void);
static GMutex events_lock;
static GQueue events = G_QUEUE_INIT;
static char *last_event;

static void emit_raw(char *json) {
  if (strlen(json) > 65536) { g_free(json); return; }
  gboolean wake = FALSE;
  g_mutex_lock(&events_lock);
  if (events.length < 512) { wake = events.length == 0; g_queue_push_tail(&events, json); json = NULL; }
  g_mutex_unlock(&events_lock);
  g_free(json);
  if (wake && signal_js) signal_js();
}

static void json_string(GString *out, const char *value) {
  g_string_append_c(out, '"');
  for (const unsigned char *p = (const unsigned char *)(value ? value : ""); *p; p++) {
    switch (*p) {
      case '"': g_string_append(out, "\\\""); break;
      case '\\': g_string_append(out, "\\\\"); break;
      case '\n': g_string_append(out, "\\n"); break;
      case '\r': g_string_append(out, "\\r"); break;
      case '\t': g_string_append(out, "\\t"); break;
      default:
        if (*p < 0x20 || *p == '<') g_string_append_printf(out, "\\u%04x", *p);
        else g_string_append_c(out, (char)*p);
    }
  }
  g_string_append_c(out, '"');
}

static void json_number(GString *out, double value) {
  if (value == (double)(long long)value && value > -1e15 && value < 1e15) g_string_append_printf(out, "%lld", (long long)value);
  else { char buffer[G_ASCII_DTOSTR_BUF_SIZE]; g_string_append(out, g_ascii_dtostr(buffer, sizeof buffer, value)); }
}

// ── Documents ──────────────────────────────────────────────────────────────────────────────────────────────────────

typedef struct {
  char *id, *category, *wrapper_url, *wrapper_html, *slot;
  double generation;
  gboolean has_generation;
  char **allowed_origins;
  WebKitWebView *web;
  WPEView *view;
  WPEToplevel *toplevel;
  gboolean placed, last_visible, has_visibility, child_frame, focused;
  int width, height;
  double scale;
  double frame_x, frame_y, frame_width, frame_height;
  double press_x, press_y; // the last pointer press, where a context menu opens
  WebKitOptionMenu *option_menu;
  WebKitContextMenu *context_menu;
  char *cursor;
  char *nonce;             // known only to Kybern's isolated worlds: the child runs host scripts that carry it
  gboolean wrapper_served; // the wrapper is served once, to the load `create_host` starts
  guint32 buttons;         // pointer buttons held in this document
  char *navigation;        // the child's latest URL, not yet sent (`flush_navigation`)
  guint navigation_timer;
} Host;

// Larger frames are refused (16384 device pixels a side is more than any window GPUI opens).
#define MAX_FRAME_SIDE 16384

typedef struct {
  guint8 *pixels;
  int width, height, stride;
  guint64 serial;
  int dirty_x0, dirty_y0, dirty_x1, dirty_y1;
  double frame_x, frame_y, frame_width, frame_height;
  gboolean visible;
  char *slot;
} Frame;

static GHashTable *hosts;            // id → Host, WebKit thread only
static GMutex frames_lock;
static GHashTable *frames;           // id → Frame, under frames_lock
static KybernNotify notify_fn;
static void *notify_context;
static GMutex notify_lock;
static WPEDisplay *display;
static WebKitNetworkSession *session;
static WebKitWebContext *context;
static GMainContext *main_context;
static char *available_reason;

static void notify(const char *doc, int kind, const char *arg) {
  g_mutex_lock(&notify_lock);
  KybernNotify fn = notify_fn; void *ctx = notify_context;
  g_mutex_unlock(&notify_lock);
  if (fn) fn(ctx, doc, kind, arg);
}

static GString *event_begin(Host *host, const char *type) {
  GString *out = g_string_new("{\"id\":");
  json_string(out, host ? host->id : "");
  if (host && host->has_generation) { g_string_append(out, ",\"generation\":"); json_number(out, host->generation); }
  g_string_append(out, ",\"type\":");
  json_string(out, type);
  return out;
}
static void event_end(GString *out) { g_string_append_c(out, '}'); emit_raw(g_string_free(out, FALSE)); }
static void emit_simple(Host *host, const char *type) { event_end(event_begin(host, type)); }
static void emit_message(Host *host, const char *type, const char *message) {
  GString *out = event_begin(host, type);
  g_string_append(out, ",\"message\":"); json_string(out, message);
  event_end(out);
}
static void emit_url(Host *host, const char *type, const char *url) {
  GString *out = event_begin(host, type);
  g_string_append(out, ",\"url\":"); json_string(out, url);
  event_end(out);
}
static void emit_failed_id(const char *id, const char *message) {
  GString *out = g_string_new("{\"id\":"); json_string(out, id);
  g_string_append(out, ",\"type\":\"failed\",\"message\":"); json_string(out, message);
  event_end(out);
}

// ── Kybern's WPE platform: a display, toplevel and view that keep frames for GPUI ─────────────────────────────────

#define KYBERN_TYPE_DISPLAY (kybern_display_get_type())
G_DECLARE_FINAL_TYPE(KybernDisplay, kybern_display, KYBERN, DISPLAY, WPEDisplay)
struct _KybernDisplay { WPEDisplay parent; WPEKeymap *keymap; };
G_DEFINE_FINAL_TYPE(KybernDisplay, kybern_display, WPE_TYPE_DISPLAY)

#define KYBERN_TYPE_TOPLEVEL (kybern_toplevel_get_type())
G_DECLARE_FINAL_TYPE(KybernToplevel, kybern_toplevel, KYBERN, TOPLEVEL, WPEToplevel)
struct _KybernToplevel { WPEToplevel parent; };
G_DEFINE_FINAL_TYPE(KybernToplevel, kybern_toplevel, WPE_TYPE_TOPLEVEL)

#define KYBERN_TYPE_VIEW (kybern_view_get_type())
G_DECLARE_FINAL_TYPE(KybernView, kybern_view, KYBERN, VIEW, WPEView)
struct _KybernView { WPEView parent; WPEBuffer *committed, *pending; char *doc; GSource *pace; gint64 last_frame; };
G_DEFINE_FINAL_TYPE(KybernView, kybern_view, WPE_TYPE_VIEW)

static gboolean kybern_toplevel_resize(WPEToplevel *toplevel, int width, int height) {
  wpe_toplevel_resized(toplevel, width, height);
  return TRUE;
}
static void kybern_toplevel_class_init(KybernToplevelClass *klass) { WPE_TOPLEVEL_CLASS(klass)->resize = kybern_toplevel_resize; }
static void kybern_toplevel_init(KybernToplevel *toplevel) { (void)toplevel; }

static gboolean kybern_display_connect(WPEDisplay *display, GError **error) { (void)display; (void)error; return TRUE; }
static WPEView *kybern_display_create_view(WPEDisplay *display) {
  return WPE_VIEW(g_object_new(KYBERN_TYPE_VIEW, "display", display, NULL));
}
static WPEToplevel *kybern_display_create_toplevel(WPEDisplay *display, guint max_views) {
  (void)max_views;
  return WPE_TOPLEVEL(g_object_new(KYBERN_TYPE_TOPLEVEL, "display", display, "max-views", 1, NULL));
}
// No EGL display and no DRM device: WebKit renders with Skia on the CPU into SHM buffers, which GPUI uploads as a
// texture. DMA-BUF import into wgpu is the later optimization (WEBVIEW-PLATFORMS.md L-S4).
static gpointer kybern_display_get_egl_display(WPEDisplay *display, GError **error) {
  (void)display;
  g_set_error_literal(error, WPE_EGL_ERROR, WPE_EGL_ERROR_NOT_AVAILABLE, "Kybern renders on the CPU");
  return NULL;
}
static WPEKeymap *kybern_display_get_keymap(WPEDisplay *display) {
  KybernDisplay *self = KYBERN_DISPLAY(display);
  if (!self->keymap) self->keymap = wpe_keymap_xkb_new();
  return self->keymap;
}
static void kybern_display_class_init(KybernDisplayClass *klass) {
  WPEDisplayClass *display = WPE_DISPLAY_CLASS(klass);
  display->connect = kybern_display_connect;
  display->create_view = kybern_display_create_view;
  display->create_toplevel = kybern_display_create_toplevel;
  display->get_egl_display = kybern_display_get_egl_display;
  display->get_keymap = kybern_display_get_keymap;
}
static void kybern_display_init(KybernDisplay *display) { (void)display; }

static Host *host_for_view(WPEView *view) {
  const char *doc = KYBERN_VIEW(view)->doc;
  return doc ? g_hash_table_lookup(hosts, doc) : NULL;
}

static gboolean kybern_view_render_buffer(WPEView *view, WPEBuffer *buffer, const WPERectangle *damage, guint n_damage, GError **error) {
  KybernView *self = KYBERN_VIEW(view);
  GBytes *bytes = wpe_buffer_import_to_pixels(buffer, error); // transfer none
  if (!bytes) return FALSE;
  gsize size = 0;
  const guint8 *source = g_bytes_get_data(bytes, &size);
  int width = wpe_buffer_get_width(buffer), height = wpe_buffer_get_height(buffer);
  gsize stride = width > 0 && height > 0 ? size / (gsize)height : 0;
  if (WPE_IS_BUFFER_SHM(buffer)) stride = wpe_buffer_shm_get_stride(WPE_BUFFER_SHM(buffer));
  gint64 copy_started = g_get_monotonic_time();
  // The buffer comes from the web process: its size must hold every row it claims (the last one only `width * 4`).
  gboolean sane = width > 0 && height > 0 && width <= MAX_FRAME_SIDE && height <= MAX_FRAME_SIDE && stride >= (gsize)width * 4 &&
                  stride <= (gsize)G_MAXINT && size >= stride * (gsize)(height - 1) + (gsize)width * 4;
  if (!sane) DEBUG("frame refused: %dx%d stride %" G_GSIZE_FORMAT " size %" G_GSIZE_FORMAT, width, height, stride, size);
  if (self->doc && sane) {
    g_mutex_lock(&frames_lock);
    Frame *frame = g_hash_table_lookup(frames, self->doc);
    if (frame) {
      gboolean resized = frame->width != width || frame->height != height;
      if (resized) {
        g_free(frame->pixels);
        frame->pixels = g_malloc0((gsize)width * height * 4);
        frame->width = width; frame->height = height; frame->stride = width * 4;
      }
      // Damage is in logical pixels of the view; buffers are device pixels.
      double scale = height && wpe_view_get_height(view) ? (double)height / wpe_view_get_height(view) : 1;
      gboolean whole = resized || !n_damage;
      int x0 = width, y0 = height, x1 = 0, y1 = 0;
      for (guint i = 0; i < (whole ? 1 : n_damage); i++) {
        int rx0 = 0, ry0 = 0, rx1 = width, ry1 = height;
        if (!whole) { // clamped as doubles: a huge or negative rectangle never overflows the casts
          rx0 = (int)CLAMP(damage[i].x * scale, 0, width); ry0 = (int)CLAMP(damage[i].y * scale, 0, height);
          rx1 = (int)CLAMP(((double)damage[i].x + damage[i].width) * scale + 1, 0, width); ry1 = (int)CLAMP(((double)damage[i].y + damage[i].height) * scale + 1, 0, height);
        }
        if (rx1 <= rx0 || ry1 <= ry0) continue;
        for (int y = ry0; y < ry1; y++) memcpy(frame->pixels + (gsize)y * frame->stride + rx0 * 4, source + (gsize)y * stride + rx0 * 4, (gsize)(rx1 - rx0) * 4);
        x0 = MIN(x0, rx0); y0 = MIN(y0, ry0); x1 = MAX(x1, rx1); y1 = MAX(y1, ry1);
      }
      if (x1 > x0 && y1 > y0) {
        if (frame->dirty_x1 > frame->dirty_x0) {
          x0 = MIN(x0, frame->dirty_x0); y0 = MIN(y0, frame->dirty_y0); x1 = MAX(x1, frame->dirty_x1); y1 = MAX(y1, frame->dirty_y1);
        }
        frame->dirty_x0 = x0; frame->dirty_y0 = y0; frame->dirty_x1 = x1; frame->dirty_y1 = y1;
        frame->serial++;
      }
    }
    g_mutex_unlock(&frames_lock);
    notify(self->doc, NOTIFY_FRAME, NULL);
    if (debug_enabled()) {
      static gint64 window_start, copy_total; static int count;
      if (!window_start) window_start = copy_started;
      copy_total += g_get_monotonic_time() - copy_started;
      if (++count == 60) {
        DEBUG("%dx%d: %.1f frames/s, copy %.2f ms/frame", width, height, 60e6 / (double)(g_get_monotonic_time() - window_start), copy_total / 60e3);
        count = 0; copy_total = 0; window_start = g_get_monotonic_time();
      }
    }
  }
  // The pixels are copied. WebKit draws its next frame once this one is "rendered", so that is paced at 60 Hz, as
  // WPE's headless view does: handing it back at once let a small animated page render 200+ frames a second.
  g_set_object(&self->pending, buffer);
  gint64 now = g_get_monotonic_time(), next = self->last_frame + G_USEC_PER_SEC / 60;
  g_source_set_ready_time(self->pace, next <= now ? 0 : next);
  return TRUE;
}

static void kybern_view_set_cursor_from_name(WPEView *view, const char *name) {
  Host *host = host_for_view(view);
  if (!host || !g_strcmp0(host->cursor, name)) return;
  g_free(host->cursor); host->cursor = g_strdup(name);
  notify(host->id, NOTIFY_CURSOR, name);
}

static gboolean kybern_view_present(gpointer data) {
  KybernView *self = KYBERN_VIEW(data);
  g_source_set_ready_time(self->pace, -1);
  if (!self->pending) return G_SOURCE_CONTINUE;
  WPEView *view = WPE_VIEW(self);
  if (self->committed) wpe_view_buffer_released(view, self->committed);
  g_clear_object(&self->committed);
  self->committed = g_steal_pointer(&self->pending);
  self->last_frame = g_get_monotonic_time();
  wpe_view_buffer_rendered(view, self->committed);
  return G_SOURCE_CONTINUE;
}

static void kybern_view_dispose(GObject *object) {
  KybernView *self = KYBERN_VIEW(object);
  if (self->pace) { g_source_destroy(self->pace); g_clear_pointer(&self->pace, g_source_unref); }
  g_clear_object(&self->pending);
  g_clear_object(&self->committed);
  g_clear_pointer(&self->doc, g_free);
  G_OBJECT_CLASS(kybern_view_parent_class)->dispose(object);
}
static void kybern_view_class_init(KybernViewClass *klass) {
  G_OBJECT_CLASS(klass)->dispose = kybern_view_dispose;
  WPEViewClass *view = WPE_VIEW_CLASS(klass);
  view->render_buffer = kybern_view_render_buffer;
  view->set_cursor_from_name = kybern_view_set_cursor_from_name;
}
static gboolean pace_dispatch(GSource *source, GSourceFunc callback, gpointer data) { (void)source; return callback(data); }
static GSourceFuncs pace_funcs = { NULL, NULL, pace_dispatch, NULL, NULL, NULL };

static void kybern_view_init(KybernView *view) {
  view->pace = g_source_new(&pace_funcs, sizeof(GSource));
  g_source_set_name(view->pace, "Kybern web view frame pacing");
  g_source_set_callback(view->pace, kybern_view_present, view, NULL);
  g_source_set_ready_time(view->pace, -1);
  g_source_attach(view->pace, g_main_context_default());
}

// ── Scripts ───────────────────────────────────────────────────────────────────────────────────────────────────────

// Top frame, main world: the wrapper's `window.webkit.messageHandlers.kybern.postMessage` (webDocument in webview.ts).
static const char *BRIDGE_SHIM =
  "(()=>{if(window.top!==window||window.webkit)return;const post=d=>document.dispatchEvent(new CustomEvent('kybern-host-bridge',{detail:JSON.stringify(d)}));"
  "Object.defineProperty(window,'webkit',{value:Object.freeze({messageHandlers:Object.freeze({kybern:Object.freeze({postMessage:post})})}),configurable:false,writable:false})})()";
// Top frame, world "KybernHost": the only script that can reach the `kybern` handler.
static const char *BRIDGE_RELAY =
  "document.addEventListener('kybern-host-bridge',e=>{try{const d=JSON.parse(e.detail);if(d&&typeof d==='object'&&!Array.isArray(d))window.webkit.messageHandlers.kybern.postMessage(d)}catch(_){}},true)";
// Child frame, world "KybernPreview" (webview.m's `frameScript`), plus: run host scripts posted by the wrapper's
// isolated world (WPE's UI API evaluates only in the main frame), and report an Escape the page left unhandled. A host
// script must come from the wrapper and carry the document's nonce, which only the isolated worlds ever see (it is in
// this script's source and in `child_run_script`'s, never in the page's world).
static char *frame_script(const char *nonce) {
  GString *out = g_string_new("if(window.parent!==window&&window.parent===window.top){const nonce=");
  json_string(out, nonce);
  g_string_append(out, ";const h=window.webkit.messageHandlers.kybernFrame,post=d=>h.postMessage(d);"
    "const send=()=>{if(location.href!=='about:blank')post({__kybernNavigation:location.href})};addEventListener('DOMContentLoaded',send);addEventListener('popstate',send);addEventListener('hashchange',send);"
    "addEventListener('message',e=>{const d=e.data;if(e.source!==window.parent||!d||typeof d!=='object'||typeof d.__kybernRun!=='string')return;"
    "e.stopImmediatePropagation();if(d.__kybernNonce!==nonce)return;let r;try{const v=(0,eval)(d.__kybernRun);r={__kybernResult:String(d.rid||''),ok:true,value:v===undefined?null:JSON.parse(JSON.stringify(v)||'null')}}"
    "catch(x){r={__kybernResult:String(d.rid||''),ok:false,message:String(x&&x.message||x)}}if(d.rid)post(r)},true);"
    "addEventListener('keydown',e=>{if(e.key==='Escape')setTimeout(()=>{if(!e.defaultPrevented)post({__kybernEscape:true})},0)},false)}");
  return g_string_free(out, FALSE);
}
// The top frame forwards a host script to the child (world "KybernPreview" in the top frame).
static char *child_run_script(Host *host, const char *script, const char *rid) {
  GString *out = g_string_new("(()=>{const f=document.querySelector('iframe');if(!f||!f.contentWindow)return false;f.contentWindow.postMessage({__kybernRun:");
  json_string(out, script);
  g_string_append(out, ",rid:");
  json_string(out, rid ? rid : "");
  g_string_append(out, ",__kybernNonce:");
  json_string(out, host->nonce);
  g_string_append(out, "},'*');return true})()");
  return g_string_free(out, FALSE);
}
static const char *ESCAPE_SCRIPT_TOP =
  "addEventListener('keydown',e=>{if(e.key==='Escape')setTimeout(()=>{if(!e.defaultPrevented)document.dispatchEvent(new CustomEvent('kybern-host-escape'))},0)},false)";
static const char *ESCAPE_RELAY_TOP =
  "document.addEventListener('kybern-host-escape',()=>window.webkit.messageHandlers.kybernFrame.postMessage({__kybernEscape:true}),true)";

// ── Policy ────────────────────────────────────────────────────────────────────────────────────────────────────────

static gboolean allows_url(Host *host, const char *url) {
  GUri *uri = g_uri_parse(url, G_URI_FLAGS_NONE, NULL);
  if (!uri) return FALSE;
  const char *scheme = g_uri_get_scheme(uri);
  gboolean ok = FALSE;
  if ((!g_ascii_strcasecmp(scheme, "http") || !g_ascii_strcasecmp(scheme, "https")) && !g_uri_get_user(uri) && !g_uri_get_password(uri) && g_uri_get_host(uri)) {
    char *lower_scheme = g_ascii_strdown(scheme, -1), *host_name = g_ascii_strdown(g_uri_get_host(uri), -1);
    int port = g_uri_get_port(uri);
    gboolean default_port = port < 0 || (!strcmp(lower_scheme, "http") && port == 80) || (!strcmp(lower_scheme, "https") && port == 443);
    gboolean v6 = strchr(host_name, ':') != NULL && host_name[0] != '[';
    char *origin = default_port ? g_strdup_printf("%s://%s%s%s", lower_scheme, v6 ? "[" : "", host_name, v6 ? "]" : "")
                                : g_strdup_printf("%s://%s%s%s:%d", lower_scheme, v6 ? "[" : "", host_name, v6 ? "]" : "", port);
    for (char **o = host->allowed_origins; o && *o && !ok; o++) ok = !strcmp(*o, origin);
    g_free(origin); g_free(lower_scheme); g_free(host_name);
  }
  g_uri_unref(uri);
  return ok;
}

static gboolean on_decide_policy(WebKitWebView *web, WebKitPolicyDecision *decision, WebKitPolicyDecisionType type, Host *host) {
  (void)web;
  if (type == WEBKIT_POLICY_DECISION_TYPE_RESPONSE) {
    WebKitResponsePolicyDecision *response = WEBKIT_RESPONSE_POLICY_DECISION(decision);
    const char *url = webkit_uri_request_get_uri(webkit_response_policy_decision_get_request(response));
    gboolean main = webkit_response_policy_decision_is_main_frame_main_resource(response);
    DEBUG("response policy main=%d %s", main, url);
    gboolean allowed = main ? !g_strcmp0(url, host->wrapper_url) : g_str_has_prefix(url, "about:") || allows_url(host, url);
    if (allowed && !webkit_response_policy_decision_is_mime_type_supported(response)) allowed = FALSE; // downloads
    if (!allowed) emit_url(host, "externalNavigation", url);
    if (allowed) webkit_policy_decision_use(decision); else webkit_policy_decision_ignore(decision);
    return TRUE;
  }
  WebKitNavigationAction *action = webkit_navigation_policy_decision_get_navigation_action(WEBKIT_NAVIGATION_POLICY_DECISION(decision));
  const char *url = webkit_uri_request_get_uri(webkit_navigation_action_get_request(action));
  DEBUG("navigation policy %d %s", type, url);
  // Popups never open. A frame may load the wrapper (top level, checked again on the response), about:, or the
  // ticket's own origins; never another scheme.
  char *scheme = g_uri_parse_scheme(url);
  gboolean allowed = FALSE;
  if (type == WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION || !scheme) allowed = FALSE;
  else if (!g_ascii_strcasecmp(scheme, "tauri")) allowed = !g_strcmp0(url, host->wrapper_url);
  else if (!g_ascii_strcasecmp(scheme, "about")) allowed = TRUE;
  else if (!g_ascii_strcasecmp(scheme, "http") || !g_ascii_strcasecmp(scheme, "https")) allowed = allows_url(host, url);
  g_free(scheme);
  if (!allowed) emit_url(host, "externalNavigation", url);
  if (allowed) webkit_policy_decision_use(decision); else webkit_policy_decision_ignore(decision);
  return TRUE;
}

// A top-level navigation away from the wrapper is caught by the main resource response check above
// (`is_main_frame_main_resource`): only the wrapper URL may be the main frame's document, and only there. The scheme
// handler also serves it once, to the load `create_host` starts, so no frame can load a second copy.

static void on_tauri_request(WebKitURISchemeRequest *request, gpointer data) {
  (void)data;
  WebKitWebView *web = webkit_uri_scheme_request_get_web_view(request);
  const char *uri = webkit_uri_scheme_request_get_uri(request);
  DEBUG("scheme request %s", uri);
  Host *host = NULL;
  GHashTableIter iter; gpointer value;
  g_hash_table_iter_init(&iter, hosts);
  while (g_hash_table_iter_next(&iter, NULL, &value)) if (((Host *)value)->web == web) { host = value; break; }
  if (!host || g_strcmp0(uri, host->wrapper_url) || !host->wrapper_html || host->wrapper_served) {
    GError *error = g_error_new_literal(G_IO_ERROR, G_IO_ERROR_NOT_FOUND, "Not found");
    webkit_uri_scheme_request_finish_error(request, error);
    g_error_free(error);
    return;
  }
  host->wrapper_served = TRUE;
  gsize length = strlen(host->wrapper_html);
  GInputStream *stream = g_memory_input_stream_new_from_data(g_memdup2(host->wrapper_html, length), (gssize)length, g_free);
  WebKitURISchemeResponse *response = webkit_uri_scheme_response_new(stream, (gint64)length);
  webkit_uri_scheme_response_set_content_type(response, "text/html; charset=utf-8");
  webkit_uri_scheme_request_finish_with_response(request, response);
  g_object_unref(response);
  g_object_unref(stream);
}

// ── Visibility, messages, load events ─────────────────────────────────────────────────────────────────────────────

static void evaluate(Host *host, const char *script, const char *world) {
  if (host->web) webkit_web_view_evaluate_javascript(host->web, script, -1, world, NULL, NULL, NULL, NULL);
}

static void update_visibility(Host *host) {
  gboolean visible = host->placed;
  if (visible) wpe_view_map(host->view); else wpe_view_unmap(host->view);
  wpe_view_set_visible(host->view, visible);
  g_mutex_lock(&frames_lock);
  Frame *frame = g_hash_table_lookup(frames, host->id);
  if (frame) frame->visible = visible;
  g_mutex_unlock(&frames_lock);
  notify(host->id, NOTIFY_FRAME, NULL);
  if (!visible && host->focused) { host->focused = FALSE; wpe_view_focus_out(host->view); notify(host->id, NOTIFY_BLUR, NULL); }
  if (host->has_visibility && host->last_visible == visible) return;
  host->has_visibility = TRUE; host->last_visible = visible;
  GString *out = event_begin(host, "visibility");
  g_string_append_printf(out, ",\"visible\":%s", visible ? "true" : "false");
  event_end(out);
  webkit_web_view_set_is_muted(host->web, !visible);
  evaluate(host, visible ? "window.kybernVisible && window.kybernVisible(true)" : "window.kybernVisible && window.kybernVisible(false)", NULL);
}

static void release_focus(Host *host) {
  if (!host->focused) return;
  host->focused = FALSE;
  wpe_view_focus_out(host->view);
  notify(host->id, NOTIFY_BLUR, NULL);
}

static void on_message(WebKitUserContentManager *manager, JSCValue *value, Host *host) {
  (void)manager;
  if (!jsc_value_is_object(value) || jsc_value_is_array(value)) return;
  char *json = jsc_value_to_json(value, 0);
  if (!json) return;
  GString *out = event_begin(host, "message");
  g_string_append(out, ",\"data\":"); g_string_append(out, json);
  event_end(out);
  g_free(json);
}

// The child's URL changes (load, popstate, hashchange) reach JS at once, then at most one per NAVIGATION_INTERVAL_MS
// (the latest): a page looping on `location.hash` must not fill the event queue every document shares.
#define NAVIGATION_INTERVAL_MS 100
static gboolean flush_navigation(gpointer data);
static void send_navigation(Host *host, char *url) {
  emit_url(host, "navigation", url);
  g_free(url);
  host->navigation_timer = g_timeout_add(NAVIGATION_INTERVAL_MS, flush_navigation, host);
}
static gboolean flush_navigation(gpointer data) {
  Host *host = data;
  host->navigation_timer = 0;
  if (host->navigation) send_navigation(host, g_steal_pointer(&host->navigation));
  return G_SOURCE_REMOVE;
}

static void on_frame_message(WebKitUserContentManager *manager, JSCValue *value, Host *host) {
  (void)manager;
  if (!jsc_value_is_object(value) || jsc_value_is_array(value)) return;
  if (jsc_value_object_has_property(value, "__kybernEscape")) { release_focus(host); return; }
  if (jsc_value_object_has_property(value, "__kybernNavigation")) {
    host->child_frame = TRUE;
    JSCValue *url = jsc_value_object_get_property(value, "__kybernNavigation");
    char *text = jsc_value_to_string(url);
    g_object_unref(url);
    if (host->navigation_timer) { g_free(host->navigation); host->navigation = text; }
    else send_navigation(host, text);
    return;
  }
  if (jsc_value_object_has_property(value, "__kybernResult")) {
    JSCValue *rid = jsc_value_object_get_property(value, "__kybernResult"), *ok = jsc_value_object_get_property(value, "ok");
    char *rid_text = jsc_value_to_string(rid);
    GString *out = event_begin(host, "frameResult");
    g_string_append(out, ",\"rid\":"); json_string(out, rid_text);
    if (jsc_value_to_boolean(ok)) {
      JSCValue *result = jsc_value_object_get_property(value, "value");
      char *json = jsc_value_to_json(result, 0);
      g_string_append(out, ",\"ok\":true,\"value\":"); g_string_append(out, json ? json : "null");
      g_free(json); g_object_unref(result);
    } else {
      JSCValue *message = jsc_value_object_get_property(value, "message");
      char *text = jsc_value_to_string(message);
      g_string_append(out, ",\"ok\":false,\"message\":"); json_string(out, text);
      g_free(text); g_object_unref(message);
    }
    event_end(out);
    g_free(rid_text); g_object_unref(rid); g_object_unref(ok);
    return;
  }
  // A dictionary from the picker (`src/pick/pickerScript.ts`), in this isolated world: page data, bounded by the client.
  char *json = jsc_value_to_json(value, 0);
  if (!json) return;
  GString *out = event_begin(host, "frameMessage");
  g_string_append(out, ",\"data\":"); g_string_append(out, json);
  event_end(out);
  g_free(json);
}

static void on_load_changed(WebKitWebView *web, WebKitLoadEvent event, Host *host) {
  (void)web;
  DEBUG("load %d %s", event, webkit_web_view_get_uri(web));
  if (event != WEBKIT_LOAD_FINISHED) return;
  host->has_visibility = FALSE;
  update_visibility(host);
  emit_simple(host, "loaded");
}

static gboolean on_load_failed(WebKitWebView *web, WebKitLoadEvent event, const char *uri, GError *error, Host *host) {
  (void)web; (void)event; (void)uri;
  if (!g_error_matches(error, WEBKIT_NETWORK_ERROR, WEBKIT_NETWORK_ERROR_CANCELLED) && !g_error_matches(error, WEBKIT_POLICY_ERROR, WEBKIT_POLICY_ERROR_FRAME_LOAD_INTERRUPTED_BY_POLICY_CHANGE))
    emit_message(host, "failed", error->message);
  return TRUE;
}

static void on_terminated(WebKitWebView *web, WebKitWebProcessTerminationReason reason, Host *host) {
  (void)web; (void)reason;
  emit_simple(host, "terminated");
}

static WebKitWebView *on_create(WebKitWebView *web, WebKitNavigationAction *action, Host *host) {
  (void)web;
  emit_url(host, "externalNavigation", webkit_uri_request_get_uri(webkit_navigation_action_get_request(action)));
  return NULL;
}

// ── Browser UI that a toolkit would draw: Kybern draws menus (JS events `optionMenu`, `contextMenu`), no dialogs ────

static gboolean on_option_menu(WebKitWebView *web, WebKitOptionMenu *menu, WebKitRectangle *rect, Host *host) {
  (void)web;
  if (host->option_menu) webkit_option_menu_close(host->option_menu);
  g_set_object(&host->option_menu, menu);
  GString *out = event_begin(host, "optionMenu");
  g_string_append_printf(out, ",\"x\":%d,\"y\":%d,\"width\":%d,\"height\":%d,\"items\":[", rect->x, rect->y, rect->width, rect->height);
  guint n = webkit_option_menu_get_n_items(menu);
  for (guint i = 0; i < n; i++) {
    WebKitOptionMenuItem *item = webkit_option_menu_get_item(menu, i);
    if (i) g_string_append_c(out, ',');
    g_string_append(out, "{\"label\":"); json_string(out, webkit_option_menu_item_get_label(item));
    g_string_append_printf(out, ",\"enabled\":%s,\"selected\":%s,\"group\":%s,\"child\":%s}",
                           webkit_option_menu_item_is_enabled(item) ? "true" : "false", webkit_option_menu_item_is_selected(item) ? "true" : "false",
                           webkit_option_menu_item_is_group_label(item) ? "true" : "false", webkit_option_menu_item_is_group_child(item) ? "true" : "false");
  }
  g_string_append_c(out, ']');
  event_end(out);
  return TRUE;
}

// `alert`, `confirm`, `prompt` and `beforeunload` answer at once and show nothing, as in a WKWebView without a UI
// delegate (macOS) and on Windows, whose preview frames have no `allow-modals`: alert returns, confirm false, prompt
// null, and a page cannot hold its own unload. A page dialog drawn as a Kybern dialog would be a phishing surface (and a
// loop a page could keep reopening), so none is drawn; the preview frames here have no `allow-modals` either.
static gboolean on_script_dialog(WebKitWebView *web, WebKitScriptDialog *dialog, Host *host) {
  (void)web; (void)host;
  if (webkit_script_dialog_get_dialog_type(dialog) == WEBKIT_SCRIPT_DIALOG_BEFORE_UNLOAD_CONFIRM) webkit_script_dialog_confirm_set_confirmed(dialog, TRUE);
  return TRUE;
}

// Only what acts on the page's content: navigation, new windows, downloads and the inspector would act on the wrapper or
// leave the sandbox, so they are left out (as macOS visuals show no menu at all). Checked again when an item runs.
static gboolean allowed_context_action(WebKitContextMenuItem *item) {
  if (webkit_context_menu_item_get_submenu(item) || webkit_context_menu_item_is_separator(item)) return FALSE;
  switch (webkit_context_menu_item_get_stock_action(item)) {
    case WEBKIT_CONTEXT_MENU_ACTION_COPY: case WEBKIT_CONTEXT_MENU_ACTION_CUT: case WEBKIT_CONTEXT_MENU_ACTION_PASTE:
    case WEBKIT_CONTEXT_MENU_ACTION_COPY_LINK_TO_CLIPBOARD: case WEBKIT_CONTEXT_MENU_ACTION_COPY_IMAGE_TO_CLIPBOARD:
    case WEBKIT_CONTEXT_MENU_ACTION_BOLD: case WEBKIT_CONTEXT_MENU_ACTION_ITALIC: case WEBKIT_CONTEXT_MENU_ACTION_UNDERLINE:
    case WEBKIT_CONTEXT_MENU_ACTION_MEDIA_PLAY: case WEBKIT_CONTEXT_MENU_ACTION_MEDIA_PAUSE: case WEBKIT_CONTEXT_MENU_ACTION_MEDIA_MUTE:
      return TRUE;
    default:
      return FALSE;
  }
}

static gboolean on_context_menu(WebKitWebView *web, WebKitContextMenu *menu, WebKitHitTestResult *hit, Host *host) {
  (void)web; (void)hit;
  // Visuals have no context menu, as on macOS. Previews get Kybern's menu with WebKit's own items.
  if (!g_strcmp0(host->category, "visual") || !g_strcmp0(host->category, "renderer")) return TRUE;
  g_set_object(&host->context_menu, menu);
  GString *out = event_begin(host, "contextMenu");
  g_string_append(out, ",\"x\":"); json_number(out, host->press_x);
  g_string_append(out, ",\"y\":"); json_number(out, host->press_y);
  g_string_append(out, ",\"items\":[");
  guint n = webkit_context_menu_get_n_items(menu), shown = 0;
  for (guint i = 0; i < n; i++) {
    WebKitContextMenuItem *item = webkit_context_menu_get_item_at_position(menu, i);
    if (webkit_context_menu_item_get_submenu(item) || (!webkit_context_menu_item_is_separator(item) && !allowed_context_action(item))) continue;
    if (shown++) g_string_append_c(out, ',');
    if (webkit_context_menu_item_is_separator(item)) { g_string_append_printf(out, "{\"index\":%u,\"separator\":true}", i); continue; }
    GAction *action = webkit_context_menu_item_get_gaction(item);
    char *label = g_strdup(webkit_context_menu_item_get_title(item));
    if (label) { char *write = label; for (char *read = label; *read; read++) if (*read != '_') *write++ = *read; *write = 0; } // GTK mnemonics
    g_string_append_printf(out, "{\"index\":%u,\"label\":", i); json_string(out, label ? label : "");
    g_free(label);
    g_string_append_printf(out, ",\"enabled\":%s}", action && g_action_get_enabled(action) ? "true" : "false");
  }
  g_string_append_c(out, ']');
  if (shown) event_end(out); else { g_string_free(out, TRUE); g_clear_object(&host->context_menu); }
  return TRUE;
}

// ── Snapshots ─────────────────────────────────────────────────────────────────────────────────────────────────────

static void put32(guint8 *p, guint32 v) { p[0] = v >> 24; p[1] = v >> 16; p[2] = v >> 8; p[3] = v; }
static void png_chunk(GByteArray *out, const char *type, const guint8 *data, guint32 length) {
  guint8 head[8]; put32(head, length); memcpy(head + 4, type, 4);
  g_byte_array_append(out, head, 8);
  if (length) g_byte_array_append(out, data, length);
  uLong crc = crc32(0, (const Bytef *)type, 4);
  if (length) crc = crc32(crc, data, length);
  guint8 tail[4]; put32(tail, (guint32)crc);
  g_byte_array_append(out, tail, 4);
}
// Premultiplied BGRA → straight RGBA PNG, cropped to (x, y, w, h).
static gboolean write_png(const char *path, const guint8 *pixels, int stride, int x, int y, int w, int h) {
  gsize raw_size = (gsize)(w * 4 + 1) * h;
  guint8 *raw = g_malloc(raw_size);
  for (int row = 0; row < h; row++) {
    guint8 *dst = raw + (gsize)row * (w * 4 + 1);
    const guint8 *src = pixels + (gsize)(y + row) * stride + x * 4;
    *dst++ = 0;
    for (int col = 0; col < w; col++, src += 4, dst += 4) {
      guint a = src[3];
      if (a == 0) { dst[0] = dst[1] = dst[2] = dst[3] = 0; continue; }
      dst[0] = (guint8)MIN(255u, (src[2] * 255u + a / 2) / a); dst[1] = (guint8)MIN(255u, (src[1] * 255u + a / 2) / a);
      dst[2] = (guint8)MIN(255u, (src[0] * 255u + a / 2) / a); dst[3] = (guint8)a;
    }
  }
  uLongf packed_size = compressBound(raw_size);
  guint8 *packed = g_malloc(packed_size);
  gboolean ok = compress2(packed, &packed_size, raw, raw_size, 6) == Z_OK;
  if (ok) {
    GByteArray *out = g_byte_array_new();
    static const guint8 signature[8] = { 137, 80, 78, 71, 13, 10, 26, 10 };
    g_byte_array_append(out, signature, 8);
    guint8 ihdr[13]; put32(ihdr, (guint32)w); put32(ihdr + 4, (guint32)h);
    ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;
    png_chunk(out, "IHDR", ihdr, 13);
    png_chunk(out, "IDAT", packed, (guint32)packed_size);
    png_chunk(out, "IEND", NULL, 0);
    ok = g_file_set_contents(path, (const char *)out->data, out->len, NULL);
    g_byte_array_unref(out);
  }
  g_free(packed); g_free(raw);
  return ok;
}

static gboolean snapshot_to(Host *host, const char *path, double lx, double ly, double lw, double lh, int *out_w, int *out_h) {
  gboolean ok = FALSE;
  g_mutex_lock(&frames_lock);
  Frame *frame = g_hash_table_lookup(frames, host->id);
  if (frame && frame->pixels && frame->width > 0) {
    double scale = host->height > 0 ? (double)frame->height / host->height : 1;
    int x = 0, y = 0, w = frame->width, h = frame->height;
    if (lw > 0 && lh > 0) {
      x = (int)MAX(0, lx * scale); y = (int)MAX(0, ly * scale);
      w = (int)MIN(frame->width - x, lw * scale + 0.5); h = (int)MIN(frame->height - y, lh * scale + 0.5);
    }
    if (w > 0 && h > 0) {
      // Copy out so the GPUI thread is not held while encoding.
      gsize size = (gsize)frame->stride * frame->height;
      guint8 *copy = g_memdup2(frame->pixels, size);
      int stride = frame->stride;
      g_mutex_unlock(&frames_lock);
      ok = write_png(path, copy, stride, x, y, w, h);
      g_free(copy);
      *out_w = w; *out_h = h;
      return ok;
    }
  }
  g_mutex_unlock(&frames_lock);
  return ok;
}

// A snapshot is written only as a `.png` under the temp folder or Kybern's own cache (`gpuixCacheDir`: visual frames
// go to its image cache), named by a clean absolute path: no `.`/`..` components, and a root matches whole components
// (`/tmp` does not cover `/tmpx`).
static gboolean path_under(const char *path, const char *root) {
  if (!root || !g_path_is_absolute(root)) return FALSE;
  char *clean = g_canonicalize_filename(root, NULL);
  size_t n = strlen(clean);
  gboolean ok = n > 1 && !strncmp(path, clean, n) && path[n] == '/';
  g_free(clean);
  return ok;
}
static gboolean snapshot_path_allowed(const char *path) {
  if (!path || !g_path_is_absolute(path) || !g_str_has_suffix(path, ".png")) return FALSE;
  char *clean = g_canonicalize_filename(path, NULL);
  gboolean ok = !strcmp(clean, path);
  g_free(clean);
  if (!ok) return FALSE;
  char *cache = g_build_filename(g_get_user_cache_dir(), "kybern-gpuix", NULL);
  const char *config = g_getenv("KYBERN_CLIENT_CONFIG_DIR");
  ok = path_under(path, g_get_tmp_dir()) || path_under(path, cache) || (config && path_under(path, config));
  g_free(cache);
  return ok;
}

typedef struct { Host *host; char *rid, *path; double x, y, w, h; } RectJob;
static void on_rect_probe(GObject *source, GAsyncResult *result, gpointer data) {
  RectJob *job = data;
  JSCValue *box = webkit_web_view_evaluate_javascript_finish(WEBKIT_WEB_VIEW(source), result, NULL);
  double left = 0, top = 0, scale = 1;
  if (box && jsc_value_is_array(box)) {
    JSCValue *a = jsc_value_object_get_property_at_index(box, 0), *b = jsc_value_object_get_property_at_index(box, 1), *c = jsc_value_object_get_property_at_index(box, 2);
    left = jsc_value_to_double(a); top = jsc_value_to_double(b); scale = jsc_value_to_double(c);
    g_object_unref(a); g_object_unref(b); g_object_unref(c);
    if (!(scale > 0)) scale = 1;
  }
  g_clear_object(&box);
  Host *owner = NULL;
  GHashTableIter iter; gpointer value;
  g_hash_table_iter_init(&iter, hosts);
  while (g_hash_table_iter_next(&iter, NULL, &value)) if (((Host *)value)->web == WEBKIT_WEB_VIEW(source)) { owner = value; break; }
  if (owner) {
    double x = left + job->x * scale, y = top + job->y * scale, w = MAX(1, job->w * scale), h = MAX(1, job->h * scale);
    double x1 = MIN(x + w, owner->width), y1 = MIN(y + h, owner->height);
    x = MAX(0, x); y = MAX(0, y);
    GString *out = event_begin(owner, "snapshotRect");
    g_string_append(out, ",\"rid\":"); json_string(out, job->rid);
    int pw = 0, ph = 0;
    if (x1 <= x || y1 <= y) { g_string_append(out, ",\"ok\":false,\"message\":\"The element is not on screen.\""); }
    else if (!snapshot_to(owner, job->path, x, y, x1 - x, y1 - y, &pw, &ph)) { g_string_append(out, ",\"ok\":false,\"message\":\"The capture could not be saved.\""); }
    else {
      g_string_append(out, ",\"ok\":true,\"path\":"); json_string(out, job->path);
      g_string_append(out, ",\"width\":"); json_number(out, x1 - x);
      g_string_append(out, ",\"height\":"); json_number(out, y1 - y);
    }
    event_end(out);
  }
  g_free(job->rid); g_free(job->path); g_free(job);
}

// ── Lifecycle ─────────────────────────────────────────────────────────────────────────────────────────────────────

static void host_free(Host *host) {
  if (host->navigation_timer) g_source_remove(host->navigation_timer);
  // The WPEView can outlive this host (WebKit holds it until the page closes): it must not deliver frames or cursors
  // to a document created later with the same id.
  if (host->view && KYBERN_IS_VIEW(host->view)) g_clear_pointer(&KYBERN_VIEW(host->view)->doc, g_free);
  if (host->web) {
    WebKitUserContentManager *manager = webkit_web_view_get_user_content_manager(host->web);
    g_signal_handlers_disconnect_by_data(manager, host);
    g_signal_handlers_disconnect_by_data(host->web, host);
    webkit_user_content_manager_unregister_script_message_handler(manager, "kybern", "KybernHost");
    webkit_user_content_manager_unregister_script_message_handler(manager, "kybernFrame", "KybernPreview");
    webkit_web_view_stop_loading(host->web);
    webkit_web_view_try_close(host->web);
  }
  if (host->option_menu) { webkit_option_menu_close(host->option_menu); g_clear_object(&host->option_menu); }
  g_clear_object(&host->context_menu);
  g_clear_object(&host->web);
  g_clear_object(&host->toplevel);
  g_mutex_lock(&frames_lock);
  g_hash_table_remove(frames, host->id);
  g_mutex_unlock(&frames_lock);
  notify(host->id, NOTIFY_GONE, NULL);
  g_free(host->id); g_free(host->category); g_free(host->wrapper_url); g_free(host->wrapper_html); g_free(host->slot); g_free(host->cursor);
  g_free(host->nonce); g_free(host->navigation);
  g_strfreev(host->allowed_origins);
  g_free(host);
}

static void frame_free(Frame *frame) { g_free(frame->pixels); g_free(frame->slot); g_free(frame); }

// Downloads are refused, as on Windows and macOS: an `<a download>` in a server preview would otherwise write into the
// user's Downloads folder without asking. Cancelled before WebKit picks a destination.
static gboolean on_download_destination(WebKitDownload *download, char *suggested, gpointer data) {
  (void)suggested; (void)data;
  webkit_download_cancel(download);
  return TRUE;
}
static void on_download_started(WebKitNetworkSession *network, WebKitDownload *download, gpointer data) {
  (void)network; (void)data;
  WebKitURIRequest *request = webkit_download_get_request(download);
  DEBUG("download refused %s", request ? webkit_uri_request_get_uri(request) : "");
  g_signal_connect(download, "decide-destination", G_CALLBACK(on_download_destination), NULL);
  webkit_download_cancel(download);
}

static gboolean ensure_engine(void) {
  if (display) return TRUE;
  display = g_getenv("KYBERN_WEBVIEW_HEADLESS") ? wpe_display_headless_new() : WPE_DISPLAY(g_object_new(KYBERN_TYPE_DISPLAY, NULL));
  GError *error = NULL;
  if (!wpe_display_connect(display, &error)) { g_clear_error(&error); g_clear_object(&display); return FALSE; }
  session = webkit_network_session_new_ephemeral();
  g_signal_connect(session, "download-started", G_CALLBACK(on_download_started), NULL);
  return TRUE;
}

static char *string_member(JSCValue *cmd, const char *name) {
  if (!jsc_value_object_has_property(cmd, name)) return NULL;
  JSCValue *value = jsc_value_object_get_property(cmd, name);
  char *text = jsc_value_is_string(value) ? jsc_value_to_string(value) : NULL;
  g_object_unref(value);
  return text;
}
static double number_member(JSCValue *cmd, const char *name, double fallback) {
  if (!jsc_value_object_has_property(cmd, name)) return fallback;
  JSCValue *value = jsc_value_object_get_property(cmd, name);
  double number = jsc_value_is_number(value) ? jsc_value_to_double(value) : jsc_value_is_boolean(value) ? jsc_value_to_boolean(value) : fallback;
  g_object_unref(value);
  return number;
}
static gboolean bool_member(JSCValue *cmd, const char *name) {
  if (!jsc_value_object_has_property(cmd, name)) return FALSE;
  JSCValue *value = jsc_value_object_get_property(cmd, name);
  gboolean result = jsc_value_to_boolean(value);
  g_object_unref(value);
  return result;
}

// The host a Fast preview's certificate is pinned for: its one allowed origin, `https://localhost:<port>` or
// `https://127.0.0.1:<port>`. NULL for anything else, which then gets no exception at all.
static char *pinned_loopback_host(char **origins) {
  if (!origins || !origins[0] || origins[1]) return NULL;
  GUri *uri = g_uri_parse(origins[0], G_URI_FLAGS_NONE, NULL);
  if (!uri) return NULL;
  const char *scheme = g_uri_get_scheme(uri), *name = g_uri_get_host(uri);
  char *pinned = NULL;
  if (scheme && !g_ascii_strcasecmp(scheme, "https") && name && (!g_ascii_strcasecmp(name, "localhost") || !strcmp(name, "127.0.0.1")))
    pinned = g_ascii_strdown(name, -1);
  g_uri_unref(uri);
  return pinned;
}

static void create_host(JSCValue *cmd, const char *id) {
  char *category = string_member(cmd, "category");
  guint previews = 0;
  GHashTableIter iter; gpointer value;
  g_hash_table_iter_init(&iter, hosts);
  while (g_hash_table_iter_next(&iter, NULL, &value)) if (!g_strcmp0(((Host *)value)->category, "preview")) previews++;
  if (g_hash_table_contains(hosts, id) || (!g_strcmp0(category, "preview") && previews >= 2)) { emit_failed_id(id, "Close another preview and try again."); g_free(category); return; }
  if (available_reason || !ensure_engine()) { emit_failed_id(id, available_reason ? available_reason : "The embedded web view could not start."); g_free(category); return; }
  Host *host = g_new0(Host, 1);
  host->id = g_strdup(id); host->category = category;
  host->has_generation = jsc_value_object_has_property(cmd, "generation");
  host->generation = number_member(cmd, "generation", 0);
  host->wrapper_html = string_member(cmd, "html");
  host->scale = 1; host->width = 1; host->height = 1;
  GPtrArray *origins = g_ptr_array_new();
  if (jsc_value_object_has_property(cmd, "allowedOrigins")) {
    JSCValue *list = jsc_value_object_get_property(cmd, "allowedOrigins");
    if (jsc_value_is_array(list)) {
      JSCValue *length = jsc_value_object_get_property(list, "length");
      int n = jsc_value_to_int32(length); g_object_unref(length);
      for (int i = 0; i < n; i++) { JSCValue *item = jsc_value_object_get_property_at_index(list, i); if (jsc_value_is_string(item)) g_ptr_array_add(origins, jsc_value_to_string(item)); g_object_unref(item); }
    }
    g_object_unref(list);
  }
  g_ptr_array_add(origins, NULL);
  host->allowed_origins = (char **)g_ptr_array_free(origins, FALSE);
  // A Fast preview (`https://localhost`, ADE-116) presents Kybern's per-install certificate, which no trust store knows.
  // Only a preview whose one origin is HTTPS on a loopback host pins it, and only for that host name; every other TLS error
  // still fails the load. WPE accepts a certificate per host name for the whole network process, not per port, session
  // or view (a separate ephemeral session was tried and still accepted it in another preview), so once pinned, every
  // document accepts exactly this certificate on that host until the app restarts. Only Kybern's own Fast forwards hold
  // its private key (in the data folder), so what that exposes is other Kybern forwards on the same machine.
  if (!g_strcmp0(category, "preview") && jsc_value_object_has_property(cmd, "trustedCertificatePems")) {
    char *pinned_host = pinned_loopback_host(host->allowed_origins);
    JSCValue *list = jsc_value_object_get_property(cmd, "trustedCertificatePems");
    if (pinned_host && jsc_value_is_array(list)) {
      JSCValue *length = jsc_value_object_get_property(list, "length");
      int n = jsc_value_to_int32(length); g_object_unref(length);
      for (int i = 0; i < n && i < 8; i++) {
        JSCValue *item = jsc_value_object_get_property_at_index(list, i);
        if (jsc_value_is_string(item)) {
          char *pem = jsc_value_to_string(item);
          GError *error = NULL;
          GTlsCertificate *certificate = g_tls_certificate_new_from_pem(pem, -1, &error);
          if (certificate) {
            webkit_network_session_allow_tls_certificate_for_host(session, certificate, pinned_host);
            g_object_unref(certificate);
          } else g_clear_error(&error);
          g_free(pem);
        }
        g_object_unref(item);
      }
    }
    g_object_unref(list);
    g_free(pinned_host);
  }
  char *escaped = g_uri_escape_string(id, G_URI_RESERVED_CHARS_ALLOWED_IN_PATH, FALSE);
  host->wrapper_url = g_strconcat("tauri://localhost/embedded/", escaped, NULL);
  g_free(escaped);

  WebKitUserContentManager *manager = webkit_user_content_manager_new();
  g_signal_connect(manager, "script-message-received::kybern", G_CALLBACK(on_message), host);
  g_signal_connect(manager, "script-message-received::kybernFrame", G_CALLBACK(on_frame_message), host);
  webkit_user_content_manager_register_script_message_handler(manager, "kybern", "KybernHost");
  webkit_user_content_manager_register_script_message_handler(manager, "kybernFrame", "KybernPreview");
  // Top-frame scripts run only in the wrapper's document (the top frame is always the wrapper: decide-policy).
  static const char *const allow_wrapper[] = { "tauri://localhost/embedded/*", NULL };
  host->nonce = g_uuid_string_random();
  char *child_script = frame_script(host->nonce);
  WebKitUserScript *scripts[] = {
    webkit_user_script_new(BRIDGE_SHIM, WEBKIT_USER_CONTENT_INJECT_TOP_FRAME, WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START, allow_wrapper, NULL),
    webkit_user_script_new_for_world(BRIDGE_RELAY, WEBKIT_USER_CONTENT_INJECT_TOP_FRAME, WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START, "KybernHost", allow_wrapper, NULL),
    webkit_user_script_new_for_world(child_script, WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES, WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START, "KybernPreview", NULL, NULL),
    webkit_user_script_new(ESCAPE_SCRIPT_TOP, WEBKIT_USER_CONTENT_INJECT_TOP_FRAME, WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START, allow_wrapper, NULL),
    webkit_user_script_new_for_world(ESCAPE_RELAY_TOP, WEBKIT_USER_CONTENT_INJECT_TOP_FRAME, WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START, "KybernPreview", allow_wrapper, NULL),
  };
  for (guint i = 0; i < G_N_ELEMENTS(scripts); i++) { webkit_user_content_manager_add_script(manager, scripts[i]); webkit_user_script_unref(scripts[i]); }
  g_free(child_script);

  WebKitSettings *settings = webkit_settings_new();
  webkit_settings_set_enable_developer_extras(settings, FALSE);
  webkit_settings_set_javascript_can_open_windows_automatically(settings, FALSE);
  webkit_settings_set_enable_page_cache(settings, FALSE);
  host->web = WEBKIT_WEB_VIEW(g_object_new(WEBKIT_TYPE_WEB_VIEW, "display", display, "network-session", session, "web-context", context,
                                           "user-content-manager", manager, "settings", settings, NULL));
  g_object_unref(settings); g_object_unref(manager);
  host->view = webkit_web_view_get_wpe_view(host->web);
  if (KYBERN_IS_VIEW(host->view)) KYBERN_VIEW(host->view)->doc = g_strdup(id);
  host->toplevel = wpe_display_create_toplevel(display, 1);
  wpe_view_set_toplevel(host->view, host->toplevel);
  wpe_toplevel_resized(host->toplevel, 1, 1);
  wpe_view_resized(host->view, 1, 1);
  if (!g_strcmp0(category, "visual") || !g_strcmp0(category, "renderer")) {
    // A visual sits on the thread's own material, like its picture: nothing is painted behind the page.
    WebKitColor clear = { 0, 0, 0, 0 };
    webkit_web_view_set_background_color(host->web, &clear);
  }
  g_signal_connect(host->web, "decide-policy", G_CALLBACK(on_decide_policy), host);
  g_signal_connect(host->web, "load-changed", G_CALLBACK(on_load_changed), host);
  g_signal_connect(host->web, "load-failed", G_CALLBACK(on_load_failed), host);
  g_signal_connect(host->web, "web-process-terminated", G_CALLBACK(on_terminated), host);
  g_signal_connect(host->web, "create", G_CALLBACK(on_create), host);
  g_signal_connect(host->web, "show-option-menu", G_CALLBACK(on_option_menu), host);
  g_signal_connect(host->web, "script-dialog", G_CALLBACK(on_script_dialog), host);
  g_signal_connect(host->web, "context-menu", G_CALLBACK(on_context_menu), host);
  Frame *frame = g_new0(Frame, 1);
  frame->slot = g_strdup("");
  g_mutex_lock(&frames_lock);
  g_hash_table_replace(frames, g_strdup(id), frame);
  g_mutex_unlock(&frames_lock);
  g_hash_table_replace(hosts, g_strdup(id), host);
  DEBUG("load %s", host->wrapper_url);
  webkit_web_view_load_uri(host->web, host->wrapper_url);
  emit_simple(host, "created");
}

static void place_host(Host *host, JSCValue *cmd) {
  int width = (int)(number_member(cmd, "logicalWidth", 0) + 0.5), height = (int)(number_member(cmd, "logicalHeight", 0) + 0.5);
  double clip_w = number_member(cmd, "width", 0), clip_h = number_member(cmd, "height", 0);
  if (width > 0 && height > 0 && (width != host->width || height != host->height)) {
    host->width = width; host->height = height;
    wpe_toplevel_resized(host->toplevel, width, height);
    wpe_view_resized(host->view, width, height);
  }
  char *slot = string_member(cmd, "slot");
  g_mutex_lock(&frames_lock);
  Frame *frame = g_hash_table_lookup(frames, host->id);
  if (frame) {
    frame->frame_x = number_member(cmd, "frameX", 0); frame->frame_y = number_member(cmd, "frameY", 0);
    frame->frame_width = number_member(cmd, "logicalWidth", 0); frame->frame_height = number_member(cmd, "logicalHeight", 0);
    g_free(frame->slot); frame->slot = g_strdup(slot ? slot : "");
  }
  g_mutex_unlock(&frames_lock);
  g_free(host->slot); host->slot = slot;
  char *layout = string_member(cmd, "layout");
  if (layout) { evaluate(host, layout, NULL); g_free(layout); }
  host->placed = bool_member(cmd, "visible") && clip_w > 0 && clip_h > 0;
  update_visibility(host);
}

static JSCContext *parser;
static void run_command(char *json) {
  JSCValue *cmd = jsc_value_new_from_json(parser, json);
  g_free(json);
  if (!cmd || !jsc_value_is_object(cmd)) { g_clear_object(&cmd); return; }
  char *op = string_member(cmd, "op"), *id = string_member(cmd, "id");
  if (!op) goto done;
  if (!strcmp(op, "capture") || !strcmp(op, "occlusion")) goto done; // GPUI hit-tests and draws popups above the page
  if (!strcmp(op, "release")) {
    GHashTableIter iter; gpointer value;
    g_hash_table_iter_init(&iter, hosts);
    while (g_hash_table_iter_next(&iter, NULL, &value)) release_focus(value);
    goto done;
  }
  if (!strcmp(op, "create")) { if (id) create_host(cmd, id); goto done; }
  Host *host = id ? g_hash_table_lookup(hosts, id) : NULL;
  if (!host) goto done;
  if (!strcmp(op, "hide")) { host->placed = FALSE; update_visibility(host); }
  else if (!strcmp(op, "place")) place_host(host, cmd);
  else if (!strcmp(op, "destroy")) {
    char *keep = g_strdup(host->id);
    gboolean has_generation = host->has_generation; double generation = host->generation;
    g_hash_table_remove(hosts, keep);
    GString *out = g_string_new("{\"id\":"); json_string(out, keep);
    if (has_generation) { g_string_append(out, ",\"generation\":"); json_number(out, generation); }
    g_string_append(out, ",\"type\":\"destroyed\"");
    event_end(out);
    g_free(keep);
  }
  else if (!strcmp(op, "post")) { char *script = string_member(cmd, "script"); if (script) { evaluate(host, script, NULL); g_free(script); } }
  else if (!strcmp(op, "navigate")) {
    char *url = string_member(cmd, "url");
    if (url && allows_url(host, url)) {
      GString *literal = g_string_new("["); json_string(literal, url); g_string_append_c(literal, ']');
      if (host->child_frame) {
        char *inner = g_strdup_printf("{const url=(%s)[0];if(location.href===url)location.reload();else location.replace(url)}", literal->str);
        char *script = child_run_script(host, inner, NULL);
        evaluate(host, script, "KybernPreview");
        g_free(script); g_free(inner);
      } else {
        char *script = g_strdup_printf("document.querySelector('iframe').src=(%s)[0]", literal->str);
        evaluate(host, script, "KybernPreview");
        g_free(script);
      }
      g_string_free(literal, TRUE);
    }
    g_free(url);
  }
  else if (!strcmp(op, "action")) {
    char *action = string_member(cmd, "action");
    if (!g_strcmp0(action, "stop")) webkit_web_view_stop_loading(host->web);
    else if (host->child_frame && action) {
      const char *inner = !strcmp(action, "back") ? "history.back()" : !strcmp(action, "forward") ? "history.forward()" : !strcmp(action, "reload") ? "location.reload()" : NULL;
      if (inner) { char *script = child_run_script(host, inner, NULL); evaluate(host, script, "KybernPreview"); g_free(script); }
    }
    g_free(action);
  }
  else if (!strcmp(op, "probe")) {
    // Presentation (fade, scale) is GPUI's paint state on Linux; the host reports the targets as reached.
    GString *out = g_string_new("{\"id\":"); json_string(out, host->id);
    g_string_append(out, ",\"type\":\"probe\",\"data\":{\"opacity\":1,\"scale\":1,\"animating\":false}");
    event_end(out);
  }
  else if (!strcmp(op, "present")) {
    // The element that paints the page (`websurface`) runs the fade and scale in GPUI's scene.
    char *fields = jsc_value_to_json(cmd, 0);
    notify(host->id, NOTIFY_PRESENT, fields);
    g_free(fields);
  }
  else if (!strcmp(op, "snapshot")) {
    char *path = string_member(cmd, "path");
    double token = number_member(cmd, "token", 0);
    int w = 0, h = 0;
    gboolean allowed = snapshot_path_allowed(path), ok = allowed && snapshot_to(host, path, 0, 0, 0, 0, &w, &h);
    GString *out = event_begin(host, "snapshot");
    g_string_append(out, ",\"token\":"); json_number(out, token);
    g_string_append_printf(out, ",\"ok\":%s,\"path\":", ok ? "true" : "false"); json_string(out, ok ? path : "");
    g_string_append_printf(out, ",\"width\":%d,\"height\":%d,\"message\":", w, h); json_string(out, ok ? "" : allowed ? "The page has not drawn yet." : "The snapshot path is not allowed.");
    event_end(out);
    g_free(path);
  }
  else if (!strcmp(op, "frame")) {
    char *script = string_member(cmd, "script"), *rid = string_member(cmd, "rid");
    if (!script || !host->child_frame) {
      if (rid) { GString *out = event_begin(host, "frameResult"); g_string_append(out, ",\"rid\":"); json_string(out, rid); g_string_append(out, ",\"ok\":false,\"message\":\"The page has not loaded yet.\""); event_end(out); }
    } else {
      char *forward = child_run_script(host, script, rid);
      evaluate(host, forward, "KybernPreview");
      g_free(forward);
    }
    g_free(script); g_free(rid);
  }
  else if (!strcmp(op, "snapshotRect")) {
    char *rid = string_member(cmd, "rid"), *path = string_member(cmd, "path");
    if (!snapshot_path_allowed(path)) {
      GString *out = event_begin(host, "snapshotRect"); g_string_append(out, ",\"rid\":"); json_string(out, rid ? rid : "");
      g_string_append(out, ",\"ok\":false,\"message\":\"The snapshot path is not allowed.\""); event_end(out);
      g_free(rid); g_free(path);
    } else {
      RectJob *job = g_new0(RectJob, 1);
      job->rid = rid ? rid : g_strdup(""); job->path = path;
      job->x = number_member(cmd, "x", 0); job->y = number_member(cmd, "y", 0); job->w = number_member(cmd, "width", 0); job->h = number_member(cmd, "height", 0);
      webkit_web_view_evaluate_javascript(host->web, "(()=>{const f=document.querySelector('iframe');if(!f)return null;const r=f.getBoundingClientRect();return [r.left,r.top,r.width/(f.offsetWidth||r.width||1)]})()",
                                          -1, NULL, NULL, NULL, on_rect_probe, job);
    }
  }
  else if (!strcmp(op, "focus")) notify(host->id, NOTIFY_FOCUS, NULL);
  else if (!strcmp(op, "optionMenu")) {
    double index = number_member(cmd, "index", -1);
    if (host->option_menu) {
      if (index >= 0 && index < webkit_option_menu_get_n_items(host->option_menu)) webkit_option_menu_activate_item(host->option_menu, (guint)index);
      webkit_option_menu_close(host->option_menu);
      g_clear_object(&host->option_menu);
    }
  }
  else if (!strcmp(op, "contextMenu")) {
    double index = number_member(cmd, "index", -1);
    if (host->context_menu && index >= 0 && index < webkit_context_menu_get_n_items(host->context_menu)) {
      WebKitContextMenuItem *item = webkit_context_menu_get_item_at_position(host->context_menu, (guint)index);
      GAction *action = allowed_context_action(item) ? webkit_context_menu_item_get_gaction(item) : NULL;
      if (action && g_action_get_enabled(action)) g_action_activate(action, NULL);
    }
    g_clear_object(&host->context_menu);
  }
done:
  g_free(op); g_free(id);
  g_object_unref(cmd);
}

// ── Input from GPUI ───────────────────────────────────────────────────────────────────────────────────────────────

typedef struct { char *doc; KybernWebInput input; } InputJob;

static WPEModifiers modifiers_of(guint32 bits, guint32 button_state) {
  WPEModifiers m = 0;
  if (bits & 1) m |= WPE_MODIFIER_KEYBOARD_SHIFT;
  if (bits & 2) m |= WPE_MODIFIER_KEYBOARD_CONTROL;
  if (bits & 4) m |= WPE_MODIFIER_KEYBOARD_ALT;
  if (bits & 8) m |= WPE_MODIFIER_KEYBOARD_META;
  if (button_state & 1) m |= WPE_MODIFIER_POINTER_BUTTON1;
  if (button_state & 2) m |= WPE_MODIFIER_POINTER_BUTTON2;
  if (button_state & 4) m |= WPE_MODIFIER_POINTER_BUTTON3;
  return m;
}

static gboolean run_input(gpointer data) {
  InputJob *job = data;
  Host *host = g_hash_table_lookup(hosts, job->doc);
  KybernWebInput *in = &job->input;
  if (in->kind == INPUT_OUTSIDE_PRESS) {
    if (host && host->placed && !g_strcmp0(host->category, "visual")) {
      GString *out = event_begin(host, "outsidePress");
      g_string_append(out, ",\"x\":"); json_number(out, in->x);
      g_string_append(out, ",\"y\":"); json_number(out, in->y);
      event_end(out);
    }
    if (host) release_focus(host);
    goto done;
  }
  if (!host || !host->view) goto done;
  guint32 time = (guint32)(g_get_monotonic_time() / 1000);
  WPEEvent *event = NULL;
  guint32 bit = in->button == 1 ? 1 : in->button == 2 ? 2 : in->button == 3 ? 4 : 0;
  switch (in->kind) {
    case INPUT_DOWN:
      if (!host->focused) { host->focused = TRUE; wpe_view_focus_in(host->view); }
      host->buttons |= bit;
      host->press_x = in->x; host->press_y = in->y;
      event = wpe_event_pointer_button_new(WPE_EVENT_POINTER_DOWN, host->view, WPE_INPUT_SOURCE_MOUSE, time, modifiers_of(in->modifiers, host->buttons), in->button, in->x, in->y, in->press_count ? in->press_count : 1);
      break;
    case INPUT_UP:
      host->buttons &= ~bit;
      event = wpe_event_pointer_button_new(WPE_EVENT_POINTER_UP, host->view, WPE_INPUT_SOURCE_MOUSE, time, modifiers_of(in->modifiers, host->buttons), in->button, in->x, in->y, 0);
      break;
    case INPUT_MOVE:
      event = wpe_event_pointer_move_new(WPE_EVENT_POINTER_MOVE, host->view, WPE_INPUT_SOURCE_MOUSE, time, modifiers_of(in->modifiers, host->buttons), in->x, in->y, 0, 0);
      break;
    case INPUT_LEAVE:
      event = wpe_event_pointer_move_new(WPE_EVENT_POINTER_LEAVE, host->view, WPE_INPUT_SOURCE_MOUSE, time, modifiers_of(in->modifiers, host->buttons), in->x, in->y, 0, 0);
      break;
    case INPUT_SCROLL:
      event = wpe_event_scroll_new(host->view, in->precise ? WPE_INPUT_SOURCE_TOUCHPAD : WPE_INPUT_SOURCE_MOUSE, time, modifiers_of(in->modifiers, host->buttons), in->dx, in->dy, in->precise, FALSE, in->x, in->y);
      break;
    case INPUT_KEY_DOWN:
    case INPUT_KEY_UP:
      if (in->kind == INPUT_KEY_DOWN && !host->focused) { host->focused = TRUE; wpe_view_focus_in(host->view); }
      event = wpe_event_keyboard_new(in->kind == INPUT_KEY_DOWN ? WPE_EVENT_KEYBOARD_KEY_DOWN : WPE_EVENT_KEYBOARD_KEY_UP, host->view, WPE_INPUT_SOURCE_KEYBOARD, time, modifiers_of(in->modifiers, host->buttons), in->keycode, in->keyval);
      break;
    case INPUT_FOCUS:
      if (!host->focused) { host->focused = TRUE; wpe_view_focus_in(host->view); }
      break;
    case INPUT_BLUR:
      if (host->focused) { host->focused = FALSE; wpe_view_focus_out(host->view); }
      break;
    case INPUT_SCALE:
      if (in->dx > 0 && in->dx != host->scale) { host->scale = in->dx; wpe_toplevel_scale_changed(host->toplevel, in->dx); }
      break;
  }
  if (event) { wpe_view_event(host->view, event); wpe_event_unref(event); }
done:
  g_free(job->doc); g_free(job);
  return G_SOURCE_REMOVE;
}

// ── Exports ───────────────────────────────────────────────────────────────────────────────────────────────────────

static GMutex started_lock;
static GCond started_cond;
static gboolean started;

static gboolean engine_started(gpointer data) {
  (void)data;
  g_mutex_lock(&started_lock); started = TRUE; g_cond_signal(&started_cond); g_mutex_unlock(&started_lock);
  return G_SOURCE_REMOVE;
}

// The default GMainContext, owned by this thread for the life of the process: WebKit makes the first thread that
// calls it its main thread and binds its main RunLoop to the default context.
static gpointer engine_thread(gpointer data) {
  (void)data;
  g_main_context_acquire(main_context);
  // WebKit must initialize before any JSCContext exists in this process: a context created first leaves the UI
  // process unable to load anything (seen with 2.54.2). The web context initializes it; then the JSON parser.
  context = webkit_web_context_new();
  webkit_web_context_register_uri_scheme(context, "tauri", on_tauri_request, NULL, NULL);
  parser = jsc_context_new();
  GMainLoop *loop = g_main_loop_new(main_context, FALSE);
  g_idle_add(engine_started, NULL);
  g_main_loop_run(loop);
  return NULL;
}

// Always queued, never run inline: g_main_context_invoke would run on the caller's thread while the context is free.
static void post_to_engine(GSourceFunc fn, gpointer data) {
  GSource *source = g_idle_source_new();
  g_source_set_priority(source, G_PRIORITY_DEFAULT);
  g_source_set_callback(source, fn, data, NULL);
  g_source_attach(source, main_context);
  g_source_unref(source);
}

static gboolean queued_command(gpointer data) { run_command(data); return G_SOURCE_REMOVE; }

// Whether WebKit's web-process sandbox can run here: the system bubblewrap and D-Bus proxy WebKit was built against,
// and an unprivileged user namespace with its own network for bwrap. Ubuntu 23.10+ refuses that to /usr/bin/bwrap
// (AppArmor `unprivileged_userns`); Kybern's .deb then gives its own copy (beside the helper processes) a profile that
// allows it, and WebKit is switched to that copy in-process (`webkit_kybern_sandbox_hardened`, wpe-sandbox.patch; never
// through the environment). Upstream WebKit also skips bubblewrap and runs pages unsandboxed inside a Snap (`SNAP*`,
// which a Snap terminal passes to its children), inside Flatpak without flatpak-spawn, in a container where bwrap
// fails, and with WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS; Kybern's WebKit aborts instead, so each of those turns the
// web view off here and every surface shows WP4's browser fallback. Keep the checks in step with `linuxSandboxProbe`
// in webview-support.ts.
static gboolean bwrap_works(const char *bwrap) {
  const char *argv[] = { bwrap, "--unshare-all", "--die-with-parent", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "true", NULL };
  int status = 0;
  if (!g_file_test(bwrap, G_FILE_TEST_IS_EXECUTABLE)) return FALSE;
  if (!g_spawn_sync(NULL, (char **)argv, NULL, G_SPAWN_STDOUT_TO_DEV_NULL | G_SPAWN_STDERR_TO_DEV_NULL, NULL, NULL, NULL, NULL, &status, NULL)) return FALSE;
  return g_spawn_check_wait_status(status, NULL);
}

static char *check_sandbox(void) {
  const char *dangerous = g_getenv("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS");
  if (dangerous && strcmp(dangerous, "0")) return g_strdup("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS is set, and Kybern never runs web pages without their sandbox, so pages open in your browser. Unset it and reopen Kybern.");
  if (g_getenv("SNAP") && g_getenv("SNAP_NAME") && g_getenv("SNAP_REVISION"))
    return g_strdup("Kybern was started from inside a Snap (SNAP is set, for example by a Snap terminal), where it cannot sandbox web pages, so pages open in your browser. Start Kybern from your desktop to see them here.");
  if (g_file_test("/.flatpak-info", G_FILE_TEST_EXISTS)) return g_strdup("Kybern is running inside Flatpak, where it cannot sandbox web pages, so pages open in your browser.");
  if (!g_file_test("/usr/bin/xdg-dbus-proxy", G_FILE_TEST_IS_EXECUTABLE)) return g_strdup("Install xdg-dbus-proxy so Kybern can sandbox web pages, then reopen Kybern.");
  // Kybern's own WebKit build exports this; a system WebKit (development on a distro that packages WPE 2.0) does not,
  // and is used only with the system bwrap, under the same conditions upstream picks bubblewrap for.
  int (*hardened)(int) = NULL;
  Dl_info info;
  gboolean located = dladdr((void *)webkit_web_view_get_type, &info) && info.dli_fname && info.dli_fname[0] == '/';
  if (located) {
    void *webkit = dlopen(info.dli_fname, RTLD_LAZY | RTLD_NOLOAD);
    if (webkit) { hardened = (int (*)(int))dlsym(webkit, "webkit_kybern_sandbox_hardened"); dlclose(webkit); }
  }
  if (bwrap_works("/usr/bin/bwrap") && (!hardened || hardened(0))) return NULL;
  if (located && hardened) {
    char *dir = g_path_get_dirname(info.dli_fname);
    char *own = g_build_filename(dir, "wpe-webkit-2.0", "bwrap", NULL);
    gboolean works = bwrap_works(own) && hardened(1);
    g_free(dir); g_free(own);
    if (works) return NULL;
  }
  if (!g_file_test("/usr/bin/bwrap", G_FILE_TEST_IS_EXECUTABLE)) return g_strdup("Install bubblewrap (bwrap) so Kybern can sandbox web pages, then reopen Kybern.");
  return g_strdup("This system does not let Kybern sandbox web pages (bubblewrap cannot create a user namespace), so pages open in your browser.");
}

__attribute__((visibility("default"))) void kybern_web_init(void (*callback)(void)) {
  static gsize once;
  signal_js = callback;
  if (!g_once_init_enter(&once)) return;
  hosts = g_hash_table_new_full(g_str_hash, g_str_equal, g_free, (GDestroyNotify)host_free);
  frames = g_hash_table_new_full(g_str_hash, g_str_equal, g_free, (GDestroyNotify)frame_free);
  available_reason = check_sandbox();
  main_context = g_main_context_default();
  g_thread_new("kybern-webview", engine_thread, NULL);
  g_mutex_lock(&started_lock);
  while (!started) g_cond_wait(&started_cond, &started_lock);
  g_mutex_unlock(&started_lock);
  g_once_init_leave(&once, 1);
}

__attribute__((visibility("default"))) const char *kybern_web_next(void) {
  g_mutex_lock(&events_lock);
  char *next = g_queue_pop_head(&events);
  g_mutex_unlock(&events_lock);
  g_free(last_event);
  last_event = next;
  return next;
}

__attribute__((visibility("default"))) void kybern_web_command(const char *json) {
  if (!json || !main_context) return;
  post_to_engine(queued_command, g_strdup(json));
}

// NULL when the web view can run, else the sentence a surface shows (read once by `webview-support.ts`).
__attribute__((visibility("default"))) const char *kybern_web_unavailable_reason(void) { return available_reason; }

__attribute__((visibility("default"))) void kybern_web_surface_listen(KybernNotify fn, void *ctx) {
  g_mutex_lock(&notify_lock);
  notify_fn = fn; notify_context = ctx;
  g_mutex_unlock(&notify_lock);
}

// Locks the document's latest frame for an upload. Returns 1 with `frames_lock` held (call `unlock`), else 0.
__attribute__((visibility("default"))) int32_t kybern_web_surface_lock(const char *doc, KybernWebFrame *out) {
  if (!frames || !doc || !out) return 0;
  g_mutex_lock(&frames_lock);
  Frame *frame = g_hash_table_lookup(frames, doc);
  if (!frame) { g_mutex_unlock(&frames_lock); return 0; }
  out->width = (uint32_t)frame->width; out->height = (uint32_t)frame->height; out->stride = (uint32_t)frame->stride;
  out->pixels = frame->pixels; out->serial = frame->serial;
  out->dirty_x = frame->dirty_x0; out->dirty_y = frame->dirty_y0;
  out->dirty_width = MAX(0, frame->dirty_x1 - frame->dirty_x0); out->dirty_height = MAX(0, frame->dirty_y1 - frame->dirty_y0);
  out->frame_x = frame->frame_x; out->frame_y = frame->frame_y; out->frame_width = frame->frame_width; out->frame_height = frame->frame_height;
  out->visible = frame->visible; out->slot = frame->slot;
  return 1;
}

// `consume` clears the dirty rectangle (the caller uploaded it).
__attribute__((visibility("default"))) void kybern_web_surface_unlock(const char *doc, int32_t consume) {
  Frame *frame = doc ? g_hash_table_lookup(frames, doc) : NULL;
  if (frame && consume) frame->dirty_x0 = frame->dirty_y0 = frame->dirty_x1 = frame->dirty_y1 = 0;
  g_mutex_unlock(&frames_lock);
}

__attribute__((visibility("default"))) void kybern_web_surface_input(const char *doc, const KybernWebInput *input) {
  if (!doc || !input || !main_context) return;
  InputJob *job = g_new0(InputJob, 1);
  job->doc = g_strdup(doc); job->input = *input;
  post_to_engine(run_input, job);
}
