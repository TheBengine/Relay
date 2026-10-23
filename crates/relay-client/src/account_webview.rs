//! The actual HTTPS identity page, hosted in Relay's own private WebView2 surface.
//! No password API, injected script, IPC bridge, cookie access or token handling.
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle, WindowHandle};
use std::{cell::RefCell, path::Path, rc::Rc, sync::mpsc};
use url::Url;
use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::WindowsAndMessaging::GetClientRect,
};
use wry::{
    dpi::{PhysicalPosition, PhysicalSize},
    WebViewBuilderExtWindows,
};

#[derive(Clone)]
struct Boundary {
    authorization: Url,
    realm: String,
    callback: Url,
}

impl Boundary {
    fn new(authorization: Url) -> Option<Self> {
        if authorization.scheme() != "https"
            || authorization.host_str().is_none()
            || !authorization.username().is_empty()
            || authorization.password().is_some()
            || authorization.fragment().is_some()
        {
            return None;
        }
        let realm = authorization
            .path()
            .strip_suffix("/protocol/openid-connect/auth")?
            .to_owned();
        if !realm.starts_with("/realms/") || realm[8..].contains('/') {
            return None;
        }
        let callbacks: Vec<_> = authorization
            .query_pairs()
            .filter(|(key, _)| key == "redirect_uri")
            .map(|(_, value)| value.into_owned())
            .collect();
        if callbacks.len() != 1 {
            return None;
        }
        let callback = Url::parse(&callbacks[0]).ok()?;
        if callback.scheme() != "http"
            || callback.host_str() != Some("127.0.0.1")
            || callback.port() != Some(4583)
            || callback.path() != "/relay-account/callback"
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.query().is_some()
            || callback.fragment().is_some()
        {
            return None;
        }
        Some(Self {
            authorization,
            realm,
            callback,
        })
    }

    fn allows(&self, value: &Url) -> bool {
        if !value.username().is_empty() || value.password().is_some() {
            return false;
        }
        (value.origin() == self.authorization.origin()
            && (value.path() == self.realm
                || value.path().starts_with(&format!("{}/", self.realm))))
            || (value.origin() == self.callback.origin() && value.path() == self.callback.path())
    }

    // Social providers keep their browser-based approval. Restart the same
    // PKCE request with the selected broker; never open an arbitrary URL.
    fn social_browser(&self, value: &Url) -> Option<Url> {
        if value.origin() != self.authorization.origin() {
            return None;
        }
        let alias = value
            .path()
            .strip_prefix(&format!("{}/broker/", self.realm))?
            .strip_suffix("/login")?;
        if !matches!(alias, "google" | "microsoft") {
            return None;
        }
        let mut browser = self.authorization.clone();
        browser.query_pairs_mut().append_pair("kc_idp_hint", alias);
        Some(browser)
    }
}

pub(crate) enum Request {
    Browser(Url),
    Blocked,
}

struct Parent(raw_window_handle::Win32WindowHandle);
impl HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        // Captured from this app's CreationContext and retained only on its UI thread.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(self.0)) })
    }
}

pub(crate) struct Surface {
    parent: Parent,
    context: wry::WebContext,
    view: Option<wry::WebView>,
    boundary: Rc<RefCell<Option<Boundary>>>,
    send: mpsc::SyncSender<Request>,
    receive: mpsc::Receiver<Request>,
    visible: bool,
    bounds: Option<(i32, i32, i32, i32)>,
    pub(crate) started: bool,
    pub(crate) browser_only: bool,
    #[cfg(test)]
    owned_fixture: Option<Url>,
}

impl Surface {
    pub(crate) fn new(cc: &eframe::CreationContext<'_>, profile: &Path) -> Option<Self> {
        let RawWindowHandle::Win32(parent) = cc.window_handle().ok()?.as_raw() else {
            return None;
        };
        let (send, receive) = mpsc::sync_channel(4);
        Some(Self {
            parent: Parent(parent),
            context: wry::WebContext::new(Some(profile.join("account-webview"))),
            view: None,
            boundary: Rc::default(),
            send,
            receive,
            visible: false,
            bounds: None,
            started: false,
            browser_only: false,
            #[cfg(test)]
            owned_fixture: None,
        })
    }

    pub(crate) fn visible(&self) -> bool {
        self.visible
    }

