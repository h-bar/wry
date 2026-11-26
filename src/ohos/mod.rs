use std::{borrow::Cow, rc::Rc};

use super::WebViewAttributes;
use crate::{Error, PageLoadEvent, Rect, RequestAsyncResponder, Result, RGBA};
use cookie::Cookie;
use http::{Request, Response, Uri};
use openharmony_ability::{native_web::WebProxyBuilder, WebViewBuilder, WebViewStyle, Webview};
use raw_window_handle::HasWindowHandle;

use crate::util::Counter;

static COUNTER: Counter = Counter::new();

mod cookie_ndk {
  use std::ffi::{c_char, c_void, CStr, CString};
  use std::sync::atomic::{AtomicPtr, Ordering};

  const ARKWEB_NATIVE_COOKIE_MANAGER: i32 = 4;
  const ARKWEB_SUCCESS: i32 = 0;

  #[repr(C)]
  struct ArkWebCookieManagerApi {
    size: usize,
    fetch_cookie_sync: Option<
      unsafe extern "C" fn(
        url: *const c_char,
        incognito: bool,
        include_http_only: bool,
        cookie_value: *mut *mut c_char,
      ) -> i32,
    >,
    config_cookie_sync: Option<
      unsafe extern "C" fn(
        url: *const c_char,
        cookie_value: *const c_char,
        incognito: bool,
        include_http_only: bool,
      ) -> i32,
    >,
    exist_cookies: Option<unsafe extern "C" fn(incognito: bool) -> bool>,
    clear_all_cookies_sync: Option<unsafe extern "C" fn(incognito: bool)>,
    clear_session_cookies_sync: Option<unsafe extern "C" fn()>,
  }

  #[link(name = "ohweb")]
  unsafe extern "C" {
    fn OH_ArkWeb_GetNativeAPI(kind: i32) -> *const c_void;
    fn OH_ArkWeb_ReleaseString(string: *mut c_char);
  }

  unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
  }

  static COOKIE_API: AtomicPtr<ArkWebCookieManagerApi> = AtomicPtr::new(std::ptr::null_mut());

  /// Obtain and cache the ArkWeb CookieManager NDK struct. Safe to call repeatedly; must be
  /// called at least once on the UI thread (e.g. during WebView init / controller attach).
  pub fn init_on_ui_thread() {
    if !COOKIE_API.load(Ordering::Acquire).is_null() {
      return;
    }
    let ptr = unsafe { OH_ArkWeb_GetNativeAPI(ARKWEB_NATIVE_COOKIE_MANAGER) }
      as *mut ArkWebCookieManagerApi;
    if !ptr.is_null() {
      COOKIE_API.store(ptr, Ordering::Release);
    }
  }

  fn api() -> Option<&'static ArkWebCookieManagerApi> {
    let mut ptr = COOKIE_API.load(Ordering::Acquire);
    if ptr.is_null() {
      init_on_ui_thread();
      ptr = COOKIE_API.load(Ordering::Acquire);
    }
    if ptr.is_null() {
      None
    } else {
      // SAFETY: `OH_ArkWeb_GetNativeAPI` returns a process-lifetime static vtable.
      Some(unsafe { &*ptr })
    }
  }

  fn save_cookie_async_best_effort() {
    type SaveFn = unsafe extern "C" fn(callback: *const c_void);
    // Available on API 20+; resolved dynamically so API 12..19 devices stay compatible.
    let sym = unsafe {
      dlsym(
        std::ptr::null_mut(), // RTLD_DEFAULT on OpenHarmony musl
        b"OH_ArkWebCookieManager_SaveCookieAsync\0".as_ptr().cast(),
      )
    };
    if !sym.is_null() {
      let save: SaveFn = unsafe { std::mem::transmute(sym) };
      unsafe { save(std::ptr::null()) };
    }
  }

  pub fn fetch_cookie_sync(url: &str) -> Option<String> {
    let api = api()?;
    if api.size < std::mem::offset_of!(ArkWebCookieManagerApi, fetch_cookie_sync) + std::mem::size_of::<usize>() {
      return None;
    }
    let fetch = api.fetch_cookie_sync?;
    let c_url = CString::new(url).ok()?;
    let mut raw: *mut c_char = std::ptr::null_mut();
    let code = unsafe { fetch(c_url.as_ptr(), false, true, &mut raw) };
    if code != ARKWEB_SUCCESS {
      if !raw.is_null() {
        unsafe { OH_ArkWeb_ReleaseString(raw) };
      }
      return None;
    }
    if raw.is_null() {
      return Some(String::new());
    }
    let out = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
    unsafe { OH_ArkWeb_ReleaseString(raw) };
    Some(out)
  }

  pub fn config_cookie_sync(url: &str, cookie_value: &str) -> std::result::Result<(), String> {
    let api = api().ok_or_else(|| "ArkWeb CookieManager NDK API unavailable".to_string())?;
    if api.size < std::mem::offset_of!(ArkWebCookieManagerApi, config_cookie_sync) + std::mem::size_of::<usize>() {
      return Err("ArkWeb CookieManager configCookieSync unavailable".to_string());
    }
    let config = api
      .config_cookie_sync
      .ok_or_else(|| "ArkWeb CookieManager configCookieSync is null".to_string())?;
    let c_url = CString::new(url).map_err(|e| format!("invalid cookie url: {e}"))?;
    let c_val = CString::new(cookie_value).map_err(|e| format!("invalid cookie value: {e}"))?;
    let code = unsafe { config(c_url.as_ptr(), c_val.as_ptr(), false, true) };
    if code != ARKWEB_SUCCESS {
      return Err(format!("configCookieSync failed with code {code}"));
    }
    save_cookie_async_best_effort();
    Ok(())
  }
}

