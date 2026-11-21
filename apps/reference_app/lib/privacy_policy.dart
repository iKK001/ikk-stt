import 'package:flutter/material.dart';

/// The privacy policy and terms of use behind the Impressum: what stays
/// on the device, what goes to which service and only with consent, and
/// where the developer's responsibility ends. The developer's name stands
/// in the Impressum only; this page refers to it. German for the German app
/// languages, English for every other.
class PrivacyPolicyScreen extends StatelessWidget {
  const PrivacyPolicyScreen({
    super.key,
    required this.german,
    required this.appName,
    required this.contact,
  });

  final bool german;
  final String appName, contact;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final sections = german ? _german(appName, contact) : _english(appName, contact);
    return Scaffold(
      appBar: AppBar(title: Text(german ? 'Datenschutz und Nutzungsbedingungen' : 'Privacy policy and terms of use')),
      body: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 860),
          child: ListView(
            padding: const EdgeInsets.all(24),
            children: [
              for (final (title, paragraphs) in sections) ...[
                Text(title, style: theme.textTheme.titleMedium),
                const SizedBox(height: 6),
                for (final p in paragraphs)
                  Padding(
                    padding: const EdgeInsets.only(bottom: 8),
                    child: Text(p, style: theme.textTheme.bodyLarge?.copyWith(height: 1.4), textAlign: TextAlign.justify),
                  ),
                const SizedBox(height: 14),
              ],
            ],
          ),
        ),
      ),
    );
  }
}

typedef _Sections = List<(String, List<String>)>;

_Sections _english(String app, String contact) => [
      (
        'In short',
        [
          '$app has no user accounts, no advertising, no tracking and no analytics. The developer runs no servers for it and receives none of your recordings, texts, settings or keys.',
          'Everything the app remembers stays on your device. Speech or text leaves the device only when you choose a cloud service yourself, enter your own key for it and agree to the transfer.',
        ]
      ),
      (
        'Who is responsible',
        [
          'Ideen Kaffee, Cham, Switzerland (see the Impressum). Contact: $contact.',
          'The Swiss Federal Act on Data Protection (FADP) applies and, where it is applicable, the EU General Data Protection Regulation (GDPR).',
        ]
      ),
      (
        'What stays on your device',
        [
          'Your settings and profiles, the words and voice commands you trained, your consent decisions and the position of the buttons are stored in the app\'s data folder. API keys are kept in the secure key store of the operating system.',
          'The app keeps no record of what you dictate. Removing the app and its data folder removes everything. Backups of your device made by you or by your operating system may contain the settings.',
        ]
      ),
      (
        'Microphone and text of other programs',
        [
          'The microphone is used only while you dictate, while a voice command is being listened for or while you record training samples. Audio is held in memory for the recognition and is not kept afterwards.',
          'To type for you and to correct text, the app reads and changes the text field you are working in, and only when you start a dictation or a correction. It uses the accessibility and input functions of the operating system for this and nothing else.',
        ]
      ),
      (
        'Cloud services, only with your consent',
        [
          'Speech recognition can be done by Deepgram, Voiceitt, OpenAI, ElevenLabs or Microsoft Azure. Live correction can be done on the device, or by LanguageTool, OpenAI or Anthropic. Before anything is sent to one of these services for the first time, the app asks for your consent for that service.',
          'The data then goes directly from your device to the service, under your own account and key with that service, and not through the developer. The service processes it under its own terms and privacy policy, possibly outside Switzerland and the EU, for example in the USA. The developer has no influence on this processing and cannot access, correct or delete data held by a service; turn to the service for that.',
          'You can withdraw a consent in the app at any time; from then on nothing more is sent to that service. Do not send particularly sensitive data, such as health data or data about other people, unless you are entitled to and accept the service\'s terms for it.',
        ]
      ),
      (
        'No sharing, no selling',
        ['The developer does not receive your data and therefore neither passes it on nor sells it.']
      ),
      (
        'Your rights',
        [
          'You have the right to information, correction, deletion, restriction, data portability and to withdraw consent. As the data is on your device, you exercise most of these rights in the app itself, and for data at a cloud service with that service. You may lodge a complaint with the Swiss Federal Data Protection and Information Commissioner (FDPIC) or your local authority.',
        ]
      ),
      (
        'Terms of use and disclaimer',
        [
          'You use the app and the services reached through it entirely at your own risk. You alone are responsible for what you dictate, enter or have generated, for checking the result before you use it, and for any further use of it. Recognition and correction can be wrong; text is typed into the program you chose and may change or replace text there.',
          'The app is a technical interface to external speech and text services. The developer has no influence on how these services process data, on their availability, or on the quality, correctness, completeness or lawfulness of what they return, and is not their agent or partner.',
          'You need your own account and key for each cloud service. You are responsible for keeping your keys secret, for complying with the terms and privacy rules of the service you choose, and for all costs that arise there, including costs caused by a lost, shared or misused key.',
          'The app is not a medical device and is not meant for emergencies or for situations in which a recognition error can cause harm or damage.',
          'You may use the app only lawfully. In particular you must not record or transcribe other people without the consent the law requires, must not use the app to deceive, harass or harm others, and must not try to gain access to services or data you are not entitled to. Anyone who misuses the app is solely responsible for the consequences and indemnifies the developer against claims by third parties arising from that misuse.',
          'The app is provided as it is, without any warranty of any kind, express or implied, including fitness for a particular purpose and uninterrupted or error-free operation. To the extent permitted by law, any liability of the developer for direct or indirect damage, loss of profit, loss of data or other disadvantages arising from the use of the app or of the connected services is excluded. Liability that cannot be excluded by law remains unaffected.',
          'Swiss law applies.',
        ]
      ),
      (
        'Changes',
        ['This text is updated when the app or the legal requirements change. The version shown in the app you have installed applies.']
      ),
    ];

