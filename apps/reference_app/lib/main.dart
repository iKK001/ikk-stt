// ikk-stt reference app. Thin shell over the Rust engine: settings in three
// areas (provider, sound, microphone), API key in the keychain, consent
// dialog before audio leaves the device, dictation into the focused app with
// the automatic fallback window. AGPL-3.0-or-later.

import 'dart:async';
import 'dart:convert';
import 'dart:io';

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
  'deu': 'Deutsch / Schweizerdeutsch',
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
  Map<String, dynamic> _settings = {};
  List<String> _providers = [];
  List<InputDevice> _mics = [];
  final _keyCtl = TextEditingController();
  bool _keyStored = false;

  String _state = 'Idle';
  double _level = -90;
  bool _speech = false;
  String _interim = '';
  final List<String> _finals = [];
  String? _fallbackReason;
  String _fallbackText = '';
  String? _error;

  @override
  void initState() {
    super.initState();
    _init();
  }

  Future<void> _init() async {
    final dir = configDir() ?? (await getApplicationSupportDirectory()).path;
    _settingsPath = '$dir${Platform.pathSeparator}settings.json';
    _consentPath = '$dir${Platform.pathSeparator}consent.json';
    _settings = jsonDecode(await settingsLoad(path: _settingsPath)) as Map<String, dynamic>;
    _providers = about().providers;
    try {
      _engine.loadConsent(consentJson: await consentLoad(path: _consentPath));
    } catch (_) {}
    try {
      _mics = await listMics();
    } catch (_) {}
    await _loadKey();
    _events = _engine.events().listen(_onEvent);
    _engine.rememberSettings(settingsJson: jsonEncode(_settings));
    if (mounted) setState(() {});
  }

  String get _provider => (_settings['providers']?['active'] as String?) ?? 'deepgram';

  Future<void> _loadKey() async {
    String k = '';
    try {
      k = await _secure.read(key: 'api_key:$_provider') ?? '';
    } catch (e) {
      _error = 'Keychain read failed: $e';
    }
    _keyStored = k.isNotEmpty;
    if (k.isNotEmpty) _engine.setCredential(provider: _provider, secret: k);
    _keyCtl.text = '';
  }

  Future<void> _saveKey() async {
    final k = _keyCtl.text.trim();
    if (k.isEmpty) return;
    // The engine gets the key first; keychain storage is best effort.
    _engine.setCredential(provider: _provider, secret: k);
    try {
      await _secure.write(key: 'api_key:$_provider', value: k);
      _snack('Key saved to keychain');
    } catch (e) {
      _snack('Key set for this run only; keychain write failed: $e');
    }
    setState(() {
      _keyStored = true;
      _keyCtl.text = '';
    });
  }

  Future<void> _saveSettings() async {
    final json = jsonEncode(_settings);
    await settingsSave(path: _settingsPath, json: json);
    _engine.rememberSettings(settingsJson: json);
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
            if (_fallbackReason != null) _fallbackText = '${_fallbackText.trimRight()} ${t['Final']}'.trim();
          }
          if (t.containsKey('Error')) _error = 'Provider: ${t['Error']}';
        case 'ConsentRequired':
          _askConsent(v as Map<String, dynamic>);
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
        case 'Command':
          _snack('Command: $v');
        default:
          break;
      }
    });
  }

  Future<void> _askConsent(Map<String, dynamic> r) async {
    final uses = (r['uses'] as List).join(', ');
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('Send audio to ${r['name']}?'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Your recordings leave this device and are processed by ${r['name']} for: $uses.'),
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

  Future<void> _start() async {
    final denied = await ensureMicPermission();
    if (denied != null) {
      setState(() => _error = denied);
      return;
    }
    setState(() {
      _error = null;
      _finals.clear();
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
          _providerSection(),
          const SizedBox(height: 16),
          _soundSection(),
          const SizedBox(height: 16),
          _micSection(),
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
          const SizedBox(height: 8),
          Center(child: Text('$_state${_speech ? ' · speech' : ''}')),
          const SizedBox(height: 8),
          LinearProgressIndicator(value: ((_level + 60) / 60).clamp(0.0, 1.0)),
          const SizedBox(height: 12),
          if (_interim.isNotEmpty) Text(_interim, style: const TextStyle(fontStyle: FontStyle.italic)),
          if (_error != null) Text(_error!, style: TextStyle(color: Theme.of(context).colorScheme.error)),
          if (_fallbackReason != null) _fallbackCard(),
          const SizedBox(height: 12),
          ..._finals.reversed.take(20).map((t) => Padding(padding: const EdgeInsets.symmetric(vertical: 2), child: Text(t))),
        ],
      ),
    );
  }

  Widget _providerSection() {
    final opts = (_settings['providers']['options'] as Map<String, dynamic>)[_provider] as Map<String, dynamic>? ?? {};
    final batch = opts['transfer'] == 'Batch';
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
              title: const Text('Batch upload (higher accuracy, slower)'),
              value: batch,
              onChanged: (b) {
                final o = Map<String, dynamic>.from(opts);
                o['transfer'] = b ? 'Batch' : 'Streaming';
                (_settings['providers']['options'] as Map<String, dynamic>)[_provider] = o;
                _saveSettings();
              },
            ),
            Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _keyCtl,
                    obscureText: true,
                    decoration: InputDecoration(
                      labelText: _keyStored ? 'API key stored · enter to replace' : 'API key for $_provider',
                      helperText: _provider == 'voiceitt' ? 'Format: APP_ID:API_KEY' : null,
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
            Row(children: [
              const SizedBox(width: 120, child: Text('Preamp gain')),
              Expanded(
                child: Slider(
                  value: (sound['preamp_gain_db'] as num).toDouble(),
                  min: 0,
                  max: 24,
                  divisions: 24,
                  label: '${sound['preamp_gain_db']} dB',
                  onChanged: (v) => setState(() => sound['preamp_gain_db'] = v.roundToDouble()),
                  onChangeEnd: (_) => _saveSettings(),
                ),
              ),
            ]),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Noise reduction (RNNoise)'),
              value: sound['denoise'] != 'Off',
              onChanged: (b) {
                sound['denoise'] = b ? 'RnNoise' : 'Off';
                _saveSettings();
              },
            ),
            Row(children: [
              const SizedBox(width: 120, child: Text('Pause (ms)')),
              Expanded(
                child: Slider(
                  value: (vad['silence_duration_ms'] as num).toDouble(),
                  min: 300,
                  max: 3000,
                  divisions: 27,
                  label: '${vad['silence_duration_ms']} ms',
                  onChanged: (v) => setState(() => vad['silence_duration_ms'] = v.round()),
                  onChangeEnd: (_) => _saveSettings(),
                ),
              ),
            ]),
          ],
        ),
      ),
    );
  }

  Widget _micSection() {
    final mic = _settings['microphone'] as Map<String, dynamic>;
    final current = mic['device_id'] as String?;
    final ids = _mics.map((m) => m.id).toList();
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Microphone', style: Theme.of(context).textTheme.titleMedium),
            DropdownButton<String?>(
              value: ids.contains(current) ? current : null,
              isExpanded: true,
              hint: const Text('System default'),
              items: [
                const DropdownMenuItem<String?>(value: null, child: Text('System default')),
                ..._mics.map((m) => DropdownMenuItem<String?>(value: m.id, child: Text(m.isDefault ? '${m.name} (default)' : m.name))),
              ],
              onChanged: (id) {
                mic['device_id'] = id;
                mic['device_label'] = _mics.where((m) => m.id == id).map((m) => m.name).firstOrNull;
                _saveSettings();
              },
            ),
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