fn cookie_url_for(cookie: &Cookie<'_>, fallback_url: Option<&str>) -> Option<String> {
  let path = cookie.path().unwrap_or("/");
  let path = if path.starts_with('/') {
    Cow::Borrowed(path)
  } else {
    Cow::Owned(format!("/{path}"))
  };
  if let Some(domain) = cookie.domain() {
    let host = domain.trim_start_matches('.');
    if !host.is_empty() {
      return Some(format!("https://{host}{path}"));
    }
  }
  let url = fallback_url?;
  let uri = url.parse::<Uri>().ok()?;
  let host = uri.host()?;
  Some(format!("https://{host}{path}"))
}

pub(crate) struct InnerWebView {
  id: String,
  webview: Webview,
}

impl Drop for InnerWebView {
  fn drop(&mut self) {
    let _ = self.webview.dispose();
  }
}

impl InnerWebView {
  pub fn new_as_child(
    _window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new(_window, attributes, pl_attrs)
  }

  pub fn new(
    _window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    _pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    let WebViewAttributes {
      id,
      url,
      html,
      initialization_scripts,
      ipc_handler,
      navigation_handler,
      on_page_load_handler,
      #[cfg(any(debug_assertions, feature = "devtools"))]
      devtools,
      custom_protocols,
      background_color,
      transparent,
      headers,
      autoplay,
      user_agent,
      javascript_disabled,
      ..
    } = attributes;

    let initial_url = url;
    let initial_headers = headers;

    let id = id
      .map(|id| id.to_string())
      .unwrap_or_else(|| COUNTER.next().to_string());

    let background_color =
      background_color.map(|c| format!("#{:02X}{:02X}{:02X}{:02X}", c.0, c.1, c.2, c.3));

    let mut webview_builder = WebViewBuilder::new()
      .id(id.clone())
      .style(WebViewStyle {
        x: None,
        y: None,
        visible: None,
        background_color,
      })
      .javascript_enabled(!javascript_disabled)
      .autoplay(autoplay)
      .initialization_scripts(vec![initialization_scripts
        .iter()
        .map(|s| {
          if s.for_main_frame_only {
            format!("if (window === window.top) {{\n{}\n}}", s.script)
          } else {
            s.script.clone()
          }
        })
        .collect::<Vec<_>>()
        .join("\n;\n")])
      .transparent(transparent);

    #[cfg(any(debug_assertions, feature = "devtools"))]
    {
      webview_builder = webview_builder.devtools(devtools);
    }

    if let Some(html) = html {
      webview_builder = webview_builder.html(html);
    } else if let Some(url) = initial_url {
      webview_builder = webview_builder.url(url);
      if let Some(headers) = initial_headers {
        webview_builder = webview_builder.headers(headers);
      }
    }

    if let Some(user_agent) = user_agent {
      webview_builder = webview_builder.user_agent(user_agent);
    }

    if let Some(navigation_handler) = navigation_handler {
      // ArkWeb `onLoadIntercept` returns `true` to block a load; wry's `navigation_handler`
      // returns `true` to allow it.
      webview_builder = webview_builder.on_navigation_request(move |url| !navigation_handler(url));
    }

    cookie_ndk::init_on_ui_thread();

    let webview = webview_builder
      .build()
      .map_err(|e| Error::OpenHarmonyInitError(e.to_string()))?;

    let current_id = id.clone();
    let ipc_webview = webview.clone();
    webview
      .on_controller_attach(move || {
        cookie_ndk::init_on_ui_thread();
        let _builder = WebProxyBuilder::new(current_id.clone(), "ipc".to_string())
          .add_method("postMessage", |_frame, params| {
            if let Some(ipc_handler) = &ipc_handler {
              let message = params.get(0).unwrap_or(&"".to_string()).to_owned();
              // when load html, url will be base64 and uri parse will failed, so we use about:blank avoid panic
              let url = ipc_webview.url().unwrap_or("about:blank".to_string());
              let uri = url
                .parse::<Uri>()
                .unwrap_or(Uri::from_static("about:blank"));

              ipc_handler(Request::builder().uri(uri).body(message).unwrap());
            }
          })
          .build()
          .expect("Failed to build web proxy");
      })
      .map_err(|e| {
        Error::OpenHarmonyInitError(format!("Failed to add controller attach listener: {}", e))
      })?;

    if let Some(handler) = on_page_load_handler {
      let handler = Rc::new(handler);
      let start_handler = handler.clone();
      let start_wv = webview.clone();
      let _ = webview.on_page_begin(move || {
        let url = start_wv.url().unwrap_or_default();
        start_handler(PageLoadEvent::Started, url);
      });
      let end_wv = webview.clone();
      let _ = webview.on_page_end(move || {
        let url = end_wv.url().unwrap_or_default();
        handler(PageLoadEvent::Finished, url);
      });
    }

    for (protocol, callback) in custom_protocols {
      let webview_id = id.clone();
      webview
        .custom_protocol_async(protocol, move |_web, req, _is_on_main_frame, responder| {
          let responder: Box<dyn FnOnce(Response<Cow<'static, [u8]>>)> = Box::new(move |resp| {
            responder.respond(resp);
          });

          (callback)(&webview_id, req, RequestAsyncResponder { responder });
        })
        .unwrap();
    }

