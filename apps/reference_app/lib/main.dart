// ikk-stt reference app. Thin shell over the Rust engine: user profiles
// (provider, language, correction, voice rhythm, filters, trained words),
// general settings (app language, microphone, floating button), the text of
// earlier sessions, API keys in the keychain, consent dialog before audio
// leaves the device, dictation into the focused app with the automatic
// fallback window. AGPL-3.0-or-later.

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
import 'profiles.dart';
import 'history.dart';
import 'background.dart';

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
  // One file per profile ("trained with the profile's voice"); the Default
  // profile keeps the file name of earlier versions.
  late String _voiceDir;
  String get _voiceFile => _profiles.activeIsDefault
      ? '$_voiceDir${Platform.pathSeparator}voice_commands.bin'
      : '$_voiceDir${Platform.pathSeparator}voice_commands_${_profiles.activeId}.bin';
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
  // Audio sent to the provider against audio recorded (this session).
  int _uploadedMs = 0;
  int _recordedMs = 0;
  String _suggestion = '';

  // ------------------------------------------------ shell
  Profiles get _profiles => Profiles(_settings);
  late HistoryStore _history;
  /// 0 profiles, 1 general, 2 history.
  int _tab = 0;
  static const _wizardSteps = 7;
  /// Open wizard: step index, the profile it edits, whether it was just
  /// created (cancel deletes it) and which profile was active before.
  int? _wizardStep;
  String? _wizardId;
  bool _wizardNew = false;
  String? _wizardPrevActive;
  final _nameCtl = TextEditingController();
  final _subCtl = TextEditingController();
  bool get _mobile => Platform.isIOS || Platform.isAndroid;

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
    _voiceDir = localDataDir() ?? dir;
    _uiPath = '$dir${Platform.pathSeparator}ui.json';
    _history = HistoryStore(dir);
    _settings = jsonDecode(await settingsLoad(path: _settingsPath)) as Map<String, dynamic>;
    // First start after the update: the profile list is built from the
    // settings as they are (see Profiles.ensure).
    final hadProfiles = _profiles.byId(kDefaultProfileId) != null;
    _profiles.ensure(
      defaultTitle: _t['defaultProfile'],
      defaultSubtitle: _t['defaultProfileSub'],
      impairedTitle: _t['impairedProfileTitle'],
    );
    if (!hadProfiles) {
      try {
        await settingsSave(path: _settingsPath, json: jsonEncode(_settings));
      } catch (_) {}
    }
    _providers = about().providers;
    try {
      _engine.loadConsent(consentJson: await consentLoad(path: _consentPath));
    } catch (_) {}
    try {
      _mics = await listMics();
    } catch (_) {}
    await _loadKey();
    await _loadLtKey();
    await _loadCorrectorKey('openai');
    await _loadCorrectorKey('anthropic');
    _events = _engine.events().listen(_onEvent);
    _engine.rememberSettings(settingsJson: jsonEncode(_settings));
    await _loadVoice();
    await _loadFloating();
    await _loadBackground();
    await _applyButtonPosition();
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
    if (_correctorKeyCtls.containsKey(_provider)) await _loadCorrectorKey(_provider);
  }

  Future<void> _saveSettings() async {
    _profiles.storeActive();
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
          if (_state == 'Idle' && v != 'Idle') {
            // A new session: its usage starts at zero.
            _uploadedMs = 0;
            _recordedMs = 0;
          }
          final wasRunning = _state != 'Idle';
          _state = v as String;
          if (_state == 'Idle') {
            _interim = '';
            _speech = false;
            if (wasRunning && _finals.isNotEmpty) _history.add(_finals.join('\n'));
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
          if (t.containsKey('Usage')) {
            final u = t['Usage'] as Map<String, dynamic>;
            _uploadedMs = (u['uploaded_ms'] as num).toInt();
            _recordedMs = (u['recorded_ms'] as num).toInt();
          }
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
        case 'Corrected':
          final c = v as Map<String, dynamic>;
          _snack(_t[c['changed'] == true ? 'corrected' : 'nothingToCorrect']);
        case 'CommandRefused':
          _snack(_t.commandRefused(v as String));
        case 'CommandHeard':
          _snack(_t.commandDone(v as String));
        case 'FloatingMoved':
          // Dragged by hand: that is where they open next time, and no
          // saved position is "the chosen one" any more.
          final m = v as Map<String, dynamic>;
          _ui['button_position'] = {'x': (m['x'] as num).toDouble(), 'y': (m['y'] as num).toDouble()};
          _ui.remove('position_selected');
          _saveUi();
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
      // A text checker's consent (live correction, correction button):
      // the engine goes on by itself; only audio consent came from a start.
      if (!textOnly) _start();
    } else {
      _engine.denyConsent(provider: r['id'] as String);
    }
  }

  // ------------------------------------------------ floating button

  final bool _floatingSupported = floatingButtonSupported();
  // Shell preferences outside the engine's settings file (ui.json).
  Map<String, dynamic> _ui = {};

  Future<void> _loadUi() async {
    if (_ui.isNotEmpty) return;
    try {
      final f = File(_uiPath);
      if (await f.exists()) _ui = jsonDecode(await f.readAsString()) as Map<String, dynamic>;
    } catch (_) {}
  }

  Future<void> _saveUi() async {
    try {
      await File(_uiPath).writeAsString(jsonEncode(_ui));
    } catch (_) {}
  }

  /// The buttons are always there on the platforms that have them: the
  /// microphone and the correction button are the way to work without
  /// the window. (An earlier switch could hide them; its stored value is
  /// no longer read.)
  Future<void> _loadFloating() async {
    if (!_floatingSupported) return;
    await _loadUi();
    _engine.showFloatingButton(show_: true);
    if (_shrinkSupported) _engine.setFloatingShrink(on_: _shrinkButtons);
  }

  /// Desktop buttons can shrink away from the mouse (ui.json
  /// `button_shrink`, on unless switched off).
  bool get _shrinkSupported => Platform.isMacOS || Platform.isWindows;
  bool get _shrinkButtons => _ui['button_shrink'] as bool? ?? true;

  // ------------------------------------------------ button positions

  bool get _buttonsMovable => _floatingSupported || Platform.isAndroid;

  List<Map<String, dynamic>> get _savedPositions =>
      ((_ui['positions'] ??= <dynamic>[]) as List).cast<Map<String, dynamic>>();

  /// Where the buttons are right now (screen points; Android: layout x, y).
  Future<List<double>?> _currentButtonPosition() async {
    if (Platform.isAndroid) return hostButtonPosition();
    if (!_floatingSupported) return null;
    final p = _engine.floatingPosition();
    return p.length >= 2 ? [p[0], p[1]] : null;
  }

  Future<void> _moveButtons(double x, double y) async {
    if (Platform.isAndroid) {
      await setHostButtonPosition(x, y);
    } else if (_floatingSupported) {
      _engine.setFloatingPosition(x: x, y: y);
    }
  }

  /// At start: the chosen saved position, else where they were left.
  Future<void> _applyButtonPosition() async {
    if (!_buttonsMovable) return;
    await _loadUi();
    final chosen = _ui['position_selected'] as String?;
    Map<String, dynamic>? target;
    if (chosen != null) {
      target = _savedPositions.where((p) => p['name'] == chosen).firstOrNull;
    }
    target ??= _ui['button_position'] as Map<String, dynamic>?;
    if (target == null) return;
    final x = (target['x'] as num?)?.toDouble();
    final y = (target['y'] as num?)?.toDouble();
    if (x == null || y == null) return;
    // The buttons are created a moment after the show call.
    await Future<void>.delayed(const Duration(milliseconds: 300));
    await _moveButtons(x, y);
  }

  Future<void> _saveCurrentPosition() async {
    final pos = await _currentButtonPosition();
    if (pos == null) {
      _snack(_t['positionUnknown']);
      return;
    }
    final ctl = TextEditingController();
    if (!mounted) return;
    final name = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(_t['savePosition']),
        content: TextField(
          controller: ctl,
          autofocus: true,
          decoration: InputDecoration(labelText: _t['positionName']),
          onSubmitted: (v) => Navigator.pop(ctx, v.trim()),
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx), child: Text(_t['cancel'])),
          FilledButton(onPressed: () => Navigator.pop(ctx, ctl.text.trim()), child: Text(_t['save'])),
        ],
      ),
    );
    if (name == null || name.isEmpty) return;
    final list = _savedPositions;
    list.removeWhere((p) => p['name'] == name);
    list.add({'name': name, 'x': pos[0], 'y': pos[1]});
    _ui['position_selected'] = name;
    _ui['button_position'] = {'x': pos[0], 'y': pos[1]};
    await _saveUi();
    if (mounted) setState(() {});
  }

  Future<void> _choosePosition(Map<String, dynamic> p) async {
    final x = (p['x'] as num).toDouble();
    final y = (p['y'] as num).toDouble();
    await _moveButtons(x, y);
    _ui['position_selected'] = p['name'];
    _ui['button_position'] = {'x': x, 'y': y};
    await _saveUi();
    _snack(_t.fmt('positionApplied', {'n': p['name']}));
    if (mounted) setState(() {});
  }

  Widget _positionsSection() {
    final chosen = _ui['position_selected'] as String?;
    final list = _savedPositions;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['positionsTitle'], style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 4),
            Text(_t['positionsBody']),
            const SizedBox(height: 8),
            if (list.isEmpty) Text(_t['noPositions'], style: Theme.of(context).textTheme.bodySmall),
            for (final p in list)
              ListTile(
                contentPadding: EdgeInsets.zero,
                leading: Icon(p['name'] == chosen ? Icons.radio_button_checked : Icons.radio_button_off),
                title: Text(p['name'] as String),
                subtitle: Text('${(p['x'] as num).round()} · ${(p['y'] as num).round()}'),
                onTap: () => _choosePosition(p),
                trailing: IconButton(
                  tooltip: _t['remove'],
                  icon: const Icon(Icons.delete_outline),
                  onPressed: () async {
                    list.remove(p);
                    if (chosen == p['name']) _ui.remove('position_selected');
                    await _saveUi();
                    if (mounted) setState(() {});
                  },
                ),
              ),
            const SizedBox(height: 4),
            FilledButton.tonalIcon(
              onPressed: _saveCurrentPosition,
              icon: const Icon(Icons.add_location_alt_outlined),
              label: Text(_t['savePosition']),
            ),
          ],
        ),
      ),
    );
  }

  // ------------------------------------------------ background mode

  bool _keepRunning = true;
  /// Registered to start with the session; null while unknown.
  bool? _loginItem;

  Future<void> _loadBackground() async {
    if (!BackgroundControl.supported) return;
    await _loadUi();
    _keepRunning = _ui['keep_running'] as bool? ?? true;
    await BackgroundControl.setKeepRunning(_keepRunning);
    _loginItem = await BackgroundControl.isLoginItem();
  }

  Widget _backgroundSection() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['backgroundTitle'], style: Theme.of(context).textTheme.titleMedium),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['keepRunning']),
              subtitle: Text(_t['keepRunningBody']),
              value: _keepRunning,
              onChanged: (on) async {
                _keepRunning = on;
                _ui['keep_running'] = on;
                await BackgroundControl.setKeepRunning(on);
                await _saveUi();
                if (mounted) setState(() {});
              },
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['startAtLogin']),
              subtitle: Text(_t['startAtLoginBody']),
              value: _loginItem ?? false,
              onChanged: _loginItem == null
                  ? null
                  : (on) async {
                      final (state, error) = await BackgroundControl.setLoginItem(on);
                      if (error != null) _snack(_t.fmt('loginItemFailed', {'e': error}));
                      if (state != null) _loginItem = state;
                      if (mounted) setState(() {});
                    },
            ),
          ],
        ),
      ),
    );
  }

  Widget _floatingSection() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['floatingTitle'], style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 4),
            Text(_t['floatingBody']),
            const SizedBox(height: 4),
            Text(_t['floatingNote'], style: Theme.of(context).textTheme.bodySmall),
            if (_shrinkSupported)
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: Text(_t['shrinkButtons']),
                subtitle: Text(_t['shrinkButtonsBody']),
                value: _shrinkButtons,
                onChanged: (on) async {
                  _ui['button_shrink'] = on;
                  _engine.setFloatingShrink(on_: on);
                  await _saveUi();
                  if (mounted) setState(() {});
                },
              ),
          ],
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
            // A slow speaker's two-word command needs time; the room is
            // measured first as well.
            final take = await voiceRecordTake(settingsJson: jsonEncode(_settings), timeoutSecs: 12);
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
    _nameCtl.dispose();
    _subCtl.dispose();
    for (final c in _correctorKeyCtls.values) {
      c.dispose();
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    // The widgets without settings at hand (licences page, platform cards,
    // history) read the language from here.
    UiStrings.current = _t;
    if (_settings.isEmpty) return const Scaffold(body: Center(child: CircularProgressIndicator()));
    if (_wizardStep != null) return _wizardScaffold();
    return _mobile ? _mobileShell() : _desktopShell();
  }

  bool get _listening => _state == 'Listening' || _state == 'Finishing';

  /// Phones and tablets: three tabs at the bottom, the profile chips as a
  /// row at the top of the Profiles tab. The in-app microphone button
  /// stays where no floating button exists (iOS).
  Widget _mobileShell() {
    final titles = [_t['profiles'], _t['general'], _t['history']];
    return Scaffold(
      appBar: AppBar(
        title: Text(titles[_tab]),
        actions: [IconButton(icon: const Icon(Icons.info_outline), tooltip: _t['about'], onPressed: () => _showAbout(context))],
      ),
      body: switch (_tab) {
        0 => _profilePage(showChips: true),
        1 => _generalPage(),
        _ => HistoryPane(key: ValueKey(_history), store: _history),
      },
      floatingActionButton: _floatingSupported ? null : _micFab(),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (i) => setState(() => _tab = i),
        destinations: [
          NavigationDestination(icon: const Icon(Icons.people_outline), selectedIcon: const Icon(Icons.people), label: _t['profiles']),
          NavigationDestination(icon: const Icon(Icons.tune), label: _t['general']),
          NavigationDestination(icon: const Icon(Icons.history), label: _t['history']),
        ],
      ),
    );
  }

  /// Desktop: sidebar with the profiles, General and History on the left,
  /// the chosen page on the right.
  Widget _desktopShell() {
    final scheme = Theme.of(context).colorScheme;
    final profiles = _profiles.list;
    final activeId = _profiles.activeId;
    final title = switch (_tab) {
      0 => _profiles.active['title'] as String? ?? '',
      1 => _t['general'],
      _ => _t['history'],
    };
    return Scaffold(
      body: Row(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          SizedBox(
            width: 272,
            child: Material(
              color: scheme.surfaceContainerLow,
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Padding(
                    padding: const EdgeInsets.fromLTRB(20, 20, 20, 8),
                    child: Text(appName, style: Theme.of(context).textTheme.titleLarge),
                  ),
                  Padding(
                    padding: const EdgeInsets.fromLTRB(20, 8, 20, 4),
                    child: Text(_t['profiles'].toUpperCase(),
                        style: Theme.of(context).textTheme.labelSmall?.copyWith(color: scheme.onSurfaceVariant, letterSpacing: 1)),
                  ),
                  Expanded(
                    child: ListView(
                      padding: const EdgeInsets.symmetric(horizontal: 12),
                      children: [
                        for (final p in profiles)
                          Padding(
                            padding: const EdgeInsets.only(bottom: 6),
                            child: ProfileChip(
                              profile: p,
                              selected: _tab == 0 && p['id'] == activeId,
                              onTap: () => _activateProfile(p['id'] as String),
                            ),
                          ),
                        AddProfileChip(label: _t['newProfile'], onTap: () => _startWizard()),
                      ],
                    ),
                  ),
                  const Divider(height: 1),
                  ListTile(
                    leading: const Icon(Icons.tune),
                    title: Text(_t['general']),
                    selected: _tab == 1,
                    onTap: () => setState(() => _tab = 1),
                  ),
                  ListTile(
                    leading: const Icon(Icons.history),
                    title: Text(_t['history']),
                    selected: _tab == 2,
                    onTap: () => setState(() => _tab = 2),
                  ),
                  ListTile(
                    leading: const Icon(Icons.info_outline),
                    title: Text(_t['about']),
                    onTap: () => _showAbout(context),
                  ),
                  const SizedBox(height: 8),
                ],
              ),
            ),
          ),
          const VerticalDivider(width: 1),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Padding(
                  padding: const EdgeInsets.fromLTRB(24, 20, 24, 0),
                  child: Text(title, style: Theme.of(context).textTheme.headlineSmall),
                ),
                Expanded(
                  child: switch (_tab) {
                    0 => _profilePage(showChips: false),
                    1 => _generalPage(),
                    _ => HistoryPane(key: ValueKey(_history), store: _history, embedded: true),
                  },
                ),
              ],
            ),
          ),
        ],
      ),
      floatingActionButton: _floatingSupported ? null : _micFab(),
    );
  }

  Widget _micFab() => FloatingActionButton.extended(
        onPressed: _listening ? _stop : _start,
        icon: Icon(_listening ? Icons.stop : Icons.mic),
        label: Text(_t[_listening ? 'stop' : 'dictate']),
      );

  // ------------------------------------------------ profiles

  Future<void> _activateProfile(String id) async {
    if (id != _profiles.activeId) {
      if (_listening) _stop();
      _profiles.activate(id);
      await _saveSettings();
      await _loadKey();
      _voiceModel = null;
      _voiceOn = false;
      await _loadVoice();
      _applyVoice();
      _snack(_t.fmt('profileSwitched', {'n': _profiles.active['title']}));
    }
    if (mounted) setState(() => _tab = 0);
  }

  Future<void> _deleteProfile(Map<String, dynamic> p) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(_t['deleteProfile']),
        content: Text(_t.fmt('deleteProfileBody', {'n': p['title']})),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx, false), child: Text(_t['cancel'])),
          FilledButton(onPressed: () => Navigator.pop(ctx, true), child: Text(_t['remove'])),
        ],
      ),
    );
    if (ok != true) return;
    final id = p['id'] as String;
    final wasActive = id == _profiles.activeId;
    if (wasActive && _listening) _stop();
    try {
      final f = File('$_voiceDir${Platform.pathSeparator}voice_commands_$id.bin');
      if (await f.exists()) await f.delete();
    } catch (_) {}
    _profiles.delete(id);
    await _saveSettings();
    if (wasActive) {
      await _loadKey();
      _voiceModel = null;
      _voiceOn = false;
      await _loadVoice();
      _applyVoice();
    }
    if (mounted) setState(() {});
  }

  /// The active profile: chips (phone), header, the six sections, status.
  Widget _profilePage({required bool showChips}) {
    final p = _profiles.active;
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        if (showChips) ...[
          SizedBox(
            height: 60,
            child: ListView(
              scrollDirection: Axis.horizontal,
              children: [
                for (final q in _profiles.list)
                  Padding(
                    padding: const EdgeInsets.only(right: 8),
                    child: ProfileChip(
                      profile: q,
                      selected: q['id'] == p['id'],
                      compact: true,
                      onTap: () => _activateProfile(q['id'] as String),
                    ),
                  ),
                AddProfileChip(label: _t['newProfile'], compact: true, onTap: () => _startWizard()),
              ],
            ),
          ),
          const SizedBox(height: 16),
        ],
        _profileHeader(p),
        const SizedBox(height: 16),
        _providerSection(),
        const SizedBox(height: 16),
        _languageSection(),
        const SizedBox(height: 16),
        _correctionSection(),
        const SizedBox(height: 16),
        _impairedSection(),
        const SizedBox(height: 16),
        _filtersSection(),
        const SizedBox(height: 16),
        _commandsSection(),
        const SizedBox(height: 16),
        _statusStrip(),
      ],
    );
  }

  Widget _profileHeader(Map<String, dynamic> p) {
    final color = profileColor(p);
    final isDefault = p['id'] == kDefaultProfileId;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Row(children: [
          Container(
            width: 48,
            height: 48,
            decoration: BoxDecoration(color: color, borderRadius: BorderRadius.circular(14)),
            child: Icon(profileIcon(p), color: Colors.white, size: 26),
          ),
          const SizedBox(width: 12),
          Expanded(
            child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
              Text(p['title'] as String? ?? '', style: Theme.of(context).textTheme.titleMedium),
              Text(
                ((p['subtitle'] as String?)?.isNotEmpty ?? false) ? p['subtitle'] as String : _t['profileSettings'],
                style: Theme.of(context).textTheme.bodySmall,
              ),
            ]),
          ),
          IconButton(
            tooltip: _t['editProfile'],
            icon: const Icon(Icons.edit_outlined),
            onPressed: () => _startWizard(id: p['id'] as String),
          ),
          if (!isDefault)
            IconButton(
              tooltip: _t['deleteProfile'],
              icon: const Icon(Icons.delete_outline),
              onPressed: () => _deleteProfile(p),
            ),
        ]),
      ),
    );
  }

  /// What the next session runs with, what it costs, and what is going on
  /// right now (state, level, errors, the fallback window's text).
  Widget _statusStrip() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(_activeSummary(), style: Theme.of(context).textTheme.bodySmall),
            if (_recordedMs > 0)
              Text(
                _t.fmt('usageLine', {
                  'u': _clock(_uploadedMs),
                  'r': _clock(_recordedMs),
                  'x': (_uploadedMs / _recordedMs).toStringAsFixed(1),
                }),
                style: Theme.of(context).textTheme.bodySmall,
              ),
            const SizedBox(height: 8),
            // What changes during dictation sits behind one fixed
            // accessibility node per area whose label changes: nodes
            // appearing and vanishing several times a second made Flutter's
            // Windows accessibility bridge log "Failed to update ui::AXTree".
            Semantics(
              container: true,
              label: _t.fmt('status', {'s': _state}),
              child: ExcludeSemantics(child: Text('$_state${_speech ? ' · ${_t['speech']}' : ''}')),
            ),
            const SizedBox(height: 6),
            ExcludeSemantics(child: LinearProgressIndicator(value: ((_level + 60) / 60).clamp(0.0, 1.0))),
            if (_error != null) ...[
              const SizedBox(height: 8),
              Text(_error!, style: TextStyle(color: Theme.of(context).colorScheme.error)),
            ],
            if (_suggestion.isNotEmpty) Padding(padding: const EdgeInsets.only(top: 4), child: Text(_t.fmt('suggested', {'s': _suggestion}))),
            if (_fallbackReason != null) ...[const SizedBox(height: 8), _fallbackCard()],
            if (_interim.isNotEmpty) ...[
              const SizedBox(height: 8),
              ExcludeSemantics(child: Text(_interim, style: const TextStyle(fontStyle: FontStyle.italic))),
            ],
            if (_finals.isNotEmpty) ...[
              const SizedBox(height: 8),
              Semantics(
                container: true,
                label: _t.fmt('lastRecognised', {'s': _finals.last}),
                child: ExcludeSemantics(child: Text(_finals.last, maxLines: 3, overflow: TextOverflow.ellipsis)),
              ),
            ],
          ],
        ),
      ),
    );
  }

  // ------------------------------------------------ general settings

  Widget _generalPage() {
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        _appLanguageSection(),
        const SizedBox(height: 16),
        _micSection(),
        if (_floatingSupported) ...[const SizedBox(height: 16), _floatingSection()],
        if (_buttonsMovable) ...[const SizedBox(height: 16), _positionsSection()],
        if (BackgroundControl.supported) ...[const SizedBox(height: 16), _backgroundSection()],
        if (otherAppsCard() case final card?) ...[const SizedBox(height: 16), card],
      ],
    );
  }

  Widget _appLanguageSection() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t.appLanguage, style: Theme.of(context).textTheme.titleMedium),
            DropdownButton<String>(
              value: _appLanguage.isEmpty ? 'eng' : _appLanguage,
              isExpanded: true,
              items: [for (final code in UiStrings.codes) DropdownMenuItem(value: code, child: Text(UiStrings.of(code).name))],
              onChanged: (code) {
                if (code == null) return;
                (_settings['app'] ??= <String, dynamic>{})['language'] = code == 'eng' ? '' : code;
                _saveSettings();
              },
            ),
          ],
        ),
      ),
    );
  }

  // ------------------------------------------------ wizard

  Map<String, dynamic>? get _wizardProfile => _wizardId == null ? null : _profiles.byId(_wizardId!);

  /// New profile (no [id]): created from the active one with the
  /// impaired preset and made active, so the steps edit it live. An
  /// existing profile is made active first for the same reason.
  Future<void> _startWizard({String? id}) async {
    if (_listening) _stop();
    _wizardPrevActive = _profiles.activeId;
    if (id == null) {
      final n = _profiles.list.length;
      final p = _profiles.create(title: '${_t['newProfile']} $n', impaired: true, color: n % kProfileColors.length);
      _wizardNew = true;
      id = p['id'] as String;
    } else {
      _wizardNew = false;
    }
    if (id != _profiles.activeId) {
      _profiles.activate(id);
      await _saveSettings();
      await _loadKey();
      _voiceModel = null;
      _voiceOn = false;
      await _loadVoice();
      _applyVoice();
    }
    final p = _profiles.byId(id)!;
    _nameCtl.text = p['title'] as String? ?? '';
    _subCtl.text = p['subtitle'] as String? ?? '';
    if (mounted) {
      setState(() {
        _wizardId = id;
        _wizardStep = 0;
        _tab = 0;
      });
    }
  }

  Future<void> _finishWizard() async {
    final p = _wizardProfile;
    if (p != null && (p['title'] as String? ?? '').trim().isEmpty) p['title'] = _t['newProfile'];
    await _saveSettings();
    if (mounted) setState(() => _wizardStep = null);
  }

  Future<void> _cancelWizard() async {
    if (_wizardNew && _wizardId != null) {
      final id = _wizardId!;
      _profiles.delete(id);
      await _saveSettings();
      if (_wizardPrevActive != null && _profiles.byId(_wizardPrevActive!) != null && _wizardPrevActive != _profiles.activeId) {
        _profiles.activate(_wizardPrevActive!);
        await _saveSettings();
      }
      await _loadKey();
      _voiceModel = null;
      _voiceOn = false;
      await _loadVoice();
      _applyVoice();
    } else {
      // Edits of an existing profile are saved as they are made.
      await _saveSettings();
    }
    if (mounted) setState(() => _wizardStep = null);
  }

  Widget _wizardScaffold() {
    final step = _wizardStep!;
    final p = _wizardProfile;
    if (p == null) {
      WidgetsBinding.instance.addPostFrameCallback((_) => setState(() => _wizardStep = null));
      return const Scaffold(body: SizedBox.shrink());
    }
    final titles = ['stepProfile', 'stepProvider', 'stepLanguage', 'stepCorrection', 'stepImpaired', 'stepFilters', 'stepCommands'];
    final body = switch (step) {
      0 => _identityStep(p),
      1 => _providerSection(),
      2 => _languageSection(),
      3 => _correctionSection(),
      4 => _impairedSection(),
      5 => _filtersSection(),
      _ => _commandsSection(),
    };
    final last = step == _wizardSteps - 1;
    return PopScope(
      canPop: false,
      onPopInvokedWithResult: (didPop, _) {
        if (!didPop) {
          if (step > 0) {
            setState(() => _wizardStep = step - 1);
          } else {
            _cancelWizard();
          }
        }
      },
      child: Scaffold(
        appBar: AppBar(
          leading: IconButton(icon: const Icon(Icons.close), tooltip: _t['cancel'], onPressed: _cancelWizard),
          title: Text(_t.fmt('stepOf', {'n': step + 1, 't': _wizardSteps})),
        ),
        body: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 720),
            child: ListView(
              padding: const EdgeInsets.all(16),
              children: [
                LinearProgressIndicator(value: (step + 1) / _wizardSteps),
                const SizedBox(height: 16),
                Text(_t[titles[step]], style: Theme.of(context).textTheme.headlineSmall),
                const SizedBox(height: 4),
                Text(_t['${titles[step]}Body']),
                const SizedBox(height: 16),
                body,
              ],
            ),
          ),
        ),
        bottomNavigationBar: SafeArea(
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Row(children: [
              if (step > 0)
                OutlinedButton.icon(
                  onPressed: () => setState(() => _wizardStep = step - 1),
                  icon: const Icon(Icons.chevron_left),
                  label: Text(_t['back']),
                ),
              const Spacer(),
              FilledButton.icon(
                onPressed: last ? _finishWizard : () => setState(() => _wizardStep = step + 1),
                icon: Icon(last ? Icons.check : Icons.chevron_right),
                label: Text(_t[last ? 'finish' : 'next']),
              ),
            ]),
          ),
        ),
      ),
    );
  }

  /// Step 0: name, subtitle, icon and colour.
  Widget _identityStep(Map<String, dynamic> p) {
    final isDefault = p['id'] == kDefaultProfileId;
    final color = profileColor(p);
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(children: [
              Container(
                width: 56,
                height: 56,
                decoration: BoxDecoration(color: color, borderRadius: BorderRadius.circular(16)),
                child: Icon(profileIcon(p), color: Colors.white, size: 30),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                  Text(p['title'] as String? ?? '', style: Theme.of(context).textTheme.titleMedium),
                  if ((p['subtitle'] as String? ?? '').isNotEmpty) Text(p['subtitle'] as String),
                ]),
              ),
            ]),
            const SizedBox(height: 12),
            TextField(
              controller: _nameCtl,
              enabled: !isDefault,
              decoration: InputDecoration(labelText: _t['profileName'], helperText: isDefault ? _t['defaultFixedName'] : null),
              onChanged: (v) => setState(() => p['title'] = v),
            ),
            TextField(
              controller: _subCtl,
              decoration: InputDecoration(labelText: _t['profileSubtitle']),
              onChanged: (v) => setState(() => p['subtitle'] = v),
            ),
            const SizedBox(height: 16),
            Text(_t['profileIcon'], style: Theme.of(context).textTheme.titleSmall),
            const SizedBox(height: 4),
            Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                for (final e in kProfileIcons.entries)
                  Semantics(
                    button: true,
                    selected: p['icon'] == e.key,
                    label: e.key,
                    child: Material(
                      color: p['icon'] == e.key ? color : Theme.of(context).colorScheme.surfaceContainerHighest,
                      borderRadius: BorderRadius.circular(12),
                      child: InkWell(
                        borderRadius: BorderRadius.circular(12),
                        onTap: () => setState(() => p['icon'] = e.key),
                        child: SizedBox(
                          width: 48,
                          height: 48,
                          child: Icon(e.value, color: p['icon'] == e.key ? Colors.white : null),
                        ),
                      ),
                    ),
                  ),
              ],
            ),
            const SizedBox(height: 16),
            Text(_t['profileColor'], style: Theme.of(context).textTheme.titleSmall),
            const SizedBox(height: 4),
            Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                for (var i = 0; i < kProfileColors.length; i++)
                  Semantics(
                    button: true,
                    selected: (p['color'] as num?)?.toInt() == i,
                    label: '${_t['profileColor']} ${i + 1}',
                    child: InkWell(
                      borderRadius: BorderRadius.circular(24),
                      onTap: () => setState(() => p['color'] = i),
                      child: Container(
                        width: 44,
                        height: 44,
                        decoration: BoxDecoration(
                          color: kProfileColors[i],
                          shape: BoxShape.circle,
                          border: Border.all(
                            color: (p['color'] as num?)?.toInt() == i ? Theme.of(context).colorScheme.onSurface : Colors.transparent,
                            width: 3,
                          ),
                        ),
                        child: (p['color'] as num?)?.toInt() == i ? const Icon(Icons.check, color: Colors.white) : null,
                      ),
                    ),
                  ),
              ],
            ),
          ],
        ),
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

  /// The floating buttons' look in the cards: a grey disc, a black glyph.
  static Widget _badge(IconData icon) => CircleAvatar(
        radius: 16,
        backgroundColor: const Color(0xFFDDDDDD),
        child: Icon(icon, size: 18, color: Colors.black),
      );

  Map<String, dynamic> get _providerOpts =>
      (_settings['providers']['options'] as Map<String, dynamic>)[_provider] as Map<String, dynamic>? ?? {};

  Widget _providerSection() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(children: [
              _badge(Icons.mic),
              const SizedBox(width: 10),
              Text(_t['providerTitle'], style: Theme.of(context).textTheme.titleMedium),
            ]),
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

  // Keys of the text-model correctors, stored like provider keys
  // (`api_key:<id>`). The OpenAI key is the speech provider's key as well:
  // entered in either place, it serves both.
  final _correctorKeyCtls = {'openai': TextEditingController(), 'anthropic': TextEditingController()};
  final Map<String, (bool, String?)> _correctorKeys = {};

  Future<void> _loadCorrectorKey(String id) async {
    try {
      final k = await _secure.read(key: 'api_key:$id') ?? '';
      _correctorKeys[id] = (k.isNotEmpty, k.isEmpty ? null : k.substring(k.length > 4 ? k.length - 4 : 0));
      if (k.isNotEmpty) _engine.setCredential(provider: id, secret: k);
    } catch (_) {
      _correctorKeys[id] = (false, null);
    }
    if (mounted) setState(() {});
  }

  Future<void> _saveCorrectorKey(String id) async {
    final k = _correctorKeyCtls[id]!.text.trim();
    if (k.isEmpty) return;
    try {
      _engine.setCredential(provider: id, secret: k);
      await _secure.write(key: 'api_key:$id', value: k);
      _correctorKeyCtls[id]!.text = '';
      _snack(_t.keySaved(_keyStoreName));
    } catch (e) {
      _snack(_t.savingFailed('$e'));
    }
    await _loadCorrectorKey(id);
    // The speech provider's card shows the same key.
    if (id == _provider) await _loadKey();
    if (mounted) setState(() {});
  }

  Future<void> _removeCorrectorKey(String id) async {
    try {
      await _secure.delete(key: 'api_key:$id');
      _engine.setCredential(provider: id, secret: '');
    } catch (_) {}
    await _loadCorrectorKey(id);
    if (id == _provider) await _loadKey();
    if (mounted) setState(() {});
  }

  /// Key status and entry for a text-model corrector.
  List<Widget> _correctorKeyRows(String id, String name) {
    final (stored, tail) = _correctorKeys[id] ?? (false, null);
    return [
      Row(children: [
        Icon(stored ? Icons.check_circle : Icons.key_off, size: 18, color: stored ? Colors.green.shade700 : Colors.orange.shade800),
        const SizedBox(width: 8),
        Expanded(
          child: Text(stored
              ? _t.fmt('keyFound', {'p': name, 's': _keyStoreName, 'k': tail ?? ''})
              : _t.fmt('keyNone', {'p': name})),
        ),
        if (stored) TextButton(onPressed: () => _removeCorrectorKey(id), child: Text(_t['remove'])),
      ]),
      Row(children: [
        Expanded(
          child: TextField(
            controller: _correctorKeyCtls[id],
            obscureText: true,
            decoration: InputDecoration(
              labelText: stored ? _t['newKey'] : _t.fmt('apiKeyFor', {'p': name}),
              helperText: id == 'openai' ? _t['openAiKeyShared'] : null,
            ),
            onSubmitted: (_) => _saveCorrectorKey(id),
          ),
        ),
        const SizedBox(width: 8),
        FilledButton(onPressed: () => _saveCorrectorKey(id), child: Text(_t['save'])),
      ]),
    ];
  }

  Map<String, dynamic> get _text => (_settings['text'] ??= <String, dynamic>{}) as Map<String, dynamic>;

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
    final corrector = _text['corrector'] as String? ?? 'languagetool';
    final range = _text['correction_range'] as String? ?? 'cursor_to_end';
    final aiModelCtl = TextEditingController(text: _text['ai_model'] as String? ?? '');
    aiModelCtl.selection = TextSelection.collapsed(offset: aiModelCtl.text.length);
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(children: [
              _badge(Icons.auto_fix_high),
              const SizedBox(width: 10),
              Text(_t['correctionTitle'], style: Theme.of(context).textTheme.titleMedium),
            ]),
            // Who corrects: rules (LanguageTool) or a text model, for the
            // live correction and the correction button alike — and right
            // under it the account or key that corrector needs.
            Text(_t['correctorTitle'], style: Theme.of(context).textTheme.titleSmall),
            DropdownButton<String>(
              value: corrector,
              isExpanded: true,
              items: [
                DropdownMenuItem(value: 'ondevice', child: Text(_t['correctorOnDevice'])),
                DropdownMenuItem(value: 'languagetool', child: Text(_t['correctorLanguageTool'])),
                DropdownMenuItem(value: 'openai', child: Text(_t['correctorOpenAi'])),
                DropdownMenuItem(value: 'anthropic', child: Text(_t['correctorAnthropic'])),
              ],
              onChanged: (c) {
                if (c == null) return;
                _text['corrector'] = c;
                _saveSettings();
              },
            ),
            if (corrector == 'ondevice')
              Padding(
                padding: const EdgeInsets.only(top: 4, bottom: 8),
                child: Text(_t['onDeviceBody'], style: Theme.of(context).textTheme.bodySmall),
              ),
            if (corrector == 'languagetool') ...[
              Row(children: [
                Icon(_ltStored ? Icons.check_circle : Icons.key_off,
                    size: 18, color: _ltStored ? Colors.green.shade700 : Colors.orange.shade800),
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
                    onSubmitted: (_) => _saveLtKey(),
                  ),
                ),
                const SizedBox(width: 8),
                FilledButton(onPressed: _saveLtKey, child: Text(_t['save'])),
              ]),
            ],
            if (corrector == 'openai') ..._correctorKeyRows('openai', 'OpenAI'),
            if (corrector == 'anthropic') ..._correctorKeyRows('anthropic', 'Anthropic'),
            if (corrector != 'ondevice')
              Padding(
                padding: const EdgeInsets.only(top: 4),
                child: Text(_t['fallbackOnDevice'], style: Theme.of(context).textTheme.bodySmall),
              ),
            if (corrector == 'openai' || corrector == 'anthropic')
              TextField(
                controller: aiModelCtl,
                decoration: InputDecoration(labelText: _t['aiModel'], helperText: _t['aiModelHelper']),
                onSubmitted: (v) {
                  _text['ai_model'] = v.trim();
                  _saveSettings();
                },
                onTapOutside: (_) {
                  if ((_text['ai_model'] as String? ?? '') != aiModelCtl.text.trim()) {
                    _text['ai_model'] = aiModelCtl.text.trim();
                    _saveSettings();
                  }
                },
              ),
            const SizedBox(height: 12),
            // What the correction button next to the microphone corrects.
            Text(_t['rangeTitle'], style: Theme.of(context).textTheme.titleSmall),
            const SizedBox(height: 6),
            SegmentedButton<String>(
              showSelectedIcon: false,
              segments: [
                ButtonSegment(value: 'click_to_click', label: Text(_t['rangeClickToClick'])),
                ButtonSegment(value: 'cursor_to_end', label: Text(_t['rangeCursorToEnd'])),
                ButtonSegment(value: 'whole_text', label: Text(_t['rangeWholeText'])),
              ],
              selected: {range},
              onSelectionChanged: (sel) {
                _text['correction_range'] = sel.first;
                _saveSettings();
              },
            ),
            const SizedBox(height: 4),
            Text(_t[range == 'click_to_click' ? 'rangeClickNote' : 'rangeNote'], style: Theme.of(context).textTheme.bodySmall),
            const Divider(height: 24),
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
          ],
        ),
      ),
    );
  }

  /// The fine-tuning folds (and the test entries of the microphone list)
  /// are open.
  bool _advanced = false;

  /// Transcription language and the words the recogniser should expect.
  Widget _languageSection() {
    final opts = _providerOpts;
    final vocab = ((opts['keywords'] as List?) ?? const []).join(', ');
    if (_vocabCtl.text != vocab && !_vocabFocused) _vocabCtl.text = vocab;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['stepLanguage'], style: Theme.of(context).textTheme.titleMedium),
            DropdownButton<String>(
              value: _settings['providers']['language'] as String,
              isExpanded: true,
              items: kLanguages.entries.map((e) => DropdownMenuItem(value: e.key, child: Text(e.value))).toList(),
              onChanged: (l) {
                _settings['providers']['language'] = l;
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
            _genderColonSwitch(),
            _genderColonField(),
          ],
        ),
      ),
    );
  }

  /// German: the gender colon for every feminine plural the word list
  /// knows, plus the stems below.
  Widget _genderColonSwitch() {
    return SwitchListTile(
      contentPadding: EdgeInsets.zero,
      title: Text(_t['genderColonAuto']),
      subtitle: Text(_t['genderColonAutoBody']),
      value: _text['gender_colon_auto'] as bool? ?? true,
      onChanged: (b) {
        _text['gender_colon_auto'] = b;
        _saveSettings();
      },
    );
  }

  /// Word stems that get the gender colon in their feminine plural.
  Widget _genderColonField() {
    final words = ((_text['gender_colon_words'] as List?) ?? const ['Autor', 'Lektor', 'Leser']).join(', ');
    final ctl = TextEditingController(text: words);
    ctl.selection = TextSelection.collapsed(offset: words.length);
    void save(String v) {
      _text['gender_colon_words'] = v.split(',').map((w) => w.trim()).where((w) => w.isNotEmpty).toList();
      _saveSettings();
    }

    return TextField(
      controller: ctl,
      decoration: InputDecoration(labelText: _t['genderColonLabel'], helperText: _t['genderColonHelper']),
      onSubmitted: save,
      onTapOutside: (_) {
        if (ctl.text != words) save(ctl.text);
      },
    );
  }

  /// Voice and rhythm: the clear-speech or impaired preset, batch upload,
  /// and how the recording is cut (chunks and pause, or readings of the
  /// thought so far with agreed words only).
  Widget _impairedSection() {
    final sound = _settings['sound'] as Map<String, dynamic>;
    final vad = sound['vad'] as Map<String, dynamic>;
    final impaired = sound['impaired_voice'] != false;
    final opts = _providerOpts;
    final batch = (opts['transfer'] ?? 'Batch') == 'Batch';
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['voiceKind'], style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            SegmentedButton<bool>(
              segments: [
                ButtonSegment(value: false, label: Text(_t['clearSpeech']), icon: const Icon(Icons.record_voice_over_outlined)),
                ButtonSegment(value: true, label: Text(_t['impairedSpeech']), icon: const Icon(Icons.accessibility_new)),
              ],
              selected: {impaired},
              onSelectionChanged: (sel) {
                setState(() => Profiles.applyVoicePreset(_settings, sel.first));
                _saveSettings();
              },
            ),
            const SizedBox(height: 8),
            Text(_t[impaired ? 'impairedOn' : 'impairedOff'], style: Theme.of(context).textTheme.bodySmall),
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
            if (impaired) ...[
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
                  if (vad['agreement'] == true) ...[
                    _stepper(
                      label: _t['agreementStep'],
                      value: (vad['agreement_step_ms'] as num? ?? 1000),
                      min: 500,
                      max: 3000,
                      step: 250,
                      unit: 'ms',
                      onChanged: (v) => vad['agreement_step_ms'] = v.round(),
                    ),
                    _stepper(
                      label: _t['agreementSpan'],
                      value: (vad['agreement_span_ms'] as num? ?? 8000) / 1000,
                      min: 4,
                      max: 20,
                      step: 1,
                      unit: 's',
                      onChanged: (v) => vad['agreement_span_ms'] = (v * 1000).round(),
                    ),
                  ],
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
                ],
              ),
            ] else
              ExpansionTile(
                tilePadding: EdgeInsets.zero,
                title: Text(_t['advanced']),
                subtitle: Text(_t['advGainChunk']),
                initiallyExpanded: _advanced,
                onExpansionChanged: (open) => setState(() => _advanced = open),
                children: [
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
              ),
          ],
        ),
      ),
    );
  }

  /// What is done to the sound before it leaves the device.
  Widget _filtersSection() {
    final sound = _settings['sound'] as Map<String, dynamic>;
    final impaired = sound['impaired_voice'] != false;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(_t['stepFilters'], style: Theme.of(context).textTheme.titleMedium),
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
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: Text(_t['rnnoise']),
              subtitle: impaired ? Text(_t['rnnoiseImpairedBody']) : null,
              value: sound['denoise'] != 'Off',
              onChanged: (b) {
                sound['denoise'] = b ? 'RnNoise' : 'Off';
                _saveSettings();
              },
            ),
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
            _stepper(
              label: _t['preamp'],
              value: sound['preamp_gain_db'] as num,
              min: 0,
              max: 24,
              step: 1,
              unit: 'dB',
              onChanged: (v) => sound['preamp_gain_db'] = v.toDouble(),
            ),
          ],
        ),
      ),
    );
  }

  /// Start and stop by voice, and the trained words.
  Widget _commandsSection() {
    return Column(children: [
      _voiceSection(),
      const SizedBox(height: 16),
      _trainingSection(),
    ]);
  }

  /// Milliseconds as m:ss.
  static String _clock(int ms) {
    final s = ms ~/ 1000;
    return '${s ~/ 60}:${(s % 60).toString().padLeft(2, '0')}';
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
        content: Text('${_t.fmt('aboutVersion', {'v': a.coreVersion, 'b': a.buildNumber, 'd': a.buildDate})}\n'
            '${_t['providers']}: ${a.providers.join(', ')}'),
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
      length: 2,
      child: Scaffold(
        appBar: AppBar(
          title: Text(t['licenses']),
          bottom: TabBar(tabs: [Tab(text: t['license']), Tab(text: t['notice'])]),
        ),
        body: TabBarView(
          children: [licenseText(), noticeText()]
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
