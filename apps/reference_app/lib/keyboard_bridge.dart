// iOS: the app's end of the line to the iKK keyboard extension. The
// keyboard cannot record, so it asks the app (start, stop, replace by
// voice, correct) and the app sends text, state and answers back through
// the App Group store. AGPL-3.0-or-later.
import 'dart:io';

import 'package:flutter/services.dart';

class KeyboardBridge {
  static const _channel = MethodChannel('ikk/keyboard');
  static bool get supported => Platform.isIOS;

  /// Commands from the keyboard: `start`, `stop`, `replace` (with
  /// `original`), `correct` (with `original`), `suggest` (with `original`,
  /// `before`, `after`), `choose` (with `original` = the chosen word), `open`.
  static void listen(void Function(String cmd, String original, Map<String, dynamic> all) onCommand) {
    if (!supported) return;
    _channel.setMethodCallHandler((call) async {
      if (call.method != 'command') return null;
      final m = Map<String, dynamic>.from(call.arguments as Map);
      onCommand(m['cmd'] as String? ?? '', m['original'] as String? ?? '', m);
      return null;
    });
    _channel.invokeMethod<void>('drain').catchError((_) {});
  }

  /// A dictated piece of text for the keyboard to insert.
  static Future<void> text(String t) => _send({'kind': 'text', 'text': t});

  /// The answer of a replacement or correction: goes over the taken-out text.
  static Future<void> replace(String t) => _send({'kind': 'replace', 'text': t});

  /// Word suggestions for the marked text (empty: none found).
  static Future<void> choices(List<String> items) => _send({'kind': 'choices', 'items': items});

  /// A replacement or correction is over.
  static Future<void> done(bool changed) => _send({'kind': 'done', 'changed': changed});

  static Future<void> status(String t) => _send({'kind': 'status', 'text': t});

  static Future<void> setListening(bool on) async {
    if (!supported) return;
    try {
      await _channel.invokeMethod<void>('setListening', {'on': on});
    } catch (_) {}
  }

  static Future<void> setCorrectionRange(String range) async {
    if (!supported) return;
    try {
      await _channel.invokeMethod<void>('setCorrectionRange', {'range': range});
    } catch (_) {}
  }

  /// Whether word suggestions can be asked for (the keyboard greys its
  /// start button otherwise while that range is chosen).
  static Future<void> setSuggestionsAvailable(bool on) async {
    if (!supported) return;
    try {
      await _channel.invokeMethod<void>('setSuggestionsAvailable', {'on': on});
    } catch (_) {}
  }

  static Future<void> _send(Map<String, Object?> item) async {
    if (!supported) return;
    try {
      await _channel.invokeMethod<void>('toKeyboard', item);
    } catch (_) {}
  }
}
