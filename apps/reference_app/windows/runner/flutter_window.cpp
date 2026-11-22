#include "flutter_window.h"

#include <flutter/standard_method_codec.h>
#include <shellapi.h>

#include <optional>
#include <string>

#include "flutter/generated_plugin_registrant.h"
#include "resource.h"

namespace {

// Tray icon callback and menu ids.
constexpr UINT kTrayMessage = WM_APP + 40;
constexpr UINT kTrayId = 1;
constexpr UINT kMenuOpen = 1001;
constexpr UINT kMenuQuit = 1002;

// Start-with-Windows entry (per user, no elevation).
const wchar_t kRunKey[] = L"Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const wchar_t kRunValue[] = L"ikk-stt";

bool IsLoginItem() {
  return ::RegGetValueW(HKEY_CURRENT_USER, kRunKey, kRunValue, RRF_RT_REG_SZ,
                        nullptr, nullptr, nullptr) == ERROR_SUCCESS;
}

// Registers "<exe> --hidden" under Run, or removes the entry.
bool SetLoginItem(bool on, std::string* error) {
  HKEY key = nullptr;
  LSTATUS status = ::RegCreateKeyExW(HKEY_CURRENT_USER, kRunKey, 0, nullptr, 0,
                                     KEY_SET_VALUE, nullptr, &key, nullptr);
  if (status != ERROR_SUCCESS) {
    *error = "registry key: " + std::to_string(status);
    return false;
  }
  if (on) {
    wchar_t exe[MAX_PATH];
    DWORD n = ::GetModuleFileNameW(nullptr, exe, MAX_PATH);
    if (n == 0 || n >= MAX_PATH) {
      ::RegCloseKey(key);
      *error = "executable path";
      return false;
    }
    std::wstring value = L"\"" + std::wstring(exe) + L"\" --hidden";
    status = ::RegSetValueExW(
        key, kRunValue, 0, REG_SZ,
        reinterpret_cast<const BYTE*>(value.c_str()),
        static_cast<DWORD>((value.size() + 1) * sizeof(wchar_t)));
  } else {
    status = ::RegDeleteValueW(key, kRunValue);
    if (status == ERROR_FILE_NOT_FOUND) {
      status = ERROR_SUCCESS;
    }
  }
  ::RegCloseKey(key);
  if (status != ERROR_SUCCESS) {
    *error = "registry value: " + std::to_string(status);
    return false;
  }
  return true;
}

bool BoolArg(const flutter::MethodCall<flutter::EncodableValue>& call,
             const char* name, bool fallback) {
  const auto* args = std::get_if<flutter::EncodableMap>(call.arguments());
  if (!args) return fallback;
  auto it = args->find(flutter::EncodableValue(name));
  if (it == args->end()) return fallback;
  const bool* b = std::get_if<bool>(&it->second);
  return b ? *b : fallback;
}

}  // namespace

FlutterWindow::FlutterWindow(const flutter::DartProject& project)
    : project_(project) {}

FlutterWindow::~FlutterWindow() {}

bool FlutterWindow::OnCreate() {
  if (!Win32Window::OnCreate()) {
    return false;
  }

  RECT frame = GetClientArea();

  // The size here must match the window dimensions to avoid unnecessary surface
  // creation / destruction in the startup path.
  flutter_controller_ = std::make_unique<flutter::FlutterViewController>(
      frame.right - frame.left, frame.bottom - frame.top, project_);
  // Ensure that basic setup of the controller was successful.
  if (!flutter_controller_->engine() || !flutter_controller_->view()) {
    return false;
  }
  RegisterPlugins(flutter_controller_->engine());
  SetChildContent(flutter_controller_->view()->GetNativeWindow());

  channel_ = std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
      flutter_controller_->engine()->messenger(), "ikk/background",
      &flutter::StandardMethodCodec::GetInstance());
  channel_->SetMethodCallHandler(
      [this](const auto& call, auto result) {
        HandleBackgroundCall(call, std::move(result));
      });
  InstallTray();

  flutter_controller_->engine()->SetNextFrameCallback([&]() {
    if (!start_hidden_) {
      this->Show();
    }
  });

  // Flutter can complete the first frame before the "show window" callback is
  // registered. The following call ensures a frame is pending to ensure the
  // window is shown. It is a no-op if the first frame hasn't completed yet.
  flutter_controller_->ForceRedraw();

  return true;
}