    pub(crate) fn open(&mut self, url: Url, ctx: &egui::Context) -> Result<(), &'static str> {
        let boundary = Boundary::new(url.clone());
        #[cfg(test)]
        let boundary = boundary.or_else(|| {
            let fixture = self.owned_fixture.as_ref()?;
            if url.origin() != fixture.origin() {
                return None;
            }
            let mut secure = url.clone();
            secure.set_scheme("https").ok()?;
            let mut boundary = Boundary::new(secure)?;
            if boundary.realm != fixture.path() {
                return None;
            }
            boundary.authorization = url.clone();
            Some(boundary)
        });
        let boundary = boundary.ok_or("This account service needs browser sign-in.")?;
        *self.boundary.borrow_mut() = Some(boundary);
        self.started = true;
        if let Some(view) = &self.view {
            view.load_url(url.as_str())
                .map_err(|_| "The sign-in page could not open. Try browser sign-in.")?;
            view.set_visible(true)
                .map_err(|_| "The sign-in page could not be shown.")?;
        } else {
            let policy = self.boundary.clone();
            let send = self.send.clone();
            let repaint = ctx.clone();
            let view = wry::WebViewBuilder::new_with_web_context(&mut self.context)
                .with_url(url.as_str())
                .with_incognito(true)
                .with_profile_name("relay-login")
                // Keep the runtime's security defaults, including SmartScreen.
                .with_additional_browser_args("")
                .with_focused(false)
                .with_devtools(false)
                .with_general_autofill_enabled(false)
                .with_background_color((17, 18, 20, 255))
                .with_bounds(wry::Rect {
                    position: PhysicalPosition::new(0, 30).into(),
                    size: PhysicalSize::new(1, 1).into(),
                })
                .with_navigation_handler(move |value| {
                    let parsed = Url::parse(&value).ok();
                    let policy = policy.borrow();
                    if let (Some(policy), Some(value)) = (policy.as_ref(), parsed.as_ref()) {
                        if let Some(browser) = policy.social_browser(value) {
                            let _ = send.try_send(Request::Browser(browser));
                            repaint.request_repaint();
                            return false;
                        }
                        if policy.allows(value) {
                            return true;
                        }
                    }
                    let _ = send.try_send(Request::Blocked);
                    repaint.request_repaint();
                    false
                })
                .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
                .with_download_started_handler(|_, _| false)
                .with_permission_handler(|_| wry::PermissionResponse::Deny)
                .build_as_child(&self.parent)
                .map_err(|_| {
                    "In-app sign-in could not start. You can still sign in with your browser."
                })?;
            self.view = Some(view);
        }
        self.visible = true;
        self.bounds = None;
        Ok(())
    }

    pub(crate) fn resize(&mut self, rect: egui::Rect, scale: f32) {
        let Some(view) = &self.view else {
            return;
        };
        let mut client = RECT::default();
        if unsafe { GetClientRect(HWND(self.parent.0.hwnd.get() as *mut _), &mut client) }.is_err()
        {
            return;
        }
        let x = (rect.left() * scale).round().max(0.0) as i32;
        let y = (rect.top() * scale).round().max(0.0) as i32;
        let width = ((rect.width() * scale).round() as i32)
            .min(client.right - x)
            .max(1);
        let height = ((rect.height() * scale).round() as i32)
            .min(client.bottom - y)
            .max(1);
        let bounds = (x, y, width, height);
        if self.bounds != Some(bounds)
            && view
                .set_bounds(wry::Rect {
                    position: PhysicalPosition::new(x, y).into(),
                    size: PhysicalSize::new(width as u32, height as u32).into(),
                })
                .is_ok()
        {
            self.bounds = Some(bounds);
        }
    }

    pub(crate) fn hide(&mut self) {
        if self.visible {
            if let Some(view) = &self.view {
                let _ = view.set_visible(false);
            }
        }
        self.visible = false;
    }
    pub(crate) fn browser_url(&self) -> Option<Url> {
        self.boundary
            .borrow()
            .as_ref()
            .map(|boundary| boundary.authorization.clone())
    }
    pub(crate) fn take_request(&mut self) -> Option<Request> {
        self.receive.try_recv().ok()
    }
    pub(crate) fn reset(&mut self) {
        self.hide();
        if let Some(view) = self.view.take() {
            let _ = view.clear_all_browsing_data();
        }
        *self.boundary.borrow_mut() = None;
        while self.receive.try_recv().is_ok() {}
    }

    #[cfg(test)]
    pub(crate) fn owned_loopback_fixture(&mut self, issuer: &str) {
        let issuer = Url::parse(issuer).expect("owned fixture issuer");
        assert_eq!(issuer.scheme(), "http");
        assert_eq!(issuer.host_str(), Some("127.0.0.1"));
        assert_eq!(issuer.path(), "/realms/relay-accounts");
        assert!(issuer.port().is_some());
        self.owned_fixture = Some(issuer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn boundary() -> Boundary {
        Boundary::new(Url::parse("https://accounts.example/realms/relay-accounts/protocol/openid-connect/auth?redirect_uri=http%3A%2F%2F127.0.0.1%3A4583%2Frelay-account%2Fcallback&state=owned&code_challenge=challenge").unwrap()).unwrap()
    }
    #[test]
    fn navigation_keeps_identity_origin_and_exact_callback() {
        let boundary = boundary();
        for allowed in ["https://accounts.example/realms/relay-accounts/login-actions/authenticate?session_code=owned", "http://127.0.0.1:4583/relay-account/callback?code=owned&state=owned"] { assert!(boundary.allows(&Url::parse(allowed).unwrap())); }
        for denied in [
            "https://accounts.example.evil/realms/relay-accounts/",
            "http://accounts.example/realms/relay-accounts/",
            "https://accounts.example/admin/",
            "https://accounts.example/realms/other/",
            "https://accounts.example/realms/relay-accounts-evil/",
            "http://127.0.0.1:4584/relay-account/callback",
            "http://127.0.0.1:4583/other",
            "file:///C:/test",
            "https://name:password@accounts.example/realms/relay-accounts/",
        ] {
            assert!(!boundary.allows(&Url::parse(denied).unwrap()));
        }
    }
    #[test]
    fn social_approval_uses_original_pkce_and_known_brokers() {
        let boundary = boundary();
        let result=boundary.social_browser(&Url::parse("https://accounts.example/realms/relay-accounts/broker/google/login?client_id=owned").unwrap()).unwrap();
        assert!(result
            .query_pairs()
            .any(|(key, value)| key == "state" && value == "owned"));
        assert!(result
            .query_pairs()
            .any(|(key, value)| key == "code_challenge" && value == "challenge"));
        assert!(result
            .query_pairs()
            .any(|(key, value)| key == "kc_idp_hint" && value == "google"));
        for value in [
            "https://other.example/realms/relay-accounts/broker/google/login",
            "https://accounts.example/realms/relay-accounts/broker/unknown/login",
        ] {
            assert!(boundary
                .social_browser(&Url::parse(value).unwrap())
                .is_none());
        }
    }
}
