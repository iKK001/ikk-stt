/// Strings the app says to the user in its own language (the toasts and
/// the few labels around them). The ten languages and their codes are the
/// ones of the iKK Voice app: ISO 639-3, English as the fallback.
/// Templates: `{c}` the command phrase, `{w}` a word or reason, `{s}` a
/// stretch of text, `{p}` a provider, `{e}` an error.
class UiStrings {
  const UiStrings({
    required this.name,
    required this.appLanguage,
    required this.commandDoneTpl,
    required this.commandRefusedTpl,
    required this.voiceStart,
    required this.voiceStop,
    required this.vocabularySaved,
    required this.settingsNotSavedTpl,
    required this.savingFailedTpl,
    required this.keySavedTpl,
    required this.keyRemovedTpl,
    required this.noKeyTpl,
    required this.learnedTpl,
    required this.insertedIntoTarget,
    required this.permissionNeeded,
  });

  /// The language's own name, for the picker.
  final String name;
  final String appLanguage;
  final String commandDoneTpl;
  final String commandRefusedTpl;
  final String voiceStart;
  final String voiceStop;
  final String vocabularySaved;
  final String settingsNotSavedTpl;
  final String savingFailedTpl;
  final String keySavedTpl;
  final String keyRemovedTpl;
  final String noKeyTpl;
  final String learnedTpl;
  final String insertedIntoTarget;
  final String permissionNeeded;

  String commandDone(String phrase) => commandDoneTpl.replaceAll('{c}', phrase);
  String commandRefused(String why) => commandRefusedTpl.replaceAll('{w}', why);
  String settingsNotSaved(String e) => settingsNotSavedTpl.replaceAll('{e}', e);
  String savingFailed(String e) => savingFailedTpl.replaceAll('{e}', e);
  String keySaved(String store) => keySavedTpl.replaceAll('{s}', store);
  String keyRemoved(String provider) => keyRemovedTpl.replaceAll('{p}', provider);
  String noKey(String provider) => noKeyTpl.replaceAll('{p}', provider);
  String learned(String word, String stretch) => learnedTpl.replaceAll('{w}', word).replaceAll('{s}', stretch);

  /// Codes in the order of the picker.
  static const codes = ['eng', 'deu', 'fra', 'ita', 'spa', 'por', 'nld', 'hrv', 'srp', 'tur'];

  static UiStrings of(String code) {
    switch (code) {
      case 'deu':
        return _german;
      case 'fra':
        return _french;
      case 'ita':
        return _italian;
      case 'spa':
        return _spanish;
      case 'por':
        return _portuguese;
      case 'nld':
        return _dutch;
      case 'hrv':
        return _croatian;
      case 'srp':
        return _serbian;
      case 'tur':
        return _turkish;
      default:
        return _english;
    }
  }
}

const _english = UiStrings(
  name: 'English',
  appLanguage: 'App language',
  commandDoneTpl: 'Command: {c}',
  commandRefusedTpl: 'Command not carried out: {w}',
  voiceStart: 'Voice command: start',
  voiceStop: 'Voice command: stop',
  vocabularySaved: 'Vocabulary saved',
  settingsNotSavedTpl: 'Settings not saved: {e}',
  savingFailedTpl: 'Saving failed: {e}',
  keySavedTpl: 'Key saved in {s}',
  keyRemovedTpl: 'Key for {p} removed',
  noKeyTpl: 'No API key for {p} – enter it first.',
  learnedTpl: '«{w}» is now written for „{s}“',
  insertedIntoTarget: 'Inserted into target',
  permissionNeeded: 'Permission needed: allow keyboard access for this app (Accessibility).',
);

const _german = UiStrings(
  name: 'Deutsch',
  appLanguage: 'App-Sprache',
  commandDoneTpl: 'Befehl: {c}',
  commandRefusedTpl: 'Befehl nicht ausgeführt: {w}',
  voiceStart: 'Sprachbefehl: Start',
  voiceStop: 'Sprachbefehl: Stopp',
  vocabularySaved: 'Vokabular gespeichert',
  settingsNotSavedTpl: 'Einstellungen nicht gespeichert: {e}',
  savingFailedTpl: 'Speichern fehlgeschlagen: {e}',
  keySavedTpl: 'Schlüssel gespeichert in {s}',
  keyRemovedTpl: 'Schlüssel für {p} entfernt',
  noKeyTpl: 'Kein API-Schlüssel für {p} – zuerst eingeben.',
  learnedTpl: '«{w}» wird jetzt für „{s}“ geschrieben',
  insertedIntoTarget: 'In das Ziel eingefügt',
  permissionNeeded: 'Berechtigung nötig: Tastaturzugriff für diese App erlauben (Bedienungshilfen).',
);

