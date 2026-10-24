// Platform-specific parts of the reference app (Windows).

import 'package:flutter/widgets.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';

/// Credential store for the API keys (Windows Credential Manager).
FlutterSecureStorage createSecureStorage() => const FlutterSecureStorage();

/// Null when the microphone may be used, otherwise a message for the user.
/// Windows has no runtime prompt; access is set in Settings > Privacy.
Future<String?> ensureMicPermission() async => null;

/// Extra settings card below the microphone settings, or null.
Widget? otherAppsCard() => null;