    Ok(Self { id, webview })
  }

  pub fn print(&self) -> crate::Result<()> {
    Ok(())
  }

  pub fn id(&self) -> crate::WebViewId<'_> {
    &self.id
  }

  pub fn url(&self) -> crate::Result<String> {
    self
      .webview
      .url()
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to get url: {}", e)))
  }

  pub fn eval(&self, js: &str, callback: Option<impl Fn(String) + Send + 'static>) -> Result<()> {
    self
      .webview
      .evaluate_script_with_callback(
        js,
        callback.map(|c| Box::new(c) as Box<dyn Fn(String) + Send + 'static>),
      )
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to evaluate script: {}", e)))
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn open_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn close_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn is_devtools_open(&self) -> bool {
    false
  }

  pub fn zoom(&self, scale_factor: f64) -> Result<()> {
    self
      .webview
      .set_zoom(scale_factor)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set zoom: {}", e)))
  }

  pub fn set_background_color(&self, background_color: RGBA) -> Result<()> {
    let hex = format!(
      "#{:02X}{:02X}{:02X}{:02X}",
      background_color.0, background_color.1, background_color.2, background_color.3
    );
    self
      .webview
      .set_background_color(&hex)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set background color: {}", e)))
  }

  pub fn cookies(&self) -> Result<Vec<Cookie<'static>>> {
    Ok(vec![])
  }

  pub fn set_cookie(&self, cookie: &Cookie<'_>) -> Result<()> {
    let fallback = self.webview.url().ok();
    let Some(url) = cookie_url_for(cookie, fallback.as_deref()) else {
      return Ok(());
    };
    cookie_ndk::config_cookie_sync(&url, &cookie.to_string())
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set cookie: {e}")))
  }

  pub fn delete_cookie(&self, cookie: &Cookie<'_>) -> Result<()> {
    let fallback = self.webview.url().ok();
    let Some(url) = cookie_url_for(cookie, fallback.as_deref()) else {
      return Ok(());
    };
    let path = cookie.path().unwrap_or("/");
    let name = cookie.name();
    let host_only =
      format!("{name}=; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Path={path}");
    cookie_ndk::config_cookie_sync(&url, &host_only)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to delete cookie: {e}")))?;
    if let Some(domain) = cookie.domain() {
      let domain_scoped = format!(
        "{name}=; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Path={path}; Domain={domain}"
      );
      let _ = cookie_ndk::config_cookie_sync(&url, &domain_scoped);
    }
    Ok(())
  }

  pub fn cookies_for_url(&self, url: &str) -> Result<Vec<Cookie<'static>>> {
    let raw = match cookie_ndk::fetch_cookie_sync(url) {
      Some(s) => s,
      None => self
        .webview
        .cookies_with_url(url)
        .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to get cookies: {}", e)))?,
    };
    let cookies_data: Vec<Cookie<'static>> = raw
      .split(';')
      .filter_map(|s| {
        let s = s.trim();
        if s.is_empty() {
          return None;
        }
        Cookie::parse(s.to_string()).map(|c| c.into_owned()).ok()
      })
      .collect();
    Ok(cookies_data)
  }

  pub fn reload(&self) -> Result<()> {
    self
      .webview
      .reload()
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to reload: {}", e)))
  }

  pub fn load_url(&self, url: &str) -> Result<()> {
    self
      .webview
      .load_url(url)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to load url: {}", e)))
  }

  pub fn load_url_with_headers(&self, url: &str, headers: http::HeaderMap) -> Result<()> {
    self
      .webview
      .load_url_with_headers(url, headers)
      .map_err(|e| {
        Error::OpenHarmonyWebviewError(format!("Failed to load url with headers: {}", e))
      })
  }

  pub fn load_html(&self, html: &str) -> Result<()> {
    self
      .webview
      .load_html(html)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to load html: {}", e)))
  }

  pub fn clear_all_browsing_data(&self) -> Result<()> {
    self.webview.clear_all_browsing_data().map_err(|e| {
      Error::OpenHarmonyWebviewError(format!("Failed to clear all browsing data: {}", e))
    })
  }

  pub fn bounds(&self) -> Result<Rect> {
    Ok(Rect::default())
  }

  pub fn set_bounds(&self, _bounds: Rect) -> Result<()> {
    Ok(())
  }

  pub fn set_visible(&self, visible: bool) -> Result<()> {
    self
      .webview
      .set_visible(visible)
      .map_err(|e| Error::OpenHarmonyWebviewError(format!("Failed to set visible: {}", e)))
  }

  pub fn focus(&self) -> Result<()> {
    self.webview.focus().map_err(|_e| Error::NotMainThread)
  }

  pub fn focus_parent(&self) -> Result<()> {
    Ok(())
  }

  pub fn go_forward(&self) -> Result<()> {
    Ok(())
  }

  pub fn go_back(&self) -> Result<()> {
    Ok(())
  }

  pub fn can_go_forward(&self) -> Result<bool> {
    Ok(true)
  }

  pub fn can_go_back(&self) -> Result<bool> {
    Ok(true)
  }
}

pub fn platform_webview_version() -> Result<String> {
  Ok("1.0.0".to_string())
}
