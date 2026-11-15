// Background mode of the desktop shells (macOS, Windows): keep running
// when the window is closed, start at login. Spoken to over a method
// channel the native runner implements; a no-op elsewhere.
// AGPL-3.0-or-later.

import 'dart:io';

import 'package:flutter/services.dart';

class BackgroundControl {
  static const _channel = MethodChannel('ikk/background');

  static bool get supported => Platform.isMacOS || Platform.isWindows;

  /// Closing the window keeps the process (and the floating button) alive.
  static Future<void> setKeepRunning(bool on) async {
    if (!supported) return;
    try {
      await _channel.invokeMethod<void>('setKeepRunning', {'on': on});
    } catch (_) {}
  }

  /// Whether the app is registered to start with the session; null when
  /// the platform cannot say.
  static Future<bool?> isLoginItem() async {
    if (!supported) return null;
    try {
      return await _channel.invokeMethod<bool>('isLoginItem');
    } catch (_) {
      return null;
    }
  }

  /// Register or remove the login item. Returns the new state, or an error
  /// message.
  static Future<(bool?, String?)> setLoginItem(bool on) async {
    if (!supported) return (null, null);
    try {
      return (await _channel.invokeMethod<bool>('setLoginItem', {'on': on}), null);
    } on PlatformException catch (e) {
      return (null, e.message ?? e.code);
    } catch (e) {
      return (null, '$e');
    }
  }
}