_Sections _german(String app, String contact) => [
      (
        'Kurz gesagt',
        [
          '$app hat keine Benutzerkonten, keine Werbung, kein Tracking und keine Analyse. Der Entwickler betreibt dafür keine Server und erhält keine Ihrer Aufnahmen, Texte, Einstellungen oder Schlüssel.',
          'Alles, was sich die App merkt, bleibt auf Ihrem Gerät. Sprache oder Text verlässt das Gerät nur, wenn Sie selbst einen Cloud-Dienst wählen, dafür Ihren eigenen Schlüssel eingeben und der Übertragung zustimmen.',
        ]
      ),
      (
        'Verantwortlich',
        [
          'Ideen Kaffee, Cham, Schweiz (siehe Impressum). Kontakt: $contact.',
          'Es gilt das Schweizer Datenschutzgesetz (DSG) und, soweit anwendbar, die Datenschutz-Grundverordnung der EU (DSGVO).',
        ]
      ),
      (
        'Was auf Ihrem Gerät bleibt',
        [
          'Ihre Einstellungen und Profile, die trainierten Wörter und Sprachbefehle, Ihre Einwilligungen und die Position der Buttons liegen im Datenordner der App. API-Schlüssel liegen im sicheren Schlüsselspeicher des Betriebssystems.',
          'Die App führt kein Protokoll darüber, was Sie diktieren. Wer die App und ihren Datenordner entfernt, entfernt alles. Sicherungen Ihres Geräts, die Sie oder Ihr Betriebssystem anlegen, können die Einstellungen enthalten.',
        ]
      ),
      (
        'Mikrofon und Text anderer Programme',
        [
          'Das Mikrofon wird nur verwendet, solange Sie diktieren, solange auf einen Sprachbefehl gehört wird oder solange Sie Trainingsbeispiele aufnehmen. Audio wird für die Erkennung im Arbeitsspeicher gehalten und danach nicht aufbewahrt.',
          'Um für Sie zu schreiben und Text zu korrigieren, liest und ändert die App das Textfeld, in dem Sie arbeiten, und nur dann, wenn Sie ein Diktat oder eine Korrektur starten. Sie nutzt dafür die Bedienungshilfen und Eingabefunktionen des Betriebssystems und nichts darüber hinaus.',
        ]
      ),
      (
        'Cloud-Dienste, nur mit Ihrer Einwilligung',
        [
          'Die Spracherkennung kann durch Deepgram, Voiceitt, OpenAI, ElevenLabs oder Microsoft Azure erfolgen. Die Live-Korrektur kann auf dem Gerät erfolgen oder durch LanguageTool, OpenAI oder Anthropic. Bevor zum ersten Mal etwas an einen dieser Dienste gesendet wird, fragt die App nach Ihrer Einwilligung für diesen Dienst.',
          'Die Daten gehen dann direkt von Ihrem Gerät an den Dienst, unter Ihrem eigenen Konto und Schlüssel bei diesem Dienst, und nicht über den Entwickler. Der Dienst verarbeitet sie nach seinen eigenen Bedingungen und Datenschutzregeln, möglicherweise ausserhalb der Schweiz und der EU, zum Beispiel in den USA. Der Entwickler hat auf diese Verarbeitung keinen Einfluss und kann Daten bei einem Dienst weder einsehen noch berichtigen oder löschen; wenden Sie sich dafür an den Dienst.',
          'Sie können eine Einwilligung jederzeit in der App widerrufen; von da an wird nichts mehr an diesen Dienst gesendet. Senden Sie keine besonders schützenswerten Daten, etwa Gesundheitsdaten oder Daten über andere Personen, sofern Sie dazu nicht berechtigt sind und die Bedingungen des Dienstes dafür akzeptieren.',
        ]
      ),
      (
        'Keine Weitergabe, kein Verkauf',
        ['Der Entwickler erhält Ihre Daten nicht und gibt sie daher weder weiter noch verkauft er sie.']
      ),
      (
        'Ihre Rechte',
        [
          'Sie haben das Recht auf Auskunft, Berichtigung, Löschung, Einschränkung, Datenübertragbarkeit und auf Widerruf einer Einwilligung. Da die Daten auf Ihrem Gerät liegen, üben Sie die meisten dieser Rechte in der App selbst aus, und für Daten bei einem Cloud-Dienst bei diesem Dienst. Sie können sich beim Eidgenössischen Datenschutz- und Öffentlichkeitsbeauftragten (EDÖB) oder bei Ihrer örtlichen Behörde beschweren.',
        ]
      ),
      (
        'Nutzungsbedingungen und Haftungsausschluss',
        [
          'Die Nutzung der App und der über sie angesprochenen Dienste erfolgt ausschliesslich auf eigenes Risiko. Sie allein sind verantwortlich für das, was Sie diktieren, eingeben oder erzeugen lassen, für die Prüfung des Ergebnisses vor dessen Verwendung und für jede weitere Verwendung. Erkennung und Korrektur können falsch sein; der Text wird in das von Ihnen gewählte Programm geschrieben und kann dort Text ändern oder ersetzen.',
          'Die App ist eine technische Schnittstelle zu externen Sprach- und Textdiensten. Der Entwickler hat keinerlei Einfluss darauf, wie diese Dienste Daten verarbeiten, auf ihre Verfügbarkeit sowie auf die Qualität, Richtigkeit, Vollständigkeit oder Rechtmässigkeit der von ihnen gelieferten Ergebnisse und ist weder ihr Vertreter noch ihr Partner.',
          'Für jeden Cloud-Dienst brauchen Sie ein eigenes Konto und einen eigenen Schlüssel. Sie sind dafür verantwortlich, Ihre Schlüssel geheim zu halten, die Nutzungs- und Datenschutzbedingungen des gewählten Dienstes einzuhalten und sämtliche dort anfallenden Kosten selbst zu tragen, auch Kosten durch einen verlorenen, weitergegebenen oder missbrauchten Schlüssel.',
          'Die App ist kein Medizinprodukt und nicht für Notfälle oder für Situationen gedacht, in denen ein Erkennungsfehler Schaden verursachen kann.',
          'Die App darf nur rechtmässig verwendet werden. Insbesondere dürfen Sie andere Personen nicht ohne die gesetzlich nötige Einwilligung aufnehmen oder transkribieren, die App nicht einsetzen, um andere zu täuschen, zu belästigen oder zu schädigen, und nicht versuchen, sich Zugang zu Diensten oder Daten zu verschaffen, zu denen Sie nicht berechtigt sind. Wer die App missbraucht, trägt die Folgen allein und stellt den Entwickler von Ansprüchen Dritter aus diesem Missbrauch frei.',
          'Die App wird so bereitgestellt, wie sie ist, ohne jede ausdrückliche oder stillschweigende Gewährleistung, auch nicht für die Eignung zu einem bestimmten Zweck oder für einen unterbrechungs- oder fehlerfreien Betrieb. Jegliche Haftung des Entwicklers für direkte oder indirekte Schäden, entgangenen Gewinn, Datenverlust oder sonstige Nachteile, die aus der Nutzung der App oder der angebundenen Dienste entstehen, ist – soweit gesetzlich zulässig – ausgeschlossen. Eine Haftung, die gesetzlich nicht ausgeschlossen werden kann, bleibt unberührt.',
          'Es gilt Schweizer Recht.',
        ]
      ),
      (
        'Änderungen',
        ['Dieser Text wird angepasst, wenn sich die App oder die rechtlichen Anforderungen ändern. Es gilt die Fassung, die in der installierten App angezeigt wird.']
      ),
    ];
