// Windows: the stored API keys follow the program when its name changes.
//
// The key store keeps its files in %APPDATA%\<company name>\<product name>,
// both names read from the program file's properties
// (windows/runner/Runner.rc). A new product name therefore means a new,
// empty folder. At start the files of every earlier name are moved over
// and the old folder is removed.
//
// The files' own names and the entry in the Windows Credential Manager
// that unlocks them start with STORAGE_PREFIX from windows/CMakeLists.txt.
// It is fixed there and must never change: files written under another
// prefix cannot be read again.

import 'dart:io';

import 'package:path_provider/path_provider.dart';

/// Every product name this program had before the current one. When the
/// product name in Runner.rc changes, add the old one here.
const earlierProductNames = ['reference_app'];

/// What two releases wrote before the prefix was fixed (they took the
/// program file's name for it): unreadable now, removed at start.
const _unreadablePrefix = 'ikk-stt_VGhpcyBpcyB0aGUgcHJlZml4IGZv_';

/// Move the key files of earlier names into the current folder and remove
/// what is left of the old folders. A file that exists under the current
/// name stays; the older one is left where it is. Returns how many moved.
Future<int> adoptEarlierKeys() async {
  if (!Platform.isWindows) return 0;
  var moved = 0;
  try {
    // The same folder the key store uses.
    final now = await getApplicationSupportDirectory();
    final sep = Platform.pathSeparator;
    await for (final f in now.list()) {
      if (f is File && f.path.split(sep).last.startsWith(_unreadablePrefix)) {
        try {
          await f.delete();
        } catch (_) {}
      }
    }
    for (final name in earlierProductNames) {
      final old = Directory('${now.parent.path}$sep$name');
      if (old.path == now.path || !await old.exists()) continue;
      // Whole files: one file holds several keys (the part of a key's
      // name after a colon is kept inside the file), so they are never
      // picked by their ending.
      await for (final f in old.list()) {
        if (f is! File) continue;
        final to = File('${now.path}$sep${f.path.split(sep).last}');
        try {
          if (!await to.exists()) {
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