const _french = UiStrings(
  name: 'Français',
  appLanguage: 'Langue de l’application',
  commandDoneTpl: 'Commande : {c}',
  commandRefusedTpl: 'Commande non exécutée : {w}',
  voiceStart: 'Commande vocale : démarrer',
  voiceStop: 'Commande vocale : arrêter',
  vocabularySaved: 'Vocabulaire enregistré',
  settingsNotSavedTpl: 'Réglages non enregistrés : {e}',
  savingFailedTpl: 'Échec de l’enregistrement : {e}',
  keySavedTpl: 'Clé enregistrée dans {s}',
  keyRemovedTpl: 'Clé pour {p} supprimée',
  noKeyTpl: 'Pas de clé API pour {p} – saisissez-la d’abord.',
  learnedTpl: '« {w} » s’écrit désormais pour « {s} »',
  insertedIntoTarget: 'Inséré dans la cible',
  permissionNeeded: 'Autorisation requise : permettre l’accès au clavier pour cette app (Accessibilité).',
);

const _italian = UiStrings(
  name: 'Italiano',
  appLanguage: 'Lingua dell’app',
  commandDoneTpl: 'Comando: {c}',
  commandRefusedTpl: 'Comando non eseguito: {w}',
  voiceStart: 'Comando vocale: avvia',
  voiceStop: 'Comando vocale: ferma',
  vocabularySaved: 'Vocabolario salvato',
  settingsNotSavedTpl: 'Impostazioni non salvate: {e}',
  savingFailedTpl: 'Salvataggio non riuscito: {e}',
  keySavedTpl: 'Chiave salvata in {s}',
  keyRemovedTpl: 'Chiave per {p} rimossa',
  noKeyTpl: 'Nessuna chiave API per {p} – inseriscila prima.',
  learnedTpl: '«{w}» viene ora scritto per „{s}“',
  insertedIntoTarget: 'Inserito nella destinazione',
  permissionNeeded: 'Autorizzazione necessaria: consenti l’accesso alla tastiera per questa app (Accessibilità).',
);

const _spanish = UiStrings(
  name: 'Español',
  appLanguage: 'Idioma de la app',
  commandDoneTpl: 'Comando: {c}',
  commandRefusedTpl: 'Comando no ejecutado: {w}',
  voiceStart: 'Comando de voz: iniciar',
  voiceStop: 'Comando de voz: detener',
  vocabularySaved: 'Vocabulario guardado',
  settingsNotSavedTpl: 'Ajustes no guardados: {e}',
  savingFailedTpl: 'Error al guardar: {e}',
  keySavedTpl: 'Clave guardada en {s}',
  keyRemovedTpl: 'Clave de {p} eliminada',
  noKeyTpl: 'No hay clave API para {p} – introdúcela primero.',
  learnedTpl: '«{w}» se escribe ahora para „{s}“',
  insertedIntoTarget: 'Insertado en el destino',
  permissionNeeded: 'Se necesita permiso: permite el acceso al teclado para esta app (Accesibilidad).',
);

const _portuguese = UiStrings(
  name: 'Português',
  appLanguage: 'Idioma da app',
  commandDoneTpl: 'Comando: {c}',
  commandRefusedTpl: 'Comando não executado: {w}',
  voiceStart: 'Comando de voz: iniciar',
  voiceStop: 'Comando de voz: parar',
  vocabularySaved: 'Vocabulário guardado',
  settingsNotSavedTpl: 'Definições não guardadas: {e}',
  savingFailedTpl: 'Falha ao guardar: {e}',
  keySavedTpl: 'Chave guardada em {s}',
  keyRemovedTpl: 'Chave de {p} removida',
  noKeyTpl: 'Sem chave API para {p} – introduza-a primeiro.',
  learnedTpl: '«{w}» é agora escrito para „{s}“',
  insertedIntoTarget: 'Inserido no destino',
  permissionNeeded: 'Permissão necessária: permita o acesso ao teclado para esta app (Acessibilidade).',
);

