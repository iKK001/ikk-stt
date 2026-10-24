//! Focused-element access through Microsoft UI Automation: element
//! classification for the probe and full-text read-back for verification.
//! COM objects are thread-bound, so the automation instance is thread-local
//! and nothing crosses a thread boundary (the probe runs under a budget on
//! a worker thread and returns plain values).

use std::cell::RefCell;

use windows::core::Interface;
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationValuePattern,
    UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
    UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId, UIA_TabItemControlTypeId,
    UIA_TextControlTypeId, UIA_TextPatternId, UIA_TreeItemControlTypeId, UIA_ValuePatternId,
};

#[derive(Debug, thiserror::Error)]
pub enum UiaError {
    #[error("UI Automation: {0}")]
    Backend(String),
    #[error("password field; content deliberately not read")]
    Password,
    #[error("element exposes no text")]
    NoText,
}

fn win(e: windows::core::Error) -> UiaError {
    UiaError::Backend(e.to_string())
}

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Field order matters: `automation` drops before the apartment closes.
struct ThreadState {
    automation: IUIAutomation,
    _com: ComApartment,
}

impl ThreadState {
    fn new() -> Result<Self, UiaError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(UiaError::Backend(format!("CoInitializeEx: {hr:?}")));
        }
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.map_err(win)?;
        Ok(Self {
            automation,
            _com: ComApartment,
        })
    }
}

thread_local! {
    static UIA: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

pub fn with_automation<T>(f: impl FnOnce(&IUIAutomation) -> Result<T, UiaError>) -> Result<T, UiaError> {
    UIA.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(ThreadState::new()?);
        }
        f(&slot.as_ref().expect("just set").automation)
    })
}

/// Fetches the focused element fresh on every call: focus may move between
/// two dictation steps.
pub fn with_focused<T>(f: impl FnOnce(&IUIAutomationElement) -> Result<T, UiaError>) -> Result<T, UiaError> {
    with_automation(|a| {
        let el = unsafe { a.GetFocusedElement() }.map_err(|e| UiaError::Backend(format!("no focused element: {e}")))?;
        f(&el)
    })
}

pub fn text_pattern(el: &IUIAutomationElement) -> Option<IUIAutomationTextPattern> {
    unsafe { el.GetCurrentPattern(UIA_TextPatternId) }
        .ok()?
        .cast::<IUIAutomationTextPattern>()
        .ok()
}

pub fn value_pattern(el: &IUIAutomationElement) -> Option<IUIAutomationValuePattern> {
    unsafe { el.GetCurrentPattern(UIA_ValuePatternId) }
        .ok()?
        .cast::<IUIAutomationValuePattern>()
        .ok()
}

pub fn is_password(el: &IUIAutomationElement) -> bool {
    unsafe { el.CurrentIsPassword() }.map(|b| b.as_bool()).unwrap_or(false)
}

pub fn native_window(el: &IUIAutomationElement) -> HWND {
    unsafe { el.CurrentNativeWindowHandle() }.unwrap_or(HWND(std::ptr::null_mut()))
}

pub fn value_is_writable(el: &IUIAutomationElement) -> bool {
    value_pattern(el)
        .and_then(|vp| unsafe { vp.CurrentIsReadOnly() }.ok())
        .map(|b| !b.as_bool())
        .unwrap_or(false)
}

/// Control types that never take text input.
pub fn is_static_control(el: &IUIAutomationElement) -> bool {
    let Ok(ct) = (unsafe { el.CurrentControlType() }) else {
        return false;
    };
    [
        UIA_ButtonControlTypeId,
        UIA_CheckBoxControlTypeId,
        UIA_RadioButtonControlTypeId,
        UIA_HyperlinkControlTypeId,
        UIA_ImageControlTypeId,
        UIA_ListItemControlTypeId,
        UIA_MenuItemControlTypeId,
        UIA_TabItemControlTypeId,
        UIA_TreeItemControlTypeId,
        UIA_TextControlTypeId,
    ]
    .contains(&ct)
}

/// Plain-value snapshot that may leave the probe thread.
pub struct ElementInfo {
    pub hwnd: HWND,
    pub is_password: bool,
    pub has_text_pattern: bool,
    pub value_writable: bool,
    pub is_static_control: bool,
}

impl ElementInfo {
    pub fn collect(el: &IUIAutomationElement) -> Self {
        Self {
            hwnd: native_window(el),
            is_password: is_password(el),
            has_text_pattern: text_pattern(el).is_some(),
            value_writable: value_is_writable(el),
            is_static_control: is_static_control(el),
        }
    }
}

const GET_TEXT_UNLIMITED: i32 = -1;

/// Full text of the focused element. Password fields are never read.
pub fn read_text(el: &IUIAutomationElement) -> Result<String, UiaError> {
    if is_password(el) {
        return Err(UiaError::Password);
    }
    if let Some(tp) = text_pattern(el) {
        let doc = unsafe { tp.DocumentRange() }.map_err(win)?;
        return Ok(unsafe { doc.GetText(GET_TEXT_UNLIMITED) }.map_err(win)?.to_string());
    }
    if let Some(vp) = value_pattern(el) {
        return Ok(unsafe { vp.CurrentValue() }.map_err(win)?.to_string());
    }
    Err(UiaError::NoText)
}
