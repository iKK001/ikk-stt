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
import 'ui_strings.dart';
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
  String get _keyStoreName {
    if (Platform.isMacOS || Platform.isIOS) return _t['storeKeychain'];
    if (Platform.isAndroid) return _t['storeAndroid'];
    if (Platform.isWindows) return _t['storeWindows'];
    return _t['storeGeneric'];
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

  /// The app's own language (settings `app.language`, ISO 639-3; '' = English).
  String get _appLanguage => ((_settings['app'] as Map<String, dynamic>?)?['language'] as String?) ?? '';
  UiStrings get _t => UiStrings.of(_appLanguage);

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
      _snack(_t.keyRemoved(_provider));
    } catch (e) {
      _snack(_t.fmt('keyRemoveFailed', {'e': e}));
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
      _snack(_t['azureRegionMissing']);
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
      _snack(_t.keySaved(_keyStoreName));
    } catch (e) {
      _snack(_t.fmt('keyRunOnly', {'e': e}));
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
      _snack(_t.settingsNotSaved('$e'));
      return;
    }
    _applyVoice();
    setState(() {});
  }

  void _onEvent(String raw) {
    final ev = jsonDecode(raw);
    if (ev is String) {
      if (ev == 'PermissionRequired') _snack(_t.permissionNeeded);
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
          _snack(v == 'Start' ? _t.voiceStart : _t.voiceStop);
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
          _snack(_t.insertedIntoTarget);
        case 'Error':
          _error = v as String;
        case 'Suggestion':
          _suggestion = v as String;
        case 'CommandRefused':
          _snack(_t.commandRefused(v as String));
        case 'CommandHeard':
          _snack(_t.commandDone(v as String));
        case 'Command':
          break;
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
        title: Text(_t.fmt(textOnly ? 'consentTitleText' : 'consentTitleAudio', {'p': r['name']})),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t.fmt(textOnly ? 'consentBodyText' : 'consentBodyAudio', {'p': r['name'], 'u': uses})),
            const SizedBox(height: 12),
            TextButton(
              onPressed: () => launchUrl(Uri.parse(r['privacy_url'] as String)),
              child: Text(_t['privacyPolicy']),
            ),
          ],
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx, false), child: Text(_t['deny'])),
          FilledButton(onPressed: () => Navigator.pop(ctx, true), child: Text(_t['allow'])),
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
          title: Text(_t['floatingTitle']),
          subtitle: Text(_t['floatingBody']),
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
      _snack(_t.fmt('voiceReadError', {'e': e}));
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
      _snack(_t.fmt('voiceError', {'e': e}));
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
      final step = ValueNotifier<_VoiceStep>(_VoiceStep(_t['getReady'], _StepKind.info));
      var cancelled = false;
      var detail = '';
      if (!mounted) return;
      showDialog<void>(
        context: context,
        barrierDismissible: false,
        builder: (ctx) => AlertDialog(
          title: Text(_t['voiceDialogTitle']),
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
              child: Text(_t['close']),
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
        final label = _t[word == 'start' ? 'wordStart' : 'wordStop'];
        if (word == 'stop') {
          await show(_t['startLearned'], _StepKind.ok, 2500);
        }
        var retry = false;
        while (!cancelled && takes[word]!.length < 3) {
          final n = takes[word]!.length + 1;
          await show(
            _t.fmt(
              retry
                  ? 'sayWordRetry'
                  : n == 1
                      ? 'sayWordNow'
                      : 'sayWordMore',
              {'d': detail, 'l': label, 'n': n},
            ),
            retry ? _StepKind.retry : _StepKind.listening,
          );
          try {
            final take = await voiceRecordTake(settingsJson: jsonEncode(_settings), timeoutSecs: 8);
            if (cancelled) break;
            takes[word]!.add(take);
            retry = false;
            await show(_t.fmt('understood', {'n': n}), _StepKind.ok, 1300);
          } catch (e) {
            if (cancelled) break;
            retry = true;
            detail = '$e'.replaceFirst(RegExp(r'^.*?(nothing heard|heard something)'), r'$1');
            await show(_t.fmt('notClear', {'d': detail}), _StepKind.retry, 2000);
          }
          if (!cancelled && takes[word]!.length == 3) {
            try {
              await voiceCheckWord(takes: takes[word]!);
            } catch (_) {
              takes[word]!.clear();
              retry = false;
              await show(_t.fmt('tooDifferent', {'l': label}), _StepKind.retry, 2500);
            }
          }
        }
        if (cancelled) break;
      }
      if (cancelled) {
        _applyVoice();
        return;
      }
      await show(_t['learningWords'], _StepKind.listening);
      try {
        _voiceModel = await voiceTrain(startTakes: takes['start']!, stopTakes: takes['stop']!);
        _voiceOn = true;
        await _saveVoice();
        await show(_t['voiceDone'], _StepKind.ok, 2500);
        if (mounted && !cancelled) Navigator.of(context).pop();
      } catch (e) {
        // Stays visible until closed: the reason matters for the next try.
        final msg = '$e'.replaceFirst(RegExp(r'^.*?Error:\s*'), '');
        await show(_t.fmt('couldNotLearn', {'e': msg}), _StepKind.retry);
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
            Text(_t['voiceTitle'], style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 4),
            Text(_t['voiceBody']),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['voiceSwitch']),
              subtitle: Text(_t[_voiceModel == null
                  ? 'voiceRecordFirst'
                  : _voiceStandby
                      ? 'voiceListening'
                      : 'voiceReady']),
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
              child: Text(_t[_voiceModel == null ? 'voiceRecord' : 'voiceRecordAgain']),
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
        title: Text(_t['setupTitle']),
        content: Text(s['message'] as String? ?? ''),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx, false), child: Text(_t['cancel'])),
          if (url.isNotEmpty)
            TextButton(
              onPressed: () => launchUrl(Uri.parse(url)),
              child: Text(_t['openSettings']),
            ),
          FilledButton(onPressed: () => Navigator.pop(ctx, true), child: Text(_t['doneStart'])),
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
    // The widgets without settings at hand (licences page, platform cards)
    // read the language from here.
    UiStrings.current = _t;
    final listening = _state == 'Listening' || _state == 'Finishing';
    if (_settings.isEmpty) return const Scaffold(body: Center(child: CircularProgressIndicator()));
    final impairedMode = (_settings['sound'] as Map<String, dynamic>)['impaired_voice'] != false;
    return Scaffold(
      appBar: AppBar(
        title: const Text(appName),
        actions: [
          PopupMenuButton<String>(
            icon: const Icon(Icons.language),
            tooltip: _t.appLanguage,
            initialValue: _appLanguage.isEmpty ? 'eng' : _appLanguage,
            onSelected: (code) {
              (_settings['app'] ??= <String, dynamic>{})['language'] = code == 'eng' ? '' : code;
              _saveSettings();
              setState(() {});
            },
            itemBuilder: (_) => [
              for (final code in UiStrings.codes) PopupMenuItem(value: code, child: Text(UiStrings.of(code).name)),
            ],
          ),
          IconButton(icon: const Icon(Icons.info_outline), onPressed: () => _showAbout(context)),
        ],
      ),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          _voiceModeSection(),
          // The cards that exist only for the impaired setup, right under
          // the switch, sliding in and out with it.
          AnimatedSize(
            duration: const Duration(milliseconds: 350),
            curve: Curves.easeInOut,
            alignment: Alignment.topCenter,
            child: impairedMode
                ? Column(children: [
                    const SizedBox(height: 16),
                    _impairedSoundSection(),
                    const SizedBox(height: 16),
                    _trainingSection(),
                  ])
                : const SizedBox.shrink(),
          ),
          const SizedBox(height: 16),
          _providerSection(),
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
              label: Text(_t[listening ? 'stop' : 'dictate'], style: const TextStyle(fontSize: 20)),
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
            label: _t.fmt('status', {'s': _state}),
            child: ExcludeSemantics(child: Center(child: Text('$_state${_speech ? ' · ${_t['speech']}' : ''}'))),
          ),
          const SizedBox(height: 8),
          ExcludeSemantics(child: LinearProgressIndicator(value: ((_level + 60) / 60).clamp(0.0, 1.0))),
          const SizedBox(height: 12),
          if (_error != null) Text(_error!, style: TextStyle(color: Theme.of(context).colorScheme.error)),
          if (_fallbackReason != null) _fallbackCard(),
          Semantics(
            container: true,
            label: _finals.isEmpty ? _t['noTextYet'] : _t.fmt('lastRecognised', {'s': _finals.last}),
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
      _snack(_t.noKey(_provider));
      return;
    }
    final denied = await ensureMicPermission();
    if (denied != null) {
      _snack(denied);
      return;
    }
    // Nothing may dictate while the takes are recorded.
    _engine.setVoiceTraining(active: true);
    final step = ValueNotifier<_VoiceStep>(_VoiceStep(_t.fmt('getReadyWord', {'w': word}), _StepKind.info));
    var cancelled = false;
    final heardAll = <String>[];
    if (!mounted) return;
    showDialog<void>(
      context: context,
      barrierDismissible: false,
      builder: (ctx) => AlertDialog(
        title: Text(_t.fmt('trainWordTitle', {'w': word})),
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
            child: Text(_t['close']),
          ),
        ],
      ),
    ).then((_) => cancelled = true);
    Future<void> show(String text, _StepKind kind, [int ms = 0]) async {
      step.value = _VoiceStep(text, kind);
      if (ms > 0) await Future<void>.delayed(Duration(milliseconds: ms));
    }

    try {
      await show(_t['getReady'], _StepKind.info, 1200);
      var n = 1;
      var failures = 0;
      while (!cancelled && n <= takes) {
        await show(_t.fmt('sayWordTake', {'w': word, 'n': n, 't': takes}), _StepKind.listening);
        try {
          final heard = await trainingTake(settingsJson: jsonEncode(_settings), credential: key, timeoutSecs: 10);
          if (cancelled) break;
          heardAll.add(heard);
          _learn(word, heard);
          await show(heard.isEmpty ? _t['heardNothing'] : _t.fmt('heardTake', {'s': heard, 'n': n, 't': takes}), _StepKind.ok, 1400);
          n++;
        } catch (e) {
          if (cancelled) break;
          failures++;
          await show(_t.fmt('didntCatch', {'e': e}), _StepKind.retry, 1800);
          if (failures >= 3) break;
        }
      }
      if (heardAll.isNotEmpty) {
        await _saveSettings();
        final forms = heardAll.where((h) => h.isNotEmpty && _normWord(h) != _normWord(word)).toSet();
        final hits = heardAll.where((h) => _normWord(h) == _normWord(word)).length;
        await show(
          forms.isEmpty
              ? _t.fmt('trainedAsIs', {'w': word, 'n': hits, 't': heardAll.length})
              : _t.fmt('trainedLearned', {'w': word, 's': forms.join(', ')}),
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
      Text(_t['teachTitle'], style: Theme.of(context).textTheme.titleSmall),
      Text(_t['teachBody']),
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
                    _snack(_t.learned(word, selected));
                    setState(() => _teachFrom = _teachTo = null);
                  },
            child: Text(_t['teach']),
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
            Text(_t['trainingTitle'], style: Theme.of(context).textTheme.titleMedium),
            Text(_t['trainingBody']),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['useLearned']),
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
                  decoration: InputDecoration(labelText: _t['wordToTrain'], hintText: _t['wordHint']),
                  onChanged: (_) => setState(() {}),
                  onSubmitted: (_) => hasKey ? _trainWord(_trainCtl.text.trim()) : null,
                ),
              ),
              const SizedBox(width: 8),
              FilledButton.icon(
                onPressed: hasKey && _state != 'Listening' ? () => _trainWord(_trainCtl.text.trim()) : null,
                icon: const Icon(Icons.mic),
                label: Text(_t['sayIt5']),
              ),
            ]),
            if (!hasKey) Padding(padding: const EdgeInsets.only(top: 4), child: Text(_t['enterKeyFirst'])),
            ..._teachFromText(words),
            for (final e in words)
              ExpansionTile(
                tilePadding: EdgeInsets.zero,
                title: Text(e['word'] as String),
                subtitle: Text(
                  ((e['heard'] as List?) ?? const []).isEmpty
                      ? _t['recognisedAsIs']
                      : _t.fmt('heardAs', {
                          's': ((e['heard'] as List).cast<Map<String, dynamic>>()).map((f) => '${f['text']} ×${f['count'] ?? 1}').join(', '),
                        }),
                ),
                children: [
                  SwitchListTile(
                    contentPadding: EdgeInsets.zero,
                    title: Text(_t['replaceAlways']),
                    subtitle: Text(_t['replaceAlwaysBody']),
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
                      label: Text(_t['sayAgain']),
                    ),
                    TextButton.icon(
                      onPressed: () {
                        (t['words'] as List).remove(e);
                        _saveSettings();
                        setState(() {});
                      },
                      icon: const Icon(Icons.delete_outline),
                      label: Text(_t['remove']),
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
            Text(_t['providerTitle'], style: Theme.of(context).textTheme.titleMedium),
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
              title: Text(_t['batchUpload']),
              subtitle: Text(_t['batchBody']),
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
              decoration: InputDecoration(labelText: _t['vocabLabel'], helperText: _t['vocabHelper']),
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
                decoration: InputDecoration(labelText: _t['azureRegion'], helperText: _t['azureRegionHelper']),
              ),
            Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _keyCtl,
                    obscureText: true,
                    decoration: InputDecoration(
                      labelText: _keyStored ? _t['newKey'] : _t.fmt('apiKeyFor', {'p': _provider}),
                      helperText: switch (_provider) {
                        'voiceitt' => 'Format: APP_ID:API_KEY',
                        'azure' => _t['azureKeyHelper'],
                        _ => null,
                      },
                    ),
                  ),
                ),
                const SizedBox(width: 8),
                FilledButton(onPressed: _saveKey, child: Text(_t['save'])),
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
      (final String e, _) => (Icons.error_outline, scheme.error, _t.fmt('keyReadError', {'e': e})),
      (null, true) => (
          Icons.check_circle,
          Colors.green.shade700,
          _t.fmt('keyFound', {'p': _provider, 's': _keyStoreName, 'k': _keyTail ?? ''}) +
              (_provider == 'azure' ? ' · ${_t['region']} ${_regionCtl.text.trim()}' : ''),
        ),
      (null, false) => (Icons.key_off, Colors.orange.shade800, _t.fmt('keyNone', {'p': _provider})),
    };
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: Row(
        children: [
          Icon(icon, color: color, size: 20),
          const SizedBox(width: 8),
          Expanded(child: Text(text, style: TextStyle(color: color))),
          if (_keyStored) TextButton(onPressed: _removeKey, child: Text(_t['remove'])),
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
        label: _t.fmt('lower', {'l': label}),
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
        label: _t.fmt('higher', {'l': label}),
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
      _snack(_t['ltBothNeeded']);
      return;
    }
    final k = '$user:$key';
    try {
      _engine.setCredential(provider: 'languagetool', secret: k);
      await _secure.write(key: 'api_key:languagetool', value: k);
      _ltKeyCtl.text = '';
      await _loadLtKey();
    } catch (e) {
      _snack(_t.savingFailed('$e'));
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
            Text(_t['correctionTitle'], style: Theme.of(context).textTheme.titleMedium),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['correctionSwitch']),
              subtitle: InkWell(
                onTap: () => launchUrl(Uri.parse('https://languagetool.org')),
                child: Text(_t['correctionBody']),
              ),
              value: on,
              onChanged: (b) {
                (_settings['text'] ??= <String, dynamic>{})['live_correction'] = b;
                _saveSettings();
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['rewriteSwitch']),
              subtitle: Text(_t['rewriteBody']),
              value: (_settings['text'] as Map<String, dynamic>?)?['correction_rewrites'] as bool? ?? false,
              onChanged: on
                  ? (b) {
                      (_settings['text'] ??= <String, dynamic>{})['correction_rewrites'] = b;
                      _saveSettings();
                      setState(() {});
                    }
                  : null,
            ),
            if (_suggestion.isNotEmpty) Padding(padding: const EdgeInsets.only(top: 4), child: Text(_t.fmt('suggested', {'s': _suggestion}))),
            Row(children: [
              Icon(_ltStored ? Icons.check_circle : Icons.info_outline,
                  size: 18, color: _ltStored ? Colors.green : Colors.grey),
              const SizedBox(width: 8),
              Expanded(
                child: Text(_ltStored ? _t.fmt('ltFound', {'s': _keyStoreName, 'k': _ltTail}) : _t['ltNone']),
              ),
              if (_ltStored) TextButton(onPressed: _removeLtKey, child: Text(_t['remove'])),
            ]),
            TextField(
              controller: _ltUserCtl,
              decoration: InputDecoration(labelText: _t['ltEmail']),
            ),
            Row(children: [
              Expanded(
                child: TextField(
                  controller: _ltKeyCtl,
                  obscureText: true,
                  decoration: InputDecoration(labelText: _t['apiKey'], hintText: _t['ltKeyHint']),
                ),
              ),
              const SizedBox(width: 8),
              FilledButton(onPressed: _saveLtKey, child: Text(_t['save'])),
            ]),
          ],
        ),
      ),
    );
  }

  /// The fine-tuning fold of the Sound card (and the test entries of the
  /// microphone list) is open.
  bool _advanced = false;

  Widget _soundSection() {
    final sound = _settings['sound'] as Map<String, dynamic>;
    final vad = sound['vad'] as Map<String, dynamic>;
    final impaired = sound['impaired_voice'] != false;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['soundTitle'], style: Theme.of(context).textTheme.titleMedium),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['agc']),
              subtitle: Text(_t['agcBody']),
              value: sound['agc'] as bool? ?? true,
              onChanged: (b) {
                sound['agc'] = b;
                _saveSettings();
              },
            ),
            if (!impaired)
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: Text(_t['rnnoise']),
                value: sound['denoise'] != 'Off',
                onChanged: (b) {
                  sound['denoise'] = b ? 'RnNoise' : 'Off';
                  _saveSettings();
                },
              ),
            ExpansionTile(
              tilePadding: EdgeInsets.zero,
              title: Text(_t['advanced']),
              subtitle: Text(_t[impaired ? 'advGain' : 'advGainChunk']),
              initiallyExpanded: _advanced,
              onExpansionChanged: (open) => setState(() => _advanced = open),
              children: [
                _stepper(
                  label: _t['preamp'],
                  value: sound['preamp_gain_db'] as num,
                  min: 0,
                  max: 24,
                  step: 1,
                  unit: 'dB',
                  onChanged: (v) => sound['preamp_gain_db'] = v.toDouble(),
                ),
                if (!impaired) ...[
                  _stepper(
                    label: _t['chunkLength'],
                    value: (vad['chunk_interval_ms'] as num) / 1000,
                    min: 1,
                    max: 60,
                    step: 1,
                    unit: 's',
                    onChanged: (v) => vad['chunk_interval_ms'] = (v * 1000).round(),
                  ),
                  _stepper(
                    label: _t['pause'],
                    value: vad['silence_duration_ms'] as num,
                    min: 300,
                    max: 6000,
                    step: 100,
                    unit: 'ms',
                    onChanged: (v) => vad['silence_duration_ms'] = v.round(),
                  ),
                  Text(_t['chunkNote'], style: Theme.of(context).textTheme.bodySmall),
                ],
              ],
            ),
          ],
        ),
      ),
    );
  }

  /// What only the impaired setup uses; shown under the master switch.
  Widget _impairedSoundSection() {
    final sound = _settings['sound'] as Map<String, dynamic>;
    final vad = sound['vad'] as Map<String, dynamic>;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['impairedSoundTitle'], style: Theme.of(context).textTheme.titleMedium),
            Text(_t['impairedSoundBody']),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['noiseFilter']),
              subtitle: Text(_t['noiseFilterBody']),
              value: sound['noise_profile'] != false,
              onChanged: (b) {
                sound['noise_profile'] = b;
                _saveSettings();
              },
            ),
            _stepper(
              label: _t['pauseThought'],
              value: vad['silence_duration_ms'] as num,
              min: 1500,
              max: 6000,
              step: 100,
              unit: 'ms',
              onChanged: (v) => vad['silence_duration_ms'] = v.round(),
            ),
            ExpansionTile(
              tilePadding: EdgeInsets.zero,
              title: Text(_t['advanced']),
              subtitle: Text(_t['impairedAdvBody']),
              initiallyExpanded: _advanced,
              onExpansionChanged: (open) => setState(() => _advanced = open),
              children: [
                SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: Text(_t['agreedOnly']),
                  subtitle: Text(_t['agreedOnlyBody']),
                  value: vad['agreement'] == true,
                  onChanged: (b) {
                    vad['agreement'] = b;
                    _saveSettings();
                  },
                ),
                SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: Text(_t['recheck']),
                  value: vad['refine'] == true,
                  onChanged: (b) {
                    vad['refine'] = b;
                    _saveSettings();
                  },
                ),
                SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: Text(_t['quickText']),
                  value: vad['type_quick_text'] == true,
                  onChanged: vad['refine'] == true
                      ? (b) {
                          vad['type_quick_text'] = b;
                          _saveSettings();
                        }
                      : null,
                ),
                SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: Text(_t['rnnoise']),
                  subtitle: Text(_t['rnnoiseImpairedBody']),
                  value: sound['denoise'] != 'Off',
                  onChanged: (b) {
                    sound['denoise'] = b ? 'RnNoise' : 'Off';
                    _saveSettings();
                  },
                ),
              ],
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
        title: Text(_t['impairedVoice']),
        subtitle: Text(_t[impaired ? 'impairedOn' : 'impairedOff']),
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
    final mic = (_settings['microphone'] as Map<String, dynamic>)['device_label'] as String? ?? _t['defaultMic'];
    final chunk = ((vad['chunk_interval_ms'] as num) / 1000).toStringAsFixed(0);
    final pause = ((vad['silence_duration_ms'] as num) / 1000).toStringAsFixed(1);
    final vocab = ((opts['keywords'] as List?) ?? const []).length;
    return [
      _t[sound['impaired_voice'] != false ? 'summaryImpaired' : 'summaryNormal'],
      // No saved options yet means the engine's default: batch.
      '$_provider · ${_t[(opts['transfer'] ?? 'Batch') == 'Batch' ? 'batch' : 'stream']} · ${kLanguages[_settings['providers']['language']] ?? _settings['providers']['language']} · ${_t['vocabulary']}: $vocab',
      vad['agreement'] == true
          ? _t.fmt('summaryAgreed', {'s': ((vad['agreement_step_ms'] as num? ?? 1000) / 1000).toStringAsFixed(1), 'p': pause})
          : _t.fmt('summaryChunks', {'c': chunk, 'p': pause, 'r': _t[vad['refine'] == true ? 'on' : 'off']}),
      _t.fmt('summaryFilters', {
        'a': _t[sound['noise_profile'] != false ? 'on' : 'off'],
        'b': _t[sound['denoise'] != 'Off' ? 'on' : 'off'],
        'g': _t[sound['agc'] == false ? 'fixed' : 'auto'],
        'm': mic,
      }),
    ].join('\n');
  }

  /// Which preset the two VAD values match, '' when neither.
  /// The vocabulary field saves when it loses focus too: a user who
  /// clicks away instead of pressing Enter must not dictate without it.
  void _saveVocab() {
    if (!_vocabDirty) return;
    _vocabDirty = false;
    _saveSettings();
    _snack(_t.vocabularySaved);
  }

  void _saveReplayPath() {
    final path = _replayCtl.text.trim();
    if (path.isEmpty) {
      _snack(_t['enterWavPath']);
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
            Text(_t['micTitle'], style: Theme.of(context).textTheme.titleMedium),
            DropdownButton<String?>(
              value: replaying
                  ? _replayPrefix
                  : ids.contains(current)
                      ? current
                      : null,
              isExpanded: true,
              hint: Text(_t['systemDefault']),
              items: [
                DropdownMenuItem<String?>(value: null, child: Text(_t['systemDefault'])),
                ..._mics.map((m) => DropdownMenuItem<String?>(value: m.id, child: Text(m.isDefault ? '${m.name} ${_t['defaultSuffix']}' : m.name))),
                if (_advanced || replaying) DropdownMenuItem<String?>(value: _replayPrefix, child: Text(_t['replayEntry'])),
              ],
              onChanged: (id) {
                if (id == _replayPrefix) {
                  // The path field below completes the device id.
                  mic['device_id'] = _replayPrefix + _replayCtl.text.trim();
                  mic['device_label'] = _t['recording'];
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
                decoration: InputDecoration(labelText: _t['wavLabel'], helperText: _t['wavHelper']),
                onSubmitted: (_) => _saveReplayPath(),
              ),
              Align(
                alignment: Alignment.centerRight,
                child: FilledButton(onPressed: _saveReplayPath, child: Text(_t['useFile'])),
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
            Text(_t.fmt('fallbackTitle', {'r': _fallbackReason})),
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
                child: Text(_t['insertFocused']),
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
                child: Text(_t['copy']),
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
        title: Text(_t.fmt('aboutTitle', {'a': appName})),
        content: Text('App 0.1.0+1\nCore ${a.coreVersion} (build ${a.buildNumber}, ${a.gitSha}, ${a.buildDate})\n'
            '${a.targetOs}-${a.targetArch}\n${_t['coreLicense']}: ${a.coreLicense}\n${_t['providers']}: ${a.providers.join(', ')}'),
        actions: [
          TextButton(
            onPressed: () {
              Navigator.pop(ctx);
              Navigator.push(context, MaterialPageRoute<void>(builder: (_) => const LicensesScreen()));
            },
            child: Text(_t['licenses']),
          ),
          TextButton(onPressed: () => Navigator.pop(ctx), child: Text(_t['close'])),
        ],
      ),
    );
  }
}

class LicensesScreen extends StatelessWidget {
  const LicensesScreen({super.key});

  @override
  Widget build(BuildContext context) {
    final t = UiStrings.current;
    return DefaultTabController(
      length: 3,
      child: Scaffold(
        appBar: AppBar(
          title: Text(t['licenses']),
          bottom: TabBar(tabs: [Tab(text: t['license']), Tab(text: t['notice']), Tab(text: t['thirdParty'])]),
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