const _dutch = UiStrings(
  name: 'Nederlands',
  appLanguage: 'Taal van de app',
  commandDoneTpl: 'Opdracht: {c}',
  commandRefusedTpl: 'Opdracht niet uitgevoerd: {w}',
  voiceStart: 'Spraakopdracht: start',
  voiceStop: 'Spraakopdracht: stop',
  vocabularySaved: 'Woordenlijst opgeslagen',
  settingsNotSavedTpl: 'Instellingen niet opgeslagen: {e}',
  savingFailedTpl: 'Opslaan mislukt: {e}',
  keySavedTpl: 'Sleutel opgeslagen in {s}',
  keyRemovedTpl: 'Sleutel voor {p} verwijderd',
  noKeyTpl: 'Geen API-sleutel voor {p} – voer die eerst in.',
  learnedTpl: '«{w}» wordt nu geschreven voor „{s}“',
  insertedIntoTarget: 'Ingevoegd in het doel',
  permissionNeeded: 'Toestemming nodig: sta toetsenbordtoegang toe voor deze app (Toegankelijkheid).',
);

const _croatian = UiStrings(
  name: 'Hrvatski',
  appLanguage: 'Jezik aplikacije',
  commandDoneTpl: 'Naredba: {c}',
  commandRefusedTpl: 'Naredba nije izvršena: {w}',
  voiceStart: 'Glasovna naredba: start',
  voiceStop: 'Glasovna naredba: stop',
  vocabularySaved: 'Rječnik spremljen',
  settingsNotSavedTpl: 'Postavke nisu spremljene: {e}',
  savingFailedTpl: 'Spremanje nije uspjelo: {e}',
  keySavedTpl: 'Ključ spremljen u {s}',
  keyRemovedTpl: 'Ključ za {p} uklonjen',
  noKeyTpl: 'Nema API ključa za {p} – prvo ga unesite.',
  learnedTpl: '«{w}» se sada piše za „{s}“',
  insertedIntoTarget: 'Umetnuto u odredište',
  permissionNeeded: 'Potrebna dozvola: dopustite pristup tipkovnici za ovu aplikaciju (Pristupačnost).',
);

const _serbian = UiStrings(
  name: 'Srpski',
  appLanguage: 'Jezik aplikacije',
  commandDoneTpl: 'Komanda: {c}',
  commandRefusedTpl: 'Komanda nije izvršena: {w}',
  voiceStart: 'Glasovna komanda: start',
  voiceStop: 'Glasovna komanda: stop',
  vocabularySaved: 'Rečnik sačuvan',
  settingsNotSavedTpl: 'Podešavanja nisu sačuvana: {e}',
  savingFailedTpl: 'Čuvanje nije uspelo: {e}',
  keySavedTpl: 'Ključ sačuvan u {s}',
  keyRemovedTpl: 'Ključ za {p} uklonjen',
  noKeyTpl: 'Nema API ključa za {p} – prvo ga unesite.',
  learnedTpl: '«{w}» se sada piše za „{s}“',
  insertedIntoTarget: 'Umetnuto u odredište',
  permissionNeeded: 'Potrebna dozvola: dozvolite pristup tastaturi za ovu aplikaciju (Pristupačnost).',
);

const _turkish = UiStrings(
  name: 'Türkçe',
  appLanguage: 'Uygulama dili',
  commandDoneTpl: 'Komut: {c}',
  commandRefusedTpl: 'Komut uygulanmadı: {w}',
  voiceStart: 'Sesli komut: başlat',
  voiceStop: 'Sesli komut: durdur',
  vocabularySaved: 'Sözlük kaydedildi',
  settingsNotSavedTpl: 'Ayarlar kaydedilmedi: {e}',
  savingFailedTpl: 'Kaydetme başarısız: {e}',
  keySavedTpl: 'Anahtar {s} içine kaydedildi',
  keyRemovedTpl: '{p} anahtarı kaldırıldı',
  noKeyTpl: '{p} için API anahtarı yok – önce girin.',
  learnedTpl: '«{w}» artık „{s}“ yerine yazılıyor',
  insertedIntoTarget: 'Hedefe eklendi',
  permissionNeeded: 'İzin gerekli: bu uygulama için klavye erişimine izin verin (Erişilebilirlik).',
);
