// User profiles: named bundles of the voice-specific settings (provider,
// language, correction, impaired-voice means, audio filters, trained
// words). The settings file keeps them under `profiles`; the top-level
// `providers`, `sound` and `text` sections are always the active profile's
// live copy, so the engine, the CLI and the other shells read them as
// before. AGPL-3.0-or-later.

import 'dart:convert';

import 'package:flutter/material.dart';

import 'theme.dart';

const kDefaultProfileId = 'default';

/// The sections a profile owns; everything else is general.
const kProfileSections = ['providers', 'sound', 'text'];

/// Icons a profile can carry, by the name stored in the settings file.
const kProfileIcons = <String, IconData>{
  'person': Icons.person,
  'voice': Icons.record_voice_over,
  'mic': Icons.mic,
  'accessibility': Icons.accessibility_new,
  'work': Icons.work_outline,
  'home': Icons.home_outlined,
  'school': Icons.school_outlined,
  'medical': Icons.medical_services_outlined,
  'star': Icons.star_outline,
  'favorite': Icons.favorite_border,
};

/// Chip colours; the stored value is the index.
const kProfileColors = <Color>[
  brandBlue,
  Color(0xFF2E7D32),
  Color(0xFF6A1B9A),
  Color(0xFFEF6C00),
  Color(0xFFC62828),
  Color(0xFF00838F),
  Color(0xFF455A64),
];

IconData profileIcon(Map<String, dynamic> p) => kProfileIcons[p['icon']] ?? Icons.person;

Color profileColor(Map<String, dynamic> p) {
  final i = (p['color'] as num?)?.toInt() ?? 0;
  return kProfileColors[i.clamp(0, kProfileColors.length - 1)];
}

Map<String, dynamic> deepCopy(Map<String, dynamic> m) => jsonDecode(jsonEncode(m)) as Map<String, dynamic>;

/// The profile list inside a settings map.
class Profiles {
  Profiles(this.settings);

  final Map<String, dynamic> settings;

  Map<String, dynamic> get _section =>
      (settings['profiles'] ??= <String, dynamic>{'active': kDefaultProfileId, 'list': <dynamic>[]}) as Map<String, dynamic>;

  List<Map<String, dynamic>> get list {
    final l = (_section['list'] ??= <dynamic>[]) as List;
    return l.cast<Map<String, dynamic>>();
  }

  String get activeId => _section['active'] as String? ?? kDefaultProfileId;

  Map<String, dynamic>? byId(String id) => list.where((p) => p['id'] == id).firstOrNull;

  Map<String, dynamic> get active => byId(activeId) ?? list.first;

  bool get activeIsDefault => activeId == kDefaultProfileId;

  /// First run after the update: the built-in Default profile does not
  /// exist yet. Default is the clear-speech setup; when the file was in
  /// impaired mode, the current setup becomes a second profile and stays
  /// active, so nobody loses their tuning.
  void ensure({required String defaultTitle, required String defaultSubtitle, required String impairedTitle}) {
    if (byId(kDefaultProfileId) != null) return;
    final current = _sectionsOf(settings);
    final impaired = (current['sound'] as Map<String, dynamic>)['impaired_voice'] != false;
    final def = <String, dynamic>{
      'id': kDefaultProfileId,
      'title': defaultTitle,
      'subtitle': defaultSubtitle,
      'icon': 'person',
      'color': 0,
      ...deepCopy(current),
    };
    applyVoicePreset(def, false);
    list.insert(0, def);
    if (impaired) {
      final own = <String, dynamic>{
        'id': _newId(),
        'title': impairedTitle,
        'subtitle': '',
        'icon': 'voice',
        'color': 1,
        ...deepCopy(current),
      };
      list.add(own);
      _section['active'] = own['id'];
    } else {
      _section['active'] = kDefaultProfileId;
    }
  }

  /// The top-level sections (the live copy) into the active profile.
  void storeActive() {
    final p = byId(activeId);
    if (p == null) return;
    for (final s in kProfileSections) {
      if (settings[s] != null) p[s] = deepCopy(settings[s] as Map<String, dynamic>);
    }
  }

  /// Make [id] the active profile: the current one is stored first, then
  /// the chosen one's sections become the top-level ones.
  void activate(String id) {
    final p = byId(id);
    if (p == null) return;
    storeActive();
    for (final s in kProfileSections) {
      if (p[s] != null) settings[s] = deepCopy(p[s] as Map<String, dynamic>);
    }
    _section['active'] = id;
  }

  /// A new profile, started from the active one's sections with the
  /// voice preset applied; not activated.
  Map<String, dynamic> create({
    required String title,
    String subtitle = '',
    String icon = 'voice',
    int color = 1,
    required bool impaired,
  }) {
    storeActive();
    final p = <String, dynamic>{
      'id': _newId(),
      'title': title,
      'subtitle': subtitle,
      'icon': icon,
      'color': color,
      ...deepCopy(_sectionsOf(settings)),
    };
    applyVoicePreset(p, impaired);
    list.add(p);
    return p;
  }

