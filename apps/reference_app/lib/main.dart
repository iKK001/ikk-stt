// ikk-stt reference app. Thin shell over the Rust engine: settings in three
// areas (provider, sound, microphone), API key in the keychain, consent
// dialog before audio leaves the device, dictation into the focused app with
// the automatic fallback window. AGPL-3.0-or-later.

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:ikk_core_flutter/ikk_core_flutter.dart';
import 'package:path_provider/path_provider.dart';
import 'package:url_launcher/url_launcher.dart';

import 'theme.dart';
import 'app_name.dart';
import 'platform_extras.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  runApp(const IkkApp());
}

class IkkApp extends StatelessWidget {
  const IkkApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: appName,
      theme: ikkTheme(),
      home: const HomeScreen(),
      debugShowCheckedModeBanner: false,
    );
  }
}

const kLanguages = {
  'deu': 'Deutsch',
  'gsw': 'Schweizerdeutsch',
  'eng': 'English',
  'fra': 'Français',
  'ita': 'Italiano',
  '': 'Auto',
};

class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key});

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  final _engine = IkkEngine.create();
  final _secure = createSecureStorage();
  StreamSubscription<String>? _events;

  late String _settingsPath;
  late String _consentPath;
  // Plain file of earlier versions; moved into _voiceFile once.
  late String _voicePath;
  // The recorded voice commands, encrypted; the key is in the secure store.
  late String _voiceFile;
  late String _uiPath;
  Map<String, dynamic> _settings = {};
  List<String> _providers = [];
  List<InputDevice> _mics = [];
  final _keyCtl = TextEditingController();
  // Azure keys belong to a region (where the Speech resource was created).
  final _regionCtl = TextEditingController(text: 'switzerlandnorth');
  final _replayCtl = TextEditingController();
  final _vocabCtl = TextEditingController();
  final _vocabFocus = FocusNode();
  bool get _vocabFocused => _vocabFocus.hasFocus;
  bool _vocabDirty = false;
  Timer? _vocabTimer;
  static const _replayPrefix = 'file:';
  bool _keyStored = false;
  // Last four characters of the stored key, so the user can tell which key
  // it is; null when none is stored. Set when the secure store failed.
  String? _keyTail;
  String? _keyReadError;

  /// Where flutter_secure_storage keeps keys on this platform.
  static String get _keyStoreName {
    if (Platform.isMacOS || Platform.isIOS) return 'the Keychain';
    if (Platform.isAndroid) return 'the Android Keystore (encrypted)';
    if (Platform.isWindows) return 'Windows (encrypted, key in the Credential Manager)';
    return 'the secure store';
  }

  void _rememberKeyTail(String credential) {
    final key = _provider == 'azure' ? credential.substring(credential.indexOf(':') + 1) : credential;
    _keyTail = key.isEmpty ? null : key.substring(key.length > 4 ? key.length - 4 : 0);
  }

  String _state = 'Idle';
  double _level = -90;
  bool _speech = false;
  String _interim = '';
  final List<String> _finals = [];
  final _trainCtl = TextEditingController();
  // Tap-to-teach: selected stretch of the last recognised text.
  int? _teachFrom;
  int? _teachTo;
  int _teachFinals = -1;
  String? _fallbackReason;
  String _fallbackText = '';
  String? _error;
  String _suggestion = '';

  @override
  void initState() {
    super.initState();
    _vocabFocus.addListener(() {
      if (!_vocabFocus.hasFocus) _saveVocab();
    });
    _init();
  }

  Future<void> _init() async {
    final dir = configDir() ?? (await getApplicationSupportDirectory()).path;
    _settingsPath = '$dir${Platform.pathSeparator}settings.json';
    _consentPath = '$dir${Platform.pathSeparator}consent.json';
    _voicePath = '$dir${Platform.pathSeparator}voice_commands.json';
    // Windows: %LOCALAPPDATA%, so the recordings never roam to other PCs.
    _voiceFile = '${localDataDir() ?? dir}${Platform.pathSeparator}voice_commands.bin';
    _uiPath = '$dir${Platform.pathSeparator}ui.json';
    _settings = jsonDecode(await settingsLoad(path: _settingsPath)) as Map<String, dynamic>;
    _providers = about().providers;
    try {
      _engine.loadConsent(consentJson: await consentLoad(path: _consentPath));
    } catch (_) {}
    try {
      _mics = await listMics();
    } catch (_) {}
    await _loadKey();
    await _loadLtKey();
    _events = _engine.events().listen(_onEvent);
    _engine.rememberSettings(settingsJson: jsonEncode(_settings));
    await _loadVoice();
    await _loadFloating();
    if (mounted) setState(() {});
  }

  String get _provider => (_settings['providers']?['active'] as String?) ?? 'deepgram';

  Future<void> _loadKey() async {
    String k = '';
    _keyReadError = null;
    try {
      k = await _secure.read(key: 'api_key:$_provider') ?? '';
    } catch (e) {
      _keyReadError = '$e';
    }
    if (_provider == 'azure' && k.isNotEmpty) {
      final i = k.indexOf(':');
      if (i > 0) {
        _regionCtl.text = k.substring(0, i);
      } else {
        // Saved before the region field existed: add the region shown.
        k = '${_regionCtl.text.trim()}:$k';
      }
    }
    _keyStored = k.isNotEmpty;
    _keyTail = null;
    if (k.isNotEmpty) {
      _engine.setCredential(provider: _provider, secret: k);
      _rememberKeyTail(k);
    }
    _keyCtl.text = '';
  }

  Future<void> _removeKey() async {
    _engine.setCredential(provider: _provider, secret: '');
    try {
      await _secure.delete(key: 'api_key:$_provider');
      _snack('Key for $_provider removed');
    } catch (e) {
      _snack('Could not remove the key: $e');
    }
    setState(() {
      _keyStored = false;
      _keyTail = null;
    });
  }

  /// Azure: "REGION:KEY" from the two fields. A pasted "REGION:KEY" is
  /// kept; an empty key field with a stored key changes only the region.
  Future<String?> _azureCredential(String key) async {
    final region = _regionCtl.text.trim().toLowerCase();
    if (key.contains(':')) return key;
    if (!RegExp(r'^[a-z0-9]+$').hasMatch(region)) {
      _snack('Enter the Azure region, e.g. switzerlandnorth (Azure portal > Speech resource > Keys and Endpoint)');
      return null;
    }
    if (key.isEmpty) {
      String stored = '';
      try {
        stored = await _secure.read(key: 'api_key:azure') ?? '';
      } catch (_) {}
      final i = stored.indexOf(':');
      key = i >= 0 ? stored.substring(i + 1) : stored;
      if (key.isEmpty) return null;
    }
    return '$region:$key';
  }

  Future<void> _saveKey() async {
    var k = _keyCtl.text.trim();
    if (_provider == 'azure') {
      final c = await _azureCredential(k);
      if (c == null) return;
      k = c;
    }
    if (k.isEmpty) return;
    // The engine gets the key first; keychain storage is best effort.
    _engine.setCredential(provider: _provider, secret: k);
    try {
      await _secure.write(key: 'api_key:$_provider', value: k);
      _keyReadError = null;
      _snack('Key saved in $_keyStoreName');
    } catch (e) {
      _snack('Key set for this run only; saving failed: $e');
    }
    setState(() {
      _keyStored = true;
      _rememberKeyTail(k);
      _keyCtl.text = '';
    });
  }

  Future<void> _saveSettings() async {
    final json = jsonEncode(_settings);
    try {
      await settingsSave(path: _settingsPath, json: json);
      _engine.rememberSettings(settingsJson: json);
    } catch (e) {
      _snack('Settings not saved: $e');
      return;
    }
    _applyVoice();
    setState(() {});
  }

  void _onEvent(String raw) {
    final ev = jsonDecode(raw);
    if (ev is String) {
      if (ev == 'PermissionRequired') _snack('Permission needed: allow keyboard access for this app (Accessibility).');
      return;
    }
    final m = ev as Map<String, dynamic>;
    final key = m.keys.first;
    final v = m[key];
    setState(() {
      switch (key) {
        case 'Level':
          _level = (v as num).toDouble();
        case 'State':
          _state = v as String;
          if (_state == 'Idle') {
            _interim = '';
            _speech = false;
          }
        case 'Speech':
          _speech = v as bool;
        case 'Transcript':
          final t = v as Map<String, dynamic>;
          if (t.containsKey('Interim')) _interim = t['Interim'] as String;
          if (t.containsKey('Final')) {
            _interim = '';
            _finals.add(t['Final'] as String);
            // Test builds: the typed text in the device log (adb logcat, tag flutter).
            if (kDebugMode) debugPrint('ikk final [${DateTime.now().toIso8601String()}]: ${t['Final']}');
            if (_fallbackReason != null) _fallbackText = '${_fallbackText.trimRight()} ${t['Final']}'.trim();
          }
          if (t.containsKey('Replace')) {
            // Live correction or batch pass: the text so far, corrected.
            _finals
              ..clear()
              ..add(t['Replace'] as String);
          }
          if (t.containsKey('Error')) _error = 'Provider: ${t['Error']}';
        case 'ConsentRequired':
          _askConsent(v as Map<String, dynamic>);
        case 'VoiceStandby':
          _voiceStandby = v as bool;
        case 'VoiceHeard':
          _voiceHeard = _describeHeard(v as Map<String, dynamic>);
        case 'VoiceStandbyLevel':
          _standbyLevel = (v as num).toDouble();
        case 'VoiceCommand':
          _snack(v == 'Start' ? 'Voice command: start' : 'Voice command: stop');
        case 'SetupRequired':
          _askSetup(v as Map<String, dynamic>);
        case 'ConsentChanged':
          consentSave(path: _consentPath, json: jsonEncode(v));
        case 'FallbackRequired':
          final f = v as Map<String, dynamic>;
          _fallbackReason = f['reason'].toString();
          _fallbackText = f['carried_text'] as String;
        case 'Transferred':
          _fallbackReason = null;
          _fallbackText = '';
          _snack('Inserted into target');
        case 'Error':
          _error = v as String;
        case 'Suggestion':
          _suggestion = v as String;
        case 'Command':
          _snack('Command: $v');
        default:
          break;
      }
    });
  }

  Future<void> _askConsent(Map<String, dynamic> r) async {
    final usesList = (r['uses'] as List).cast<String>();
    final uses = usesList.join(', ');
    // A text checker gets the dictated text, not the recordings.
    final textOnly = usesList.every((u) => u == 'Correction');
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(textOnly ? 'Send text to ${r['name']}?' : 'Send audio to ${r['name']}?'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(textOnly
                ? 'Your dictated text leaves this device and is checked by ${r['name']} for: $uses.'
                : 'Your recordings leave this device and are processed by ${r['name']} for: $uses.'),
            const SizedBox(height: 12),
            TextButton(
              onPressed: () => launchUrl(Uri.parse(r['privacy_url'] as String)),
              child: const Text('Privacy policy'),
            ),
          ],
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx, false), child: const Text('Deny')),
          FilledButton(onPressed: () => Navigator.pop(ctx, true), child: const Text('Allow')),
        ],
      ),
    );
    if (ok == true) {
      _engine.grantConsent(provider: r['id'] as String);
      _start();
    } else {
      _engine.denyConsent(provider: r['id'] as String);
    }
  }

  // ------------------------------------------------ floating button

  final bool _floatingSupported = floatingButtonSupported();
  bool _floatingOn = true;

  Future<void> _loadFloating() async {
    if (!_floatingSupported) return;
    try {
      final f = File(_uiPath);
      if (await f.exists()) {
        _floatingOn = (jsonDecode(await f.readAsString()) as Map<String, dynamic>)['floating_button'] as bool? ?? true;
      }
    } catch (_) {}
    _engine.showFloatingButton(show_: _floatingOn);
  }

  Future<void> _setFloating(bool on) async {
    _floatingOn = on;
    _engine.showFloatingButton(show_: on);
    try {
      await File(_uiPath).writeAsString(jsonEncode({'floating_button': on}));
    } catch (_) {}
    setState(() {});
  }

  Widget _floatingSection() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: SwitchListTile(
          contentPadding: EdgeInsets.zero,
          title: const Text('Floating microphone button'),
          subtitle: const Text('Stays on top of all apps. Click it to start or stop dictating into the app '
              'you are working in; drag it to move it. Red while listening.'),
          value: _floatingOn,
          onChanged: _setFloating,
        ),
      ),
    );
  }

  // ------------------------------------------------ offline voice commands

  String? _voiceModel;
  bool _voiceOn = false;
  bool _voiceStandby = false;
  // Diagnosis: the listener's last utterance and its loudest recent level.
  String? _voiceHeard;
  double? _standbyLevel;

  String _describeHeard(Map<String, dynamic> e) {
    final j = e['judgement'] as Map<String, dynamic>;
    String d(Object? v) => v == null ? 'length off' : (v as num).toStringAsFixed(1);
    final result = switch (j['command']) { 'Start' => 'START', 'Stop' => 'STOP', _ => 'nothing' };
    return '${e['standby'] == true ? 'While waiting' : 'While dictating'}: '
        '${(j['secs'] as num).toStringAsFixed(2)} s, ${(j['level_dbfs'] as num).toStringAsFixed(0)} dBFS, '
        'start ${d(j['start'])} (limit ${(j['start_threshold'] as num).toStringAsFixed(1)}), '
        'stop ${d(j['stop'])} (limit ${(j['stop_threshold'] as num).toStringAsFixed(1)}) -> $result';
  }

  /// Key of the encrypted voice commands file, created on first use.
  Future<String> _voiceKey() async {
    const name = 'file_key:voice_commands';
    final stored = await _secure.read(key: name);
    if (stored != null && stored.isNotEmpty) return stored;
    final k = sealedNewKey();
    await _secure.write(key: name, value: k);
    return k;
  }

  Future<void> _loadVoice() async {
    try {
      String? json;
      final legacy = File(_voicePath);
      if (await File(_voiceFile).exists()) {
        json = await sealedLoad(path: _voiceFile, keyHex: await _voiceKey());
      } else if (await legacy.exists()) {
        json = await legacy.readAsString();
      }
      if (json == null) return;
      final j = jsonDecode(json) as Map<String, dynamic>;
      _voiceModel = j['model'] as String?;
      _voiceOn = (j['enabled'] as bool? ?? false) && _voiceModel != null;
      _applyVoice();
    } catch (e) {
      _snack('Voice commands could not be read ($e). Please record them again.');
      return;
    }
    // A plain file of an earlier version: encrypt it, then remove it. If
    // that fails, the plain file stays and is tried again next start.
    try {
      final legacy = File(_voicePath);
      if (await legacy.exists()) {
        await _saveVoice();
        await legacy.delete();
      }
    } catch (_) {}
  }

  Future<void> _saveVoice() async {
    final json = jsonEncode({'enabled': _voiceOn, 'model': _voiceModel});
    await sealedSave(path: _voiceFile, keyHex: await _voiceKey(), plain: json);
  }

  /// Tell the engine; a voice start dictates with the current settings.
  void _applyVoice() {
    try {
      _engine.setVoiceCommands(
        commandsJson: _voiceOn ? _voiceModel : null,
        settingsJson: jsonEncode(_settings),
      );
    } catch (e) {
      _snack('Voice commands: $e');
    }
    if (!_voiceOn) _voiceStandby = false;
  }

  /// Guided recording: three takes of the start word, then three of the
  /// stop word. Every take is confirmed or repeated with a clear message;
  /// each word is checked right after its takes.
  Future<void> _trainVoice() async {
    final denied = await ensureMicPermission();
    if (denied != null) {
      _snack(denied);
      return;
    }
    // Nothing may listen or dictate while the words are recorded: a
    // running dictation would transcribe them (and type them somewhere).
    _engine.setVoiceTraining(active: true);
    try {
      final step = ValueNotifier<_VoiceStep>(const _VoiceStep('Get ready …', _StepKind.info));
      var cancelled = false;
      var detail = '';
      if (!mounted) return;
      showDialog<void>(
        context: context,
        barrierDismissible: false,
        builder: (ctx) => AlertDialog(
          title: const Text('Record your voice commands'),
          content: ValueListenableBuilder<_VoiceStep>(
            valueListenable: step,
            builder: (_, st, _) => Column(mainAxisSize: MainAxisSize.min, children: [
              Icon(st.icon, size: 48, color: st.color(Theme.of(ctx).colorScheme)),
              const SizedBox(height: 12),
              Text(st.text, style: Theme.of(ctx).textTheme.titleMedium, textAlign: TextAlign.center),
              const SizedBox(height: 16),
              if (st.kind == _StepKind.listening) const LinearProgressIndicator(),
            ]),
          ),
          actions: [
            TextButton(
              onPressed: () {
                cancelled = true;
                Navigator.of(ctx).pop();
              },
              child: const Text('Close'),
            ),
          ],
        ),
      );
      Future<void> show(String text, _StepKind kind, [int ms = 0]) async {
        step.value = _VoiceStep(text, kind);
        if (ms > 0) await Future<void>.delayed(Duration(milliseconds: ms));
      }

      final takes = <String, List<Float32List>>{'start': [], 'stop': []};
      for (final word in ['start', 'stop']) {
        final label = word == 'start' ? 'START' : 'STOP';
        if (word == 'stop') {
          await show('✓ START word learned.\nNext: your STOP word – a different word.', _StepKind.ok, 2500);
        }
        var retry = false;
        while (!cancelled && takes[word]!.length < 3) {
          final n = takes[word]!.length + 1;
          await show(
            retry
                ? 'Didn\'t catch that clearly ($detail).\nPlease say your $label word again ($n of 3).'
                : n == 1
                    ? 'Say your $label word now ($n of 3).'
                    : 'Say your $label word once more ($n of 3).',
            retry ? _StepKind.retry : _StepKind.listening,
          );
          try {
            final take = await voiceRecordTake(settingsJson: jsonEncode(_settings), timeoutSecs: 8);
            if (cancelled) break;
            takes[word]!.add(take);
            retry = false;
            await show('✓ Understood ($n of 3).', _StepKind.ok, 1300);
          } catch (e) {
            if (cancelled) break;
            retry = true;
            detail = '$e'.replaceFirst(RegExp(r'^.*?(nothing heard|heard something)'), r'$1');
            await show('Didn\'t catch that clearly.\n($detail)', _StepKind.retry, 2000);
          }
          if (!cancelled && takes[word]!.length == 3) {
            try {
              await voiceCheckWord(takes: takes[word]!);
            } catch (_) {
              takes[word]!.clear();
              retry = false;
              await show('Those three sounded too different.\nLet\'s record your $label word again.', _StepKind.retry, 2500);
            }
          }
        }
        if (cancelled) break;
      }
      if (cancelled) {
        _applyVoice();
        return;
      }
      await show('Learning your words …', _StepKind.listening);
      try {
        _voiceModel = await voiceTrain(startTakes: takes['start']!, stopTakes: takes['stop']!);
        _voiceOn = true;
        await _saveVoice();
        await show('✓ Done. Say your START word to dictate,\nyour STOP word to stop.', _StepKind.ok, 2500);
        if (mounted && !cancelled) Navigator.of(context).pop();
      } catch (e) {
        // Stays visible until closed: the reason matters for the next try.
        final msg = '$e'.replaceFirst(RegExp(r'^.*?Error:\s*'), '');
        await show('Could not learn your words:\n$msg\n\nClose and record them again.', _StepKind.retry);
      }
      _applyVoice();
      setState(() {});
    } finally {
      _engine.setVoiceTraining(active: false);
    }
  }

  Widget _voiceSection() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Voice commands (offline)', style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 4),
            const Text('Start and stop dictation with two words of your own. They are recorded in your '
                'voice and recognised on this device only; nothing is sent anywhere.'),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Start and stop by voice'),
              subtitle: Text(_voiceModel == null
                  ? 'Record your words first'
                  : _voiceStandby
                      ? 'Listening for your start word'
                      : 'Ready'),
              value: _voiceOn,
              onChanged: _voiceModel == null
                  ? null
                  : (on) async {
                      _voiceOn = on;
                      await _saveVoice();
                      _applyVoice();
                      setState(() {});
                    },
            ),
            // Diagnosis for tuning, updated every few seconds: not for
            // screen readers.
            if (_voiceOn && (_voiceHeard != null || _standbyLevel != null))
              Padding(
                padding: const EdgeInsets.only(bottom: 8),
                child: ExcludeSemantics(
                  child: Text(
                  [
                    if (_standbyLevel != null) 'Waiting listener level: ${_standbyLevel!.toStringAsFixed(0)} dBFS',
                    if (_voiceHeard != null) 'Last heard. $_voiceHeard',
                  ].join('\n'),
                  style: Theme.of(context).textTheme.bodySmall,
                  ),
                ),
              ),
            OutlinedButton(
              onPressed: _trainVoice,
              child: Text(_voiceModel == null ? 'Record start and stop word' : 'Record them again'),
            ),
          ],
        ),
      ),
    );
  }

  /// The provider needs a setting outside the app (asked after consent).
  Future<void> _askSetup(Map<String, dynamic> s) async {
    final url = s['settings_url'] as String? ?? '';
    final retry = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('One more setting needed'),
        content: Text(s['message'] as String? ?? ''),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx, false), child: const Text('Cancel')),
          if (url.isNotEmpty)
            TextButton(
              onPressed: () => launchUrl(Uri.parse(url)),
              child: const Text('Open settings'),
            ),
          FilledButton(onPressed: () => Navigator.pop(ctx, true), child: const Text('Done, start')),
        ],
      ),
    );
    if (retry == true) _start();
  }

  Future<void> _start() async {
    final denied = await ensureMicPermission();
    if (denied != null) {
      setState(() => _error = denied);
      return;
    }
    setState(() {
      _error = null;
      _finals.clear();
      _suggestion = '';
      _fallbackReason = null;
      _fallbackText = '';
    });
    try {
      _engine.start(settingsJson: jsonEncode(_settings));
    } catch (e) {
      setState(() => _error = '$e');
    }
  }

  void _stop() => _engine.stop();

  void _snack(String msg) {
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(msg)));
  }

  @override
  void dispose() {
    _events?.cancel();
    _keyCtl.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final listening = _state == 'Listening' || _state == 'Finishing';
    if (_settings.isEmpty) return const Scaffold(body: Center(child: CircularProgressIndicator()));
    return Scaffold(
      appBar: AppBar(
        title: const Text(appName),
        actions: [
          IconButton(icon: const Icon(Icons.info_outline), onPressed: () => _showAbout(context)),
        ],
      ),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          _voiceModeSection(),
          const SizedBox(height: 16),
          _providerSection(),
          _trainingSection(),
          const SizedBox(height: 16),
          _correctionSection(),
          const SizedBox(height: 16),
          _soundSection(),
          const SizedBox(height: 16),
          _micSection(),
          const SizedBox(height: 16),
          _voiceSection(),
          if (_floatingSupported) ...[const SizedBox(height: 16), _floatingSection()],
          if (otherAppsCard() case final card?) ...[const SizedBox(height: 16), card],
          const SizedBox(height: 24),
          Center(
            child: FilledButton.icon(
              style: FilledButton.styleFrom(minimumSize: const Size(220, 64)),
              onPressed: listening ? _stop : _start,
              icon: Icon(listening ? Icons.stop : Icons.mic),
              label: Text(listening ? 'Stop' : 'Dictate', style: const TextStyle(fontSize: 20)),
            ),
          ),
          const SizedBox(height: 6),
          // What the next session will actually use, so stale settings
          // never go unnoticed.
          Center(
            child: Text(
              _activeSummary(),
              textAlign: TextAlign.center,
              style: Theme.of(context).textTheme.bodySmall,
            ),
          ),
          const SizedBox(height: 8),
          // What changes during dictation (status, level, interim text, the
          // growing list of sentences) sits behind one fixed accessibility
          // node per area whose label changes: nodes appearing, vanishing
          // and shifting several times a second made Flutter's Windows
          // accessibility bridge log "Failed to update ui::AXTree" lines.
          Semantics(
            container: true,
            label: 'Status: $_state',
            child: ExcludeSemantics(child: Center(child: Text('$_state${_speech ? ' · speech' : ''}'))),
          ),
          const SizedBox(height: 8),
          ExcludeSemantics(child: LinearProgressIndicator(value: ((_level + 60) / 60).clamp(0.0, 1.0))),
          const SizedBox(height: 12),
          if (_error != null) Text(_error!, style: TextStyle(color: Theme.of(context).colorScheme.error)),
          if (_fallbackReason != null) _fallbackCard(),
          Semantics(
            container: true,
            label: _finals.isEmpty ? 'No text recognised yet' : 'Last recognised: ${_finals.last}',
            child: ExcludeSemantics(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  if (_interim.isNotEmpty) Text(_interim, style: const TextStyle(fontStyle: FontStyle.italic)),
                  const SizedBox(height: 12),
                  ..._finals.reversed
                      .take(20)
                      .map((t) => Padding(padding: const EdgeInsets.symmetric(vertical: 2), child: Text(t))),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }

  /// The training-mode settings (`text.training`), created when missing.
  Map<String, dynamic> _training() {
    final text = (_settings['text'] ??= <String, dynamic>{}) as Map<String, dynamic>;
    final t = (text['training'] ??= <String, dynamic>{'enabled': true, 'words': <dynamic>[]}) as Map<String, dynamic>;
    t['words'] ??= <dynamic>[];
    t['enabled'] ??= true;
    return t;
  }

  static String _normWord(String s) => s.toLowerCase().replaceAll(RegExp(r'[^\p{L}\p{N}]', unicode: true), '');

  /// Store what the recogniser heard for [word] (nothing when it heard
  /// the word itself).
  void _learn(String word, String heard) {
    final words = _training()['words'] as List;
    var entry = words.cast<Map<String, dynamic>>().where((e) => _normWord(e['word'] as String) == _normWord(word)).firstOrNull;
    if (entry == null) {
      entry = <String, dynamic>{'word': word, 'heard': <dynamic>[], 'always': false};
      words.add(entry);
    }
    final h = heard.trim().replaceAll(RegExp(r'[.,!?;:]+$'), '');
    if (h.isEmpty || _normWord(h) == _normWord(word)) return;
    final forms = (entry['heard'] ??= <dynamic>[]) as List;
    final same = forms.cast<Map<String, dynamic>>().where((f) => _normWord(f['text'] as String) == _normWord(h)).firstOrNull;
    if (same != null) {
      same['count'] = ((same['count'] as num?) ?? 0) + 1;
    } else {
      forms.add(<String, dynamic>{'text': h, 'count': 1});
    }
  }

  /// Say [word] [takes] times; each take goes to the recogniser without
  /// hints and the answers become the word's heard forms.
  Future<void> _trainWord(String word, {int takes = 5}) async {
    if (word.isEmpty) return;
    String key = '';
    try {
      key = await _secure.read(key: 'api_key:$_provider') ?? '';
    } catch (_) {}
    if (key.isEmpty) {
      _snack('No API key for $_provider – enter it first.');
      return;
    }
    final denied = await ensureMicPermission();
    if (denied != null) {
      _snack(denied);
      return;
    }
    // Nothing may dictate while the takes are recorded.
    _engine.setVoiceTraining(active: true);
    final step = ValueNotifier<_VoiceStep>(_VoiceStep('Get ready to say «$word» …', _StepKind.info));
    var cancelled = false;
    final heardAll = <String>[];
    if (!mounted) return;
    showDialog<void>(
      context: context,
      barrierDismissible: false,
      builder: (ctx) => AlertDialog(
        title: Text('Training «$word»'),
        content: ValueListenableBuilder<_VoiceStep>(
          valueListenable: step,
          builder: (_, st, _) => Column(mainAxisSize: MainAxisSize.min, children: [
            Icon(st.icon, size: 48, color: st.color(Theme.of(ctx).colorScheme)),
            const SizedBox(height: 12),
            Text(st.text, style: Theme.of(ctx).textTheme.titleMedium, textAlign: TextAlign.center),
            const SizedBox(height: 16),
            if (st.kind == _StepKind.listening) const LinearProgressIndicator(),
          ]),
        ),
        actions: [
          TextButton(
            onPressed: () {
              cancelled = true;
              Navigator.of(ctx).pop();
            },
            child: const Text('Close'),
          ),
        ],
      ),
    ).then((_) => cancelled = true);
    Future<void> show(String text, _StepKind kind, [int ms = 0]) async {
      step.value = _VoiceStep(text, kind);
      if (ms > 0) await Future<void>.delayed(Duration(milliseconds: ms));
    }

    try {
      await show('Get ready …', _StepKind.info, 1200);
      var n = 1;
      var failures = 0;
      while (!cancelled && n <= takes) {
        await show('Say «$word» now ($n of $takes).', _StepKind.listening);
        try {
          final heard = await trainingTake(settingsJson: jsonEncode(_settings), credential: key, timeoutSecs: 10);
          if (cancelled) break;
          heardAll.add(heard);
          _learn(word, heard);
          await show(heard.isEmpty ? 'Heard nothing in that take.' : 'Heard: „$heard“ ($n of $takes)', _StepKind.ok, 1400);
          n++;
        } catch (e) {
          if (cancelled) break;
          failures++;
          await show('Didn\'t catch that.\n($e)', _StepKind.retry, 1800);
          if (failures >= 3) break;
        }
      }
      if (heardAll.isNotEmpty) {
        await _saveSettings();
        final forms = heardAll.where((h) => h.isNotEmpty && _normWord(h) != _normWord(word)).toSet();
        final hits = heardAll.where((h) => _normWord(h) == _normWord(word)).length;
        await show(
          forms.isEmpty
              ? '✓ «$word» was recognised $hits of ${heardAll.length} times as it is.'
              : '✓ Learned. «$word» is heard as:\n${forms.join(', ')}\n\nFrom now on these are written as «$word».',
          _StepKind.ok,
          3500,
        );
      }
    } finally {
      _engine.setVoiceTraining(active: false);
      if (!cancelled && mounted) Navigator.of(context, rootNavigator: true).pop();
      _applyVoice();
      if (mounted) setState(() {});
    }
  }

  /// Teach from the last recognised text: tap the first and the last wrong
  /// word, enter (or pick) the right one, Teach. The tapped stretch becomes
  /// a heard form of the word — as it comes back inside a sentence, which
  /// differs from the word said alone.
  List<Widget> _teachFromText(List<Map<String, dynamic>> learned) {
    if (_finals.isEmpty) return const [];
    if (_teachFinals != _finals.length) {
      _teachFinals = _finals.length;
      _teachFrom = _teachTo = null;
    }
    final all = _finals.join(' ').split(RegExp(r'\s+')).where((w) => w.isNotEmpty).toList();
    final words = all.length > 40 ? all.sublist(all.length - 40) : all;
    final from = _teachFrom, to = _teachTo;
    final selected = from == null || to == null ? '' : words.sublist(from, to + 1).join(' ');
    return [
      const SizedBox(height: 8),
      Text('Teach from the last text', style: Theme.of(context).textTheme.titleSmall),
      const Text('Tap the first and the last wrong word, then enter the right word above and press Teach.'),
      Wrap(
        spacing: 4,
        runSpacing: 0,
        children: [
          for (var i = 0; i < words.length; i++)
            FilterChip(
              label: Text(words[i]),
              selected: from != null && to != null && i >= from && i <= to,
              onSelected: (_) => setState(() {
                if (from == null || to == null || i < from || (from != to)) {
                  _teachFrom = _teachTo = i;
                } else {
                  _teachTo = i;
                }
              }),
            ),
        ],
      ),
      if (selected.isNotEmpty)
        Row(children: [
          Expanded(child: Text('„$selected“ → «${_trainCtl.text.trim().isEmpty ? '…' : _trainCtl.text.trim()}»')),
          FilledButton.tonal(
            onPressed: _trainCtl.text.trim().isEmpty
                ? null
                : () {
                    final word = _trainCtl.text.trim();
                    _learn(word, selected);
                    _saveSettings();
                    _snack('«$word» is now written for „$selected“');
                    setState(() => _teachFrom = _teachTo = null);
                  },
            child: const Text('Teach'),
          ),
        ]),
      if (selected.isNotEmpty && learned.isNotEmpty)
        Wrap(
          spacing: 4,
          children: [
            for (final e in learned)
              ActionChip(
                label: Text(e['word'] as String),
                onPressed: () => setState(() => _trainCtl.text = e['word'] as String),
              ),
          ],
        ),
    ];
  }

  Widget _trainingSection() {
    final t = _training();
    final words = (t['words'] as List).cast<Map<String, dynamic>>();
    final hasKey = _keyTail != null;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Training', style: Theme.of(context).textTheme.titleMedium),
            const Text(
              'A word the recogniser never gets right: enter it once, say it five times. What the recogniser hears '
              'for it is remembered, and written as the word wherever the recogniser was unsure. A similar word it '
              'hears clearly stays as it is. Stays on this device.',
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Use learned words'),
              value: t['enabled'] as bool? ?? true,
              onChanged: (b) {
                t['enabled'] = b;
                _saveSettings();
                setState(() {});
              },
            ),
            Row(children: [
              Expanded(
                child: TextField(
                  controller: _trainCtl,
                  decoration: const InputDecoration(labelText: 'Word to train', hintText: 'e.g. Bildschirmtastatur'),
                  onChanged: (_) => setState(() {}),
                  onSubmitted: (_) => hasKey ? _trainWord(_trainCtl.text.trim()) : null,
                ),
              ),
              const SizedBox(width: 8),
              FilledButton.icon(
                onPressed: hasKey && _state != 'Listening' ? () => _trainWord(_trainCtl.text.trim()) : null,
                icon: const Icon(Icons.mic),
                label: const Text('Say it 5×'),
              ),
            ]),
            if (!hasKey) const Padding(padding: EdgeInsets.only(top: 4), child: Text('Enter the API key of the recogniser first.')),
            ..._teachFromText(words),
            for (final e in words)
              ExpansionTile(
                tilePadding: EdgeInsets.zero,
                title: Text(e['word'] as String),
                subtitle: Text(
                  ((e['heard'] as List?) ?? const []).isEmpty
                      ? 'recognised as it is so far'
                      : 'heard as: ${((e['heard'] as List).cast<Map<String, dynamic>>()).map((f) => '${f['text']} ×${f['count'] ?? 1}').join(', ')}',
                ),
                children: [
                  SwitchListTile(
                    contentPadding: EdgeInsets.zero,
                    title: const Text('Replace even when heard clearly'),
                    subtitle: const Text('Off: a heard form is replaced only where the recogniser was unsure, so a similar real word '
                        'still gets through. On: replaced always – only for a word whose mishearings are never real words.'),
                    value: e['always'] as bool? ?? false,
                    onChanged: (b) {
                      e['always'] = b;
                      _saveSettings();
                      setState(() {});
                    },
                  ),
                  Row(children: [
                    TextButton.icon(
                      onPressed: hasKey && _state != 'Listening' ? () => _trainWord(e['word'] as String) : null,
                      icon: const Icon(Icons.mic),
                      label: const Text('Say it again'),
                    ),
                    TextButton.icon(
                      onPressed: () {
                        (t['words'] as List).remove(e);
                        _saveSettings();
                        setState(() {});
                      },
                      icon: const Icon(Icons.delete_outline),
                      label: const Text('Remove'),
                    ),
                  ]),
                ],
              ),
          ],
        ),
      ),
    );
  }

  Widget _providerSection() {
    final opts = (_settings['providers']['options'] as Map<String, dynamic>)[_provider] as Map<String, dynamic>? ?? {};
    final batch = (opts['transfer'] ?? 'Batch') == 'Batch';
    final vocab = ((opts['keywords'] as List?) ?? const []).join(', ');
    if (_vocabCtl.text != vocab && !_vocabFocused) _vocabCtl.text = vocab;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Speech recognition', style: Theme.of(context).textTheme.titleMedium),
            DropdownButton<String>(
              value: _providers.contains(_provider) ? _provider : null,
              isExpanded: true,
              items: _providers.map((p) => DropdownMenuItem(value: p, child: Text(p))).toList(),
              onChanged: (p) async {
                if (p == null) return;
                _settings['providers']['active'] = p;
                await _saveSettings();
                await _loadKey();
                setState(() {});
              },
            ),
            DropdownButton<String>(
              value: _settings['providers']['language'] as String,
              isExpanded: true,
              items: kLanguages.entries.map((e) => DropdownMenuItem(value: e.key, child: Text(e.value))).toList(),
              onChanged: (l) {
                _settings['providers']['language'] = l;
                _saveSettings();
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Batch upload'),
              subtitle: const Text('Chunks cut between words instead of a live stream: more accurate, a little later. Needed for slow or unclear speech.'),
              value: batch,
              onChanged: (b) {
                final o = Map<String, dynamic>.from(opts);
                o['transfer'] = b ? 'Batch' : 'Streaming';
                (_settings['providers']['options'] as Map<String, dynamic>)[_provider] = o;
                _saveSettings();
              },
            ),
            TextField(
              controller: _vocabCtl,
              focusNode: _vocabFocus,
              decoration: const InputDecoration(
                labelText: 'Personal vocabulary',
                helperText: 'Names, places, special words, separated by commas. The recogniser is told to expect them '
                    '(OpenAI prompt, Deepgram keywords).',
              ),
              onChanged: (v) {
                final o = Map<String, dynamic>.from(opts);
                o['keywords'] = v.split(',').map((w) => w.trim()).where((w) => w.isNotEmpty).toList();
                (_settings['providers']['options'] as Map<String, dynamic>)[_provider] = o;
                _vocabDirty = true;
                // Saved while typing, so no Enter or click elsewhere is needed.
                _vocabTimer?.cancel();
                _vocabTimer = Timer(const Duration(milliseconds: 600), _saveVocab);
              },
              onSubmitted: (_) => _saveVocab(),
              onEditingComplete: _saveVocab,
              onTapOutside: (_) {
                _vocabFocus.unfocus();
                _saveVocab();
              },
            ),
            _keyStatus(),
            if (_provider == 'azure')
              TextField(
                controller: _regionCtl,
                decoration: const InputDecoration(
                  labelText: 'Azure region',
                  helperText: 'Where your Speech resource is, e.g. switzerlandnorth or westeurope '
                      '(Azure portal > Speech resource > Keys and Endpoint > Location/Region)',
                ),
              ),
            Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _keyCtl,
                    obscureText: true,
                    decoration: InputDecoration(
                      labelText: _keyStored ? 'New API key (replaces the stored one)' : 'API key for $_provider',
                      helperText: switch (_provider) {
                        'voiceitt' => 'Format: APP_ID:API_KEY',
                        'azure' => 'KEY 1 or KEY 2 of the Speech resource',
                        _ => null,
                      },
                    ),
                  ),
                ),
                const SizedBox(width: 8),
                FilledButton(onPressed: _saveKey, child: const Text('Save')),
              ],
            ),
          ],
        ),
      ),
    );
  }

  /// Whether a key for the chosen provider is stored, and where.
  Widget _keyStatus() {
    final scheme = Theme.of(context).colorScheme;
    final (IconData icon, Color color, String text) = switch ((_keyReadError, _keyStored)) {
      (final String e, _) => (Icons.error_outline, scheme.error, 'Could not read the key store: $e'),
      (null, true) => (
          Icons.check_circle,
          Colors.green.shade700,
          'Key for $_provider found in $_keyStoreName: ••••${_keyTail ?? ''}'
              '${_provider == 'azure' ? ' · region ${_regionCtl.text.trim()}' : ''}',
        ),
      (null, false) => (Icons.key_off, Colors.orange.shade800, 'No key stored for $_provider yet: enter it below.'),
    };
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: Row(
        children: [
          Icon(icon, color: color, size: 20),
          const SizedBox(width: 8),
          Expanded(child: Text(text, style: TextStyle(color: color))),
          if (_keyStored) TextButton(onPressed: _removeKey, child: const Text('Remove')),
        ],
      ),
    );
  }

  /// A number setting as "−  value  +" buttons. Not a Material Slider: it
  /// always keeps an overlay (its value bubble) whose accessibility node
  /// moves on every rebuild, which Flutter's Windows bridge rejects with
  /// "Failed to update ui::AXTree". Big buttons are also easier to hit.
  Widget _stepper({
    required String label,
    required num value,
    required num min,
    required num max,
    required num step,
    required String unit,
    required void Function(num) onChanged,
  }) {
    void set(num v) {
      setState(() => onChanged(v.clamp(min, max)));
      _saveSettings();
    }

    final text = '${value.round()} $unit';
    return Row(children: [
      SizedBox(width: 120, child: Text(label)),
      Semantics(
        button: true,
        label: '$label lower',
        child: IconButton(
          icon: const Icon(Icons.remove),
          onPressed: value > min ? () => set(value - step) : null,
        ),
      ),
      Semantics(
        label: label,
        value: text,
        child: ExcludeSemantics(
          child: SizedBox(width: 80, child: Text(text, textAlign: TextAlign.center)),
        ),
      ),
      Semantics(
        button: true,
        label: '$label higher',
        child: IconButton(
          icon: const Icon(Icons.add),
          onPressed: value < max ? () => set(value + step) : null,
        ),
      ),
    ]);
  }

  // ------------------------------------------------ live correction
  // LanguageTool account (optional): "e-mail:key", stored like a provider key.
  final _ltUserCtl = TextEditingController();
  final _ltKeyCtl = TextEditingController();
  bool _ltStored = false;
  String? _ltTail;

  Future<void> _loadLtKey() async {
    try {
      final k = await _secure.read(key: 'api_key:languagetool') ?? '';
      _ltStored = k.isNotEmpty;
      _ltTail = k.isEmpty ? null : k.substring(k.length > 4 ? k.length - 4 : 0);
      final i = k.lastIndexOf(':');
      if (i > 0) _ltUserCtl.text = k.substring(0, i);
      if (k.isNotEmpty) _engine.setCredential(provider: 'languagetool', secret: k);
    } catch (_) {}
    if (mounted) setState(() {});
  }

  Future<void> _saveLtKey() async {
    final user = _ltUserCtl.text.trim();
    final key = _ltKeyCtl.text.trim();
    if (user.isEmpty || key.isEmpty) {
      _snack('E-mail and API key are both needed');
      return;
    }
    final k = '$user:$key';
    try {
      _engine.setCredential(provider: 'languagetool', secret: k);
      await _secure.write(key: 'api_key:languagetool', value: k);
      _ltKeyCtl.text = '';
      await _loadLtKey();
    } catch (e) {
      _snack('Saving failed: $e');
    }
  }

  Future<void> _removeLtKey() async {
    try {
      await _secure.delete(key: 'api_key:languagetool');
      _engine.setCredential(provider: 'languagetool', secret: '');
    } catch (_) {}
    _ltUserCtl.text = '';
    await _loadLtKey();
  }

  Widget _correctionSection() {
    final on = (_settings['text'] as Map<String, dynamic>?)?['live_correction'] as bool? ?? false;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Live correction', style: Theme.of(context).textTheme.titleMedium),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Correct grammar and spelling while dictating'),
              subtitle: InkWell(
                onTap: () => launchUrl(Uri.parse('https://languagetool.org')),
                child: const Text('Checked by LanguageTool (languagetool.org) with your own account; the dictated text is sent there'),
              ),
              value: on,
              onChanged: (b) {
                (_settings['text'] ??= <String, dynamic>{})['live_correction'] = b;
                _saveSettings();
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Write corrections into the text'),
              subtitle: const Text('Off: corrections are only shown here as a suggestion, nothing typed is deleted. '
                  'On: the typed text is replaced by the corrected one.'),
              value: (_settings['text'] as Map<String, dynamic>?)?['correction_rewrites'] as bool? ?? false,
              onChanged: on
                  ? (b) {
                      (_settings['text'] ??= <String, dynamic>{})['correction_rewrites'] = b;
                      _saveSettings();
                      setState(() {});
                    }
                  : null,
            ),
            if (_suggestion.isNotEmpty) Padding(padding: const EdgeInsets.only(top: 4), child: Text('Suggested: $_suggestion')),
            Row(children: [
              Icon(_ltStored ? Icons.check_circle : Icons.info_outline,
                  size: 18, color: _ltStored ? Colors.green : Colors.grey),
              const SizedBox(width: 8),
              Expanded(
                child: Text(_ltStored
                    ? 'LanguageTool account found in $_keyStoreName: ••••$_ltTail (own limits)'
                    : 'No account yet: enter your LanguageTool e-mail and API key below, then corrections start'),
              ),
              if (_ltStored) TextButton(onPressed: _removeLtKey, child: const Text('Remove')),
            ]),
            TextField(
              controller: _ltUserCtl,
              decoration: const InputDecoration(labelText: 'LanguageTool account e-mail'),
            ),
            Row(children: [
              Expanded(
                child: TextField(
                  controller: _ltKeyCtl,
                  obscureText: true,
                  decoration: const InputDecoration(
                    labelText: 'API key',
                    hintText: 'From languagetool.org > Account > API',
                  ),
                ),
              ),
              const SizedBox(width: 8),
              FilledButton(onPressed: _saveLtKey, child: const Text('Save')),
            ]),
          ],
        ),
      ),
    );
  }

  Widget _soundSection() {
    final sound = _settings['sound'] as Map<String, dynamic>;
    final vad = sound['vad'] as Map<String, dynamic>;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Sound', style: Theme.of(context).textTheme.titleMedium),
            _stepper(
              label: 'Preamp gain',
              value: sound['preamp_gain_db'] as num,
              min: 0,
              max: 24,
              step: 1,
              unit: 'dB',
              onChanged: (v) => sound['preamp_gain_db'] = v.toDouble(),
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Automatic gain'),
              subtitle: const Text('Brings your voice to a steady level before it is sent'),
              value: sound['agc'] as bool? ?? true,
              onChanged: (b) {
                sound['agc'] = b;
                _saveSettings();
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Steady-noise filter (ventilator, fan)'),
              subtitle: const Text('Learns the room\'s steady noise from the pauses and takes it out under the words. Leaves the voice alone.'),
              value: sound['noise_profile'] != false,
              onChanged: (b) {
                sound['noise_profile'] = b;
                _saveSettings();
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Noise reduction (RNNoise)'),
              value: sound['denoise'] != 'Off',
              onChanged: (b) {
                sound['denoise'] = b ? 'RnNoise' : 'Off';
                _saveSettings();
              },
            ),
            const SizedBox(height: 8),
            const Text('Speech rhythm'),
            const SizedBox(height: 4),
            SegmentedButton<String>(
              segments: const [
                ButtonSegment(value: 'fluent', label: Text('Fluent'), tooltip: 'Text appears every 2 s; a 0.9 s pause ends a chunk'),
                ButtonSegment(value: 'words', label: Text('Word by word'), tooltip: 'Agreed words only: the last 12 s are transcribed every second and a word is typed once three of the last four readings agree on it, never changed. Batch upload, noise reduction off.'),
              ],
              selected: {_rhythmPreset(vad)},
              emptySelectionAllowed: true,
              showSelectedIcon: false,
              onSelectionChanged: (sel) {
                if (sel.isEmpty) return;
                setState(() {
                  final words = sel.first == 'words';
                  vad['chunk_interval_ms'] = words ? 3000 : 2000;
                  vad['silence_duration_ms'] = words ? 4000 : 900;
                  vad['refine'] = words;
                  vad['type_quick_text'] = true;
                  vad['agreement'] = words;
                  // Measured on impaired speech: RNNoise removes the soft
                  // consonants and makes every provider worse.
                  if (words) sound['denoise'] = 'Off';
                  // Live streams of unclear speech come back empty or as
                  // word salad; chunks between words carry the context.
                  final options = _settings['providers']['options'] as Map<String, dynamic>;
                  final o = Map<String, dynamic>.from(options[_provider] as Map<String, dynamic>? ?? {});
                  o['transfer'] = words ? 'Batch' : 'Streaming';
                  options[_provider] = o;
                });
                _saveSettings();
              },
            ),
            _stepper(
              label: 'Chunk length',
              value: (vad['chunk_interval_ms'] as num) / 1000,
              min: 1,
              max: 60,
              step: 1,
              unit: 's',
              onChanged: (v) => vad['chunk_interval_ms'] = (v * 1000).round(),
            ),
            _stepper(
              label: 'Pause',
              value: vad['silence_duration_ms'] as num,
              min: 300,
              max: 6000,
              step: 100,
              unit: 'ms',
              onChanged: (v) => vad['silence_duration_ms'] = v.round(),
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Agreed words only (no corrections)'),
              subtitle: const Text('The last 12 s are transcribed every second; a word is typed once three of the last four readings agree on it, '
                  'and never changed. The rest comes at the pause. Measured 100% with OpenAI (whisper-1) and your phrases in the vocabulary.'),
              value: vad['agreement'] == true,
              onChanged: (b) {
                vad['agreement'] = b;
                _saveSettings();
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Re-check whole thoughts'),
              subtitle: const Text('After a pause the whole thought is transcribed again in one piece and replaces the quick text. '
                  'Needed for slow or unclear speech; the recording is sent twice.'),
              value: vad['refine'] == true,
              onChanged: (b) {
                vad['refine'] = b;
                _saveSettings();
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Type the quick text too'),
              subtitle: const Text('Off: the quick chunks only show up here while you speak and each thought is typed once, when it is known. '
                  'On: they are typed at once and replaced afterwards (fluent speakers).'),
              value: vad['type_quick_text'] == true,
              onChanged: vad['refine'] == true
                  ? (b) {
                      vad['type_quick_text'] = b;
                      _saveSettings();
                    }
                  : null,
            ),
            Text(
              'Chunks are cut only between words, at the first gap after the chunk length; a pause always ends one. '
              'Quick chunks give fast text, the whole thought gives the right text.',
              style: Theme.of(context).textTheme.bodySmall,
            ),
          ],
        ),
      ),
    );
  }

  /// Master switch between the released pipeline for clear speech and the
  /// impaired-voice setup under development. Each side writes its whole
  /// profile, so later experiments change only the impaired side.
  Widget _voiceModeSection() {
    final sound = _settings['sound'] as Map<String, dynamic>;
    final impaired = sound['impaired_voice'] != false;
    return Card(
      child: SwitchListTile(
        title: const Text('Impaired voice'),
        subtitle: Text(impaired
            ? 'On: agreed words only, batch upload, steady-noise filter, 4 s pause — the setup for slow or unclear speech (in development).'
            : 'Off: the released setup for clear speech (v0.0.1(51)): quick 2 s chunks, 0.9 s pause, live stream where available, no filters.'),
        value: impaired,
        onChanged: (b) {
          setState(() => _applyVoiceProfile(b));
          _saveSettings();
        },
      ),
    );
  }

  void _applyVoiceProfile(bool impaired) {
    final sound = _settings['sound'] as Map<String, dynamic>;
    final vad = sound['vad'] as Map<String, dynamic>;
    final options = _settings['providers']['options'] as Map<String, dynamic>;
    final o = Map<String, dynamic>.from(options[_provider] as Map<String, dynamic>? ?? {});
    sound['impaired_voice'] = impaired;
    // Common to both: automatic gain, no RNNoise, no tone shaping.
    sound['agc'] = true;
    sound['denoise'] = 'Off';
    sound['gate'] = false;
    sound['highpass_hz'] = 0.0;
    sound['presence_db'] = 0.0;
    sound['compressor'] = false;
    sound['trill_filler'] = false;
    if (impaired) {
      sound['noise_profile'] = true;
      vad['chunk_interval_ms'] = 3000;
      vad['silence_duration_ms'] = 4000;
      vad['refine'] = true;
      vad['type_quick_text'] = true;
      vad['agreement'] = true;
      vad['agreement_step_ms'] = 1000;
      vad['agreement_windows'] = 4;
      o['transfer'] = 'Batch';
    } else {
      // v0.0.1(51), the release for clear speech.
      sound['noise_profile'] = false;
      vad['chunk_interval_ms'] = 2000;
      vad['silence_duration_ms'] = 900;
      vad['refine'] = false;
      vad['agreement'] = false;
      o['transfer'] = 'Streaming';
    }
    options[_provider] = o;
  }

  /// One line with the settings the next session runs with.
  String _activeSummary() {
    final opts = (_settings['providers']['options'] as Map<String, dynamic>)[_provider] as Map<String, dynamic>? ?? {};
    final sound = _settings['sound'] as Map<String, dynamic>;
    final vad = sound['vad'] as Map<String, dynamic>;
    final mic = (_settings['microphone'] as Map<String, dynamic>)['device_label'] as String? ?? 'default microphone';
    final chunk = ((vad['chunk_interval_ms'] as num) / 1000).toStringAsFixed(0);
    final pause = ((vad['silence_duration_ms'] as num) / 1000).toStringAsFixed(1);
    final vocab = ((opts['keywords'] as List?) ?? const []).length;
    return [
      sound['impaired_voice'] != false ? 'IMPAIRED VOICE setup' : 'normal voice setup (v0.0.1(51))',
      // No saved options yet means the engine's default: batch.
      '$_provider · ${(opts['transfer'] ?? 'Batch') == 'Batch' ? 'batch' : 'stream'} · ${kLanguages[_settings['providers']['language']] ?? _settings['providers']['language']} · vocabulary: $vocab',
      vad['agreement'] == true
          ? 'agreed words only · every ${((vad['agreement_step_ms'] as num? ?? 1000) / 1000).toStringAsFixed(1)} s · pause $pause s'
          : 'chunks $chunk s · pause $pause s · re-check ${vad['refine'] == true ? 'on' : 'off'}',
      'steady-noise filter ${sound['noise_profile'] != false ? 'on' : 'off'} · noise reduction ${sound['denoise'] != 'Off' ? 'on' : 'off'} · gain ${sound['agc'] == false ? 'fixed' : 'auto'} · $mic',
    ].join('\n');
  }

  /// Which preset the two VAD values match, '' when neither.
  String _rhythmPreset(Map<String, dynamic> vad) {
    final chunk = vad['chunk_interval_ms'], pause = vad['silence_duration_ms'];
    if (chunk == 2000 && pause == 900) return 'fluent';
    if (chunk == 3000 && pause == 4000 && vad['refine'] == true && vad['agreement'] == true) return 'words';
    if (vad['refine'] == true || vad['agreement'] == true) return '';
    return '';
  }

  /// The vocabulary field saves when it loses focus too: a user who
  /// clicks away instead of pressing Enter must not dictate without it.
  void _saveVocab() {
    if (!_vocabDirty) return;
    _vocabDirty = false;
    _saveSettings();
    _snack('Vocabulary saved');
  }

  void _saveReplayPath() {
    final path = _replayCtl.text.trim();
    if (path.isEmpty) {
      _snack('Enter the path of a WAV file');
      return;
    }
    final mic = _settings['microphone'] as Map<String, dynamic>;
    mic['device_id'] = _replayPrefix + path;
    mic['device_label'] = path.split(Platform.pathSeparator).last;
    _saveSettings();
  }

  Widget _micSection() {
    final mic = _settings['microphone'] as Map<String, dynamic>;
    final current = mic['device_id'] as String?;
    final ids = _mics.map((m) => m.id).toList();
    final replaying = current != null && current.startsWith(_replayPrefix);
    if (replaying && _replayCtl.text.isEmpty) _replayCtl.text = current.substring(_replayPrefix.length);
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Microphone', style: Theme.of(context).textTheme.titleMedium),
            DropdownButton<String?>(
              value: replaying
                  ? _replayPrefix
                  : ids.contains(current)
                      ? current
                      : null,
              isExpanded: true,
              hint: const Text('System default'),
              items: [
                const DropdownMenuItem<String?>(value: null, child: Text('System default')),
                ..._mics.map((m) => DropdownMenuItem<String?>(value: m.id, child: Text(m.isDefault ? '${m.name} (default)' : m.name))),
                const DropdownMenuItem<String?>(value: _replayPrefix, child: Text('Replay a recording (test)')),
              ],
              onChanged: (id) {
                if (id == _replayPrefix) {
                  // The path field below completes the device id.
                  mic['device_id'] = _replayPrefix + _replayCtl.text.trim();
                  mic['device_label'] = 'Recording';
                } else {
                  mic['device_id'] = id;
                  mic['device_label'] = _mics.where((m) => m.id == id).map((m) => m.name).firstOrNull;
                }
                _saveSettings();
              },
            ),
            if (replaying) ...[
              TextField(
                controller: _replayCtl,
                decoration: const InputDecoration(
                  labelText: 'WAV file (16-bit PCM)',
                  helperText: 'Played through the same chain as a microphone, in real time, then silence until you stop. '
                      'The same sample on every device and provider.',
                ),
                onSubmitted: (_) => _saveReplayPath(),
              ),
              Align(
                alignment: Alignment.centerRight,
                child: FilledButton(onPressed: _saveReplayPath, child: const Text('Use this file')),
              ),
            ],
          ],
        ),
      ),
    );
  }

  Widget _fallbackCard() {
    return Card(
      color: Theme.of(context).colorScheme.primaryContainer,
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Cannot type into the focused app ($_fallbackReason). Your text is kept here:'),
            const SizedBox(height: 8),
            SelectableText(_fallbackText),
            const SizedBox(height: 8),
            Row(children: [
              FilledButton(
                onPressed: () {
                  try {
                    _engine.transferToTarget();
                  } catch (e) {
                    _snack('$e');
                  }
                },
                child: const Text('Insert into focused app'),
              ),
              const SizedBox(width: 8),
              OutlinedButton(
                onPressed: () {
                  Clipboard.setData(ClipboardData(text: _fallbackText));
                  _engine.clearSessionText();
                  setState(() {
                    _fallbackReason = null;
                    _fallbackText = '';
                  });
                },
                child: const Text('Copy'),
              ),
            ]),
          ],
        ),
      ),
    );
  }

  void _showAbout(BuildContext context) {
    final a = about();
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('About $appName'),
        content: Text('App 0.1.0+1\nCore ${a.coreVersion} (build ${a.buildNumber}, ${a.gitSha}, ${a.buildDate})\n'
            '${a.targetOs}-${a.targetArch}\nCore license: ${a.coreLicense}\nProviders: ${a.providers.join(', ')}'),
        actions: [
          TextButton(
            onPressed: () {
              Navigator.pop(ctx);
              Navigator.push(context, MaterialPageRoute<void>(builder: (_) => const LicensesScreen()));
            },
            child: const Text('Licenses'),
          ),
          TextButton(onPressed: () => Navigator.pop(ctx), child: const Text('Close')),
        ],
      ),
    );
  }
}

