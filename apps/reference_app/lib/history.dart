// Text of earlier dictation sessions: one small text file per session in
// the app's data folder, the newest 50 kept. Text only, no audio.
// AGPL-3.0-or-later.

import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'ui_strings.dart';

class HistoryEntry {
  const HistoryEntry(this.file, this.when, this.preview);
  final File file;
  final DateTime when;
  final String preview;
}

class HistoryStore {
  HistoryStore(String dataDir) : dir = Directory('$dataDir${Platform.pathSeparator}history');

  final Directory dir;
  static const keep = 50;

  Future<void> add(String text) async {
    if (text.trim().isEmpty) return;
    try {
      await dir.create(recursive: true);
      final stamp = DateTime.now().toIso8601String().replaceAll(':', '-').split('.').first;
      await File('${dir.path}${Platform.pathSeparator}$stamp.txt').writeAsString(text);
      final all = await _files();
      for (final f in all.skip(keep)) {
        await f.delete();
      }
    } catch (_) {
      // History is a convenience; a full disk must not stop dictation.
    }
  }

  Future<List<File>> _files() async {
    if (!await dir.exists()) return const [];
    final files = await dir.list().where((e) => e is File && e.path.endsWith('.txt')).cast<File>().toList();
    files.sort((a, b) => b.path.compareTo(a.path));
    return files;
  }

  Future<List<HistoryEntry>> list() async {
    final out = <HistoryEntry>[];
    for (final f in await _files()) {
      String text;
      try {
        text = await f.readAsString();
      } catch (_) {
        continue;
      }
      final name = f.uri.pathSegments.last.replaceAll('.txt', '');
      final when = DateTime.tryParse(name.replaceFirstMapped(RegExp(r'T(\d\d)-(\d\d)-(\d\d)'), (m) => 'T${m[1]}:${m[2]}:${m[3]}')) ??
          (await f.lastModified());
      final line = text.trim().split('\n').first;
      out.add(HistoryEntry(f, when, line.length > 120 ? '${line.substring(0, 120)}…' : line));
    }
    return out;
  }

  Future<String> read(HistoryEntry e) => e.file.readAsString();

  Future<void> delete(HistoryEntry e) async {
    try {
      await e.file.delete();
    } catch (_) {}
  }

  Future<void> clear() async {
    for (final f in await _files()) {
      try {
        await f.delete();
      } catch (_) {}
    }
  }
}

/// The list of sessions; a tap opens the text, the trash empties the list.
class HistoryPane extends StatefulWidget {
  const HistoryPane({super.key, required this.store, this.embedded = false});

  final HistoryStore store;
  /// Inside the desktop shell (no own app bar).
  final bool embedded;

  @override
  State<HistoryPane> createState() => _HistoryPaneState();
}

class _HistoryPaneState extends State<HistoryPane> {
  late Future<List<HistoryEntry>> _entries = widget.store.list();

  void _reload() => setState(() => _entries = widget.store.list());

  String _when(DateTime d) {
    String two(int n) => n.toString().padLeft(2, '0');
    return '${d.year}-${two(d.month)}-${two(d.day)} ${two(d.hour)}:${two(d.minute)}';
  }

  Future<void> _open(HistoryEntry e) async {
    final t = UiStrings.current;
    final text = await widget.store.read(e);
    if (!mounted) return;
    await showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(_when(e.when)),
        content: SizedBox(
          width: 520,
          child: SingleChildScrollView(child: SelectableText(text)),
        ),
        actions: [
          TextButton.icon(
            onPressed: () async {
              await widget.store.delete(e);
              if (ctx.mounted) Navigator.pop(ctx);
              _reload();
            },
            icon: const Icon(Icons.delete_outline),
            label: Text(t['remove']),
          ),
          FilledButton.tonal(
            onPressed: () {
              Clipboard.setData(ClipboardData(text: text));
              ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(t['copied'])));
            },
            child: Text(t['copy']),
          ),
          TextButton(onPressed: () => Navigator.pop(ctx), child: Text(t['close'])),
        ],
      ),
    );
  }

  Future<void> _clear() async {
    final t = UiStrings.current;
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(t['historyClear']),
        content: Text(t['historyClearBody']),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx, false), child: Text(t['cancel'])),
          FilledButton(onPressed: () => Navigator.pop(ctx, true), child: Text(t['remove'])),
        ],
      ),
    );
    if (ok == true) {
      await widget.store.clear();
      _reload();
    }
  }

  @override
  Widget build(BuildContext context) {
    final t = UiStrings.current;
    return FutureBuilder<List<HistoryEntry>>(
      future: _entries,
      builder: (context, snap) {
        final entries = snap.data ?? const <HistoryEntry>[];
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 16, 8, 0),
              child: Row(children: [
                Expanded(child: Text(t['historyBody'], style: Theme.of(context).textTheme.bodyMedium)),
                IconButton(
                  tooltip: t['historyClear'],
                  onPressed: entries.isEmpty ? null : _clear,
                  icon: const Icon(Icons.delete_sweep_outlined),
                ),
              ]),
            ),
            Expanded(
              child: snap.connectionState != ConnectionState.done
                  ? const Center(child: CircularProgressIndicator())
                  : entries.isEmpty
                      ? Center(child: Text(t['historyEmpty']))
                      : ListView.separated(
                          padding: const EdgeInsets.all(8),
                          itemCount: entries.length,
                          separatorBuilder: (_, _) => const Divider(height: 1),
                          itemBuilder: (_, i) {
                            final e = entries[i];
                            return ListTile(
                              leading: const Icon(Icons.notes),
                              title: Text(e.preview, maxLines: 2, overflow: TextOverflow.ellipsis),
                              subtitle: Text(_when(e.when)),
                              onTap: () => _open(e),
                            );
                          },
                        ),
            ),
          ],
        );
      },
    );
  }
}
