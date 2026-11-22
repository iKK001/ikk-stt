// Windows: the stored API keys follow the program when its name changes.
//
// The key store keeps one encrypted file per key in
// %APPDATA%\<company name>\<product name>, both names read from the
// program file's properties (windows/runner/Runner.rc). A new product name
// therefore means a new, empty folder, and the keys entered under the old
// name stay behind unread. At start the keys of every earlier name are
// moved over and the old folder is removed.

import 'dart:io';

import 'package:path_provider/path_provider.dart';

/// Every product name this program had before the current one. When the
/// product name in Runner.rc changes, add the old one here.
const earlierProductNames = ['reference_app'];

/// Move the key files of earlier names into the current folder and remove
/// what is left of the old folders. A key that exists under the current
/// name wins; the older one is deleted. Returns how many keys moved.
Future<int> adoptEarlierKeys() async {
  if (!Platform.isWindows) return 0;
  var moved = 0;
  try {
    // The same folder the key store uses.
    final now = await getApplicationSupportDirectory();
    for (final name in earlierProductNames) {
      final old = Directory('${now.parent.path}${Platform.pathSeparator}$name');
      if (old.path == now.path || !await old.exists()) continue;
      await for (final f in old.list()) {
        if (f is! File || !f.path.endsWith('.secure')) continue;
        final to = File('${now.path}${Platform.pathSeparator}${f.uri.pathSegments.last}');
        try {
          if (await to.exists()) {
            await f.delete();
          } else {
            await f.rename(to.path);
            moved++;
          }
        } catch (_) {}
      }
      // Only when nothing else is in it.
      try {
        if (await old.list().isEmpty) await old.delete();
      } catch (_) {}
    }
  } catch (_) {}
  return moved;
}