class LicensesScreen extends StatelessWidget {
  const LicensesScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return DefaultTabController(
      length: 3,
      child: Scaffold(
        appBar: AppBar(
          title: const Text('Licenses'),
          bottom: const TabBar(tabs: [Tab(text: 'License'), Tab(text: 'Notice'), Tab(text: 'Third party')]),
        ),
        body: TabBarView(
          children: [licenseText(), noticeText(), thirdPartyText()]
              .map((t) => SingleChildScrollView(padding: const EdgeInsets.all(16), child: SelectableText(t)))
              .toList(),
        ),
      ),
    );
  }
}

enum _StepKind { info, listening, ok, retry }

/// One message of the guided recording.
class _VoiceStep {
  const _VoiceStep(this.text, this.kind);
  final String text;
  final _StepKind kind;

  IconData get icon => switch (kind) {
        _StepKind.listening => Icons.mic,
        _StepKind.ok => Icons.check_circle,
        _StepKind.retry => Icons.replay,
        _StepKind.info => Icons.info_outline,
      };

  Color color(ColorScheme c) => switch (kind) {
        _StepKind.listening => c.primary,
        _StepKind.ok => Colors.green.shade700,
        _StepKind.retry => Colors.orange.shade800,
        _StepKind.info => c.onSurfaceVariant,
      };
}