void FlutterWindow::OnDestroy() {
  RemoveTray();
  channel_ = nullptr;
  if (flutter_controller_) {
    flutter_controller_ = nullptr;
  }

  Win32Window::OnDestroy();
}

void FlutterWindow::HandleBackgroundCall(
    const flutter::MethodCall<flutter::EncodableValue>& call,
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  const std::string& method = call.method_name();
  if (method == "setKeepRunning") {
    keep_running_ = BoolArg(call, "on", false);
    result->Success();
  } else if (method == "showWindow") {
    ShowFromTray();
    result->Success();
  } else if (method == "isLoginItem") {
    result->Success(flutter::EncodableValue(IsLoginItem()));
  } else if (method == "setLoginItem") {
    std::string error;
    if (SetLoginItem(BoolArg(call, "on", false), &error)) {
      result->Success(flutter::EncodableValue(IsLoginItem()));
    } else {
      result->Error("login_item", error);
    }
  } else {
    result->NotImplemented();
  }
}

void FlutterWindow::InstallTray() {
  if (tray_installed_) return;
  NOTIFYICONDATAW nid = {};
  nid.cbSize = sizeof(nid);
  nid.hWnd = GetHandle();
  nid.uID = kTrayId;
  nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
  nid.uCallbackMessage = kTrayMessage;
  nid.hIcon = ::LoadIconW(::GetModuleHandleW(nullptr),
                          MAKEINTRESOURCEW(IDI_APP_ICON));
  wcscpy_s(nid.szTip, L"Dictation");
  tray_installed_ = ::Shell_NotifyIconW(NIM_ADD, &nid) == TRUE;
}

void FlutterWindow::RemoveTray() {
  if (!tray_installed_) return;
  NOTIFYICONDATAW nid = {};
  nid.cbSize = sizeof(nid);
  nid.hWnd = GetHandle();
  nid.uID = kTrayId;
  ::Shell_NotifyIconW(NIM_DELETE, &nid);
  tray_installed_ = false;
}

void FlutterWindow::ShowFromTray() {
  HWND hwnd = GetHandle();
  if (!hwnd) return;
  ::ShowWindow(hwnd, SW_SHOW);
  ::ShowWindow(hwnd, SW_RESTORE);
  ::SetForegroundWindow(hwnd);
}

void FlutterWindow::TrayMenu() {
  HWND hwnd = GetHandle();
  HMENU menu = ::CreatePopupMenu();
  if (!menu) return;
  ::AppendMenuW(menu, MF_STRING, kMenuOpen, L"Open");
  ::AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
  ::AppendMenuW(menu, MF_STRING, kMenuQuit, L"Quit");
  POINT pt;
  ::GetCursorPos(&pt);
  // Required so the menu closes when the user clicks elsewhere.
  ::SetForegroundWindow(hwnd);
  UINT chosen = ::TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x,
                                 pt.y, 0, hwnd, nullptr);
  ::DestroyMenu(menu);
  if (chosen == kMenuOpen) {
    ShowFromTray();
  } else if (chosen == kMenuQuit) {
    keep_running_ = false;
    ::DestroyWindow(hwnd);
  }
}

LRESULT
FlutterWindow::MessageHandler(HWND hwnd, UINT const message,
                              WPARAM const wparam,
                              LPARAM const lparam) noexcept {
  switch (message) {
    case WM_CLOSE:
      if (keep_running_) {
        // Hide; the process, the floating button and the voice commands
        // stay. Back via the tray icon.
        ::ShowWindow(hwnd, SW_HIDE);
        return 0;
      }
      break;
    case kTrayMessage:
      switch (LOWORD(lparam)) {
        case WM_LBUTTONUP:
        case WM_LBUTTONDBLCLK:
          ShowFromTray();
          return 0;
        case WM_RBUTTONUP:
        case WM_CONTEXTMENU:
          TrayMenu();
          return 0;
      }
      return 0;
  }

  // Give Flutter, including plugins, an opportunity to handle window messages.
  if (flutter_controller_) {
    std::optional<LRESULT> result =
        flutter_controller_->HandleTopLevelWindowProc(hwnd, message, wparam,
                                                      lparam);
    if (result) {
      return *result;
    }
  }

  switch (message) {
    case WM_FONTCHANGE:
      flutter_controller_->engine()->ReloadSystemFonts();
      break;
  }

  return Win32Window::MessageHandler(hwnd, message, wparam, lparam);
}
