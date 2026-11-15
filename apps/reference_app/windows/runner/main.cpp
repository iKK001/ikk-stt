#include <flutter/dart_project.h>
#include <flutter/flutter_view_controller.h>
#include <windows.h>

#include <algorithm>

#include "flutter_window.h"
#include "utils.h"

int APIENTRY wWinMain(_In_ HINSTANCE instance, _In_opt_ HINSTANCE prev,
                      _In_ wchar_t *command_line, _In_ int show_command) {
  // Attach to console when present (e.g., 'flutter run') or create a
  // new console when running with a debugger.
  if (!::AttachConsole(ATTACH_PARENT_PROCESS) && ::IsDebuggerPresent()) {
    CreateAndAttachConsole();
  }

  // Initialize COM, so that it is available for use in the library and/or
  // plugins.
  ::CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);

  flutter::DartProject project(L"data");

  std::vector<std::string> command_line_arguments =
      GetCommandLineArguments();
  // Started with Windows (Run key): tray icon and floating button only.
  bool start_hidden =
      std::find(command_line_arguments.begin(), command_line_arguments.end(),
                "--hidden") != command_line_arguments.end();

  project.set_dart_entrypoint_arguments(std::move(command_line_arguments));

  FlutterWindow window(project);
  window.SetStartHidden(start_hidden);
  // Default size: 60 % of the work area's width, 90 % of its height,
  // centred. Win32Window scales logical sizes by the monitor's DPI, so the
  // physical work area is converted to logical pixels first.
  RECT work = {0, 0, 1920, 1080};
  ::SystemParametersInfoW(SPI_GETWORKAREA, 0, &work, 0);
  double scale = ::GetDpiForSystem() / 96.0;
  if (scale <= 0) scale = 1.0;
  int work_w = static_cast<int>((work.right - work.left) / scale);
  int work_h = static_cast<int>((work.bottom - work.top) / scale);
  int width = work_w * 6 / 10;
  int height = work_h * 9 / 10;
  Win32Window::Point origin(static_cast<unsigned int>(work.left / scale + (work_w - width) / 2),
                            static_cast<unsigned int>(work.top / scale + (work_h - height) / 2));
  Win32Window::Size size(static_cast<unsigned int>(width), static_cast<unsigned int>(height));
  if (!window.Create(L"reference_app", origin, size)) {
    return EXIT_FAILURE;
  }
  window.SetQuitOnClose(true);

  ::MSG msg;
  while (::GetMessage(&msg, nullptr, 0, 0)) {
    ::TranslateMessage(&msg);
    ::DispatchMessage(&msg);
  }

  ::CoUninitialize();
  return EXIT_SUCCESS;
}