  /// Remove [id]; Default stays. Removing the active one activates Default.
  void delete(String id) {
    if (id == kDefaultProfileId) return;
    if (activeId == id) activate(kDefaultProfileId);
    list.removeWhere((p) => p['id'] == id);
  }

  static String _newId() => 'p${DateTime.now().millisecondsSinceEpoch.toRadixString(36)}';

  static Map<String, dynamic> _sectionsOf(Map<String, dynamic> m) => {
        for (final s in kProfileSections)
          if (m[s] != null) s: m[s],
      };

  /// The two presets. Each side writes its whole setup, so later
  /// experiments change only the impaired side. [m] holds `providers`,
  /// `sound` and `text` (a profile or the top-level settings).
  static void applyVoicePreset(Map<String, dynamic> m, bool impaired) {
    final sound = m['sound'] as Map<String, dynamic>;
    final vad = sound['vad'] as Map<String, dynamic>;
    final providers = m['providers'] as Map<String, dynamic>;
    final options = (providers['options'] ??= <String, dynamic>{}) as Map<String, dynamic>;
    final provider = providers['active'] as String? ?? 'elevenlabs';
    final o = Map<String, dynamic>.from(options[provider] as Map<String, dynamic>? ?? {});
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
      vad['agreement_span_ms'] = 8000;
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
    options[provider] = o;
  }
}

/// A profile as a rounded chip: icon in its colour, title, subtitle.
class ProfileChip extends StatelessWidget {
  const ProfileChip({
    super.key,
    required this.profile,
    required this.selected,
    required this.onTap,
    this.compact = false,
  });

  final Map<String, dynamic> profile;
  final bool selected;
  final VoidCallback onTap;
  /// Phone row: narrower, subtitle under the title in one line.
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = profileColor(profile);
    final title = profile['title'] as String? ?? '';
    final subtitle = profile['subtitle'] as String? ?? '';
    return Semantics(
      button: true,
      selected: selected,
      label: subtitle.isEmpty ? title : '$title, $subtitle',
      child: Material(
        color: selected ? color.withValues(alpha: 0.14) : scheme.surface,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(16),
          side: BorderSide(color: selected ? color : scheme.outlineVariant, width: selected ? 2 : 1),
        ),
        clipBehavior: Clip.antiAlias,
        child: InkWell(
          onTap: onTap,
          child: Padding(
            padding: EdgeInsets.symmetric(horizontal: compact ? 12 : 14, vertical: compact ? 8 : 10),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                Container(
                  width: compact ? 36 : 40,
                  height: compact ? 36 : 40,
                  decoration: BoxDecoration(color: color, borderRadius: BorderRadius.circular(12)),
                  child: Icon(profileIcon(profile), color: Colors.white, size: compact ? 20 : 22),
                ),
                const SizedBox(width: 10),
                ConstrainedBox(
                  constraints: BoxConstraints(maxWidth: compact ? 140 : 170),
                  child: Column(
                    mainAxisSize: MainAxisSize.min,
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(title,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: Theme.of(context).textTheme.titleSmall?.copyWith(fontWeight: FontWeight.w600)),
                      if (subtitle.isNotEmpty)
                        Text(subtitle,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: Theme.of(context).textTheme.bodySmall?.copyWith(color: scheme.onSurfaceVariant)),
                    ],
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

/// The "+" chip that starts the profile wizard.
class AddProfileChip extends StatelessWidget {
  const AddProfileChip({super.key, required this.label, required this.onTap, this.compact = false});

  final String label;
  final VoidCallback onTap;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Semantics(
      button: true,
      label: label,
      child: Material(
        color: scheme.surface,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(16),
          side: BorderSide(color: scheme.outlineVariant),
        ),
        clipBehavior: Clip.antiAlias,
        child: InkWell(
          onTap: onTap,
          child: Padding(
            padding: EdgeInsets.symmetric(horizontal: compact ? 12 : 14, vertical: compact ? 8 : 10),
            child: Row(mainAxisSize: MainAxisSize.min, children: [
              Container(
                width: compact ? 36 : 40,
                height: compact ? 36 : 40,
                decoration: BoxDecoration(
                  borderRadius: BorderRadius.circular(12),
                  border: Border.all(color: scheme.primary, width: 2),
                ),
                child: Icon(Icons.add, color: scheme.primary, size: compact ? 20 : 22),
              ),
              if (!compact) ...[
                const SizedBox(width: 10),
                Text(label, style: Theme.of(context).textTheme.titleSmall?.copyWith(color: scheme.primary)),
              ],
            ]),
          ),
        ),
      ),
    );
  }
}
