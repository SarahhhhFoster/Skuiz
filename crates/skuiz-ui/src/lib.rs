//! skuiz-ui: embeds a webview editor (via wry) as a child of a
//! host-provided native view. The system webview is a shared OS library, so
//! each plugin instance stays light.
//!
//! The JS side talks to Rust with `window.ipc.postMessage("...")`; Rust talks
//! back with [`Editor::eval`]. The protocol is plain strings — plugin authors
//! layer whatever they like on top.

#![warn(missing_docs)]
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};
use wry::dpi::{LogicalPosition, LogicalSize};

/// A native view handle given to us by a plugin host
/// (CLAP `cocoa` NSView, VST3 NSView, ...).
pub struct ParentView(RawWindowHandle);

#[cfg(target_os = "macos")]
impl ParentView {
    /// # Safety
    /// `ns_view` must be a valid `NSView*` that outlives the [`Editor`]
    /// attached to it.
    pub unsafe fn from_ns_view(ns_view: *mut std::ffi::c_void) -> Option<Self> {
        std::ptr::NonNull::new(ns_view).map(|nn| {
            Self(RawWindowHandle::AppKit(
                raw_window_handle::AppKitWindowHandle::new(nn),
            ))
        })
    }
}

// ponytail: X11/Wayland still missing; add a from_x11 constructor when
// someone runs this on Linux. wry builds child webviews on X11 only.
#[cfg(target_os = "windows")]
impl ParentView {
    /// # Safety
    /// `hwnd` must be a valid window handle that outlives the [`Editor`]
    /// attached to it.
    pub unsafe fn from_hwnd(hwnd: *mut std::ffi::c_void) -> Option<Self> {
        std::ptr::NonNull::new(hwnd).map(|nn| {
            let mut handle = raw_window_handle::Win32WindowHandle::new(
                std::num::NonZeroIsize::new(nn.as_ptr() as isize)
                    .expect("NonNull pointer is never zero"),
            );
            // The module handle is optional; wry only needs the window.
            handle.hinstance = None;
            Self(RawWindowHandle::Win32(handle))
        })
    }
}

impl HasWindowHandle for ParentView {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // Safety: the unsafe constructors require the view to outlive the Editor.
        Ok(unsafe { WindowHandle::borrow_raw(self.0) })
    }
}

/// A live webview editor attached to a host view. Drop to detach.
///
/// Drop it on the main thread: detaching tears down the native webview,
/// which is as main-thread-bound as creating it was.
pub struct Editor {
    webview: wry::WebView,
}

impl Editor {
    /// Attach a webview showing `html`, sized in logical pixels. Must be
    /// called on the main thread. `on_message` receives strings the page
    /// sends via `window.ipc.postMessage(...)`.
    pub fn attach(
        parent: &ParentView,
        html: &str,
        size: (u32, u32),
        on_message: impl Fn(String) + 'static,
    ) -> Result<Self, wry::Error> {
        let webview = wry::WebViewBuilder::new()
            .with_html(html)
            .with_bounds(wry::Rect {
                position: LogicalPosition::new(0, 0).into(),
                size: LogicalSize::new(size.0, size.1).into(),
            })
            .with_ipc_handler(move |req| on_message(req.into_body()))
            .build_as_child(parent)?;
        Ok(Self { webview })
    }

    /// Run JavaScript in the page (Rust -> UI channel). Main thread only:
    /// wry hands the call straight to the native webview (WKWebView /
    /// WebView2), which requires the thread the webview was created on.
    pub fn eval(&self, js: &str) -> Result<(), wry::Error> {
        self.webview.evaluate_script(js)
    }

    /// Resize the webview to `size` in logical pixels. Main thread only.
    pub fn resize(&self, size: (u32, u32)) -> Result<(), wry::Error> {
        self.webview.set_bounds(wry::Rect {
            position: LogicalPosition::new(0, 0).into(),
            size: LogicalSize::new(size.0, size.1).into(),
        })
    }
}
