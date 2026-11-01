# Voice commands

Spoken inside dictation, in German, as a phrase on its own. A final that
is exactly one of these is carried out instead of typed. The recogniser's
punctuation does not matter ("Neue Zeile." works), nor does case; numbers
can be said as words or digits. When the agreed-words mode types a command
in two pieces ("Neue" … "Zeile"), the first piece is taken back and the
command carried out.

The phrases follow the Dragon NaturallySpeaking German quick-start guide
(`documentation/001_Dragon_Kurzanleitung`). This list is the one that is
implemented; it will change. Source: `crates/ikk-commands` (phrases),
`crates/ikk-engine/src/commands.rs` (what they do).

**Platforms.** Keys and shortcuts go to the focused application through the
desktop sink: macOS (tested), Windows and Linux (same code, SendInput and
wtype/enigo). On the phones the app inserts text through the host (keyboard
extension, accessibility service), which takes text but no key presses yet:
there, only the commands marked *text* work; the others are reported as
"not carried out". The microphone words are the trained start and stop
words of the offline voice commands; "Geh schlafen" and "Wach auf" are
whatever you trained them as.

Where a command needs the dictated text ("Streich das", "Markier Lösung"),
the engine uses what it typed itself in this session. After a move it
cannot follow (a line up, a page down, undo, paste), it no longer knows
where the caret is and refuses those commands until the caret is back at
the end ("Gehe zum Dokumentende") or a new session starts.

| Say | Does | Where |
|---|---|---|
| **Mikrofon** | | |
| Geh schlafen · Mikrofon ausschalten · Diktat stopp | ends the dictation (the trained stop word does the same) | all |
| Wach auf | the trained start word, recognised offline while waiting | all |
| **Zeilen** | | |
| Neue Zeile | a line break | all, *text* |
| Neuer Absatz | an empty line | all, *text* |
| Drücke Eingabetaste · Eingabetaste | Enter key | desktop |
| Drücke Tab · Tabulator | Tab key | desktop |
| **Löschen** | | |
| Streich das · Lösch das | deletes the last dictated piece | all, *text* |
| Streich das ‹n› Mal | deletes the last n pieces | all, *text* |
| Lösche die letzten ‹n› Wörter · Lösche das letzte Wort | deletes the last n words | all, *text* |
| Zeile löschen | Home, select to the end of the line, delete | desktop |
| Rückschritt ‹n› · Rückschritt | n backspaces | all, *text* at the end |
| Rückgängig machen · Rückgängig | the application's undo (Cmd/Ctrl+Z) | desktop |
| Schneide das aus · Ausschneiden | cut (Cmd/Ctrl+X) | desktop |
| Kopiere das · Kopieren | copy (Cmd/Ctrl+C) | desktop |
| Füge das ein · Einfügen | paste (Cmd/Ctrl+V) | desktop |
| **Markieren** | | |
| Alles markieren · Markier Dokument | select all (Cmd/Ctrl+A) | desktop |
| Markier das | selects the last dictated piece | desktop |
| Markier ‹Wort oder Wörter› | selects those words in the dictated text (last occurrence) | desktop |
| Markier ‹Anfang› bis ‹Ende› | selects from the first words to the last | desktop |
| Markier die nächsten ‹n› Wörter | selects n words to the right of the caret | desktop |
| Markier vorherigen Absatz | selects to the start of the paragraph | desktop |
| Auswahl aufheben | drops the selection (caret at its end) | desktop |
| **Cursor** | | |
| Cursor vor ‹Text› | caret before those words | desktop |
| Cursor nach ‹Text› · Weiter bei ‹Wort› | caret after those words | desktop |
| Gehe zum Anfang · Zeilenanfang | start of the line | desktop |
| Gehe zum Ende · Zeilenende | end of the line | desktop |
| Gehe zum Dokumentanfang · Dokumentanfang | start of the document | desktop |
| Gehe zum Dokumentende · Dokumentende | end of the document | desktop |
| Gehe ‹n› nach unten / oben · Gehe ‹n› Zeilen nach unten / oben | n lines down or up | desktop |
| Gehe ‹n› Zeichen nach links / rechts | n characters | desktop |
| Gehe ‹n› Wörter nach links / rechts | n words | desktop |
| Drücke Pfeil nach rechts / links / oben / unten | one arrow key | desktop |
| Seite nach oben / unten | Page Up / Page Down | desktop |
| **Formatieren** | | |
| Fett · Mach das fett | bold shortcut (Cmd/Ctrl+B); select first | desktop |
| Kursiv · Mach das kursiv | italic shortcut (Cmd/Ctrl+I) | desktop |
| Unterstreich das · Unterstreichen | underline shortcut (Cmd/Ctrl+U) | desktop |
| Mach das gross | the last dictated piece in capitals | all, *text* |
| Mach das klein | the last dictated piece in lower case | all, *text* |
| Grossschaltung anfangen / beenden | everything dictated from now on in capitals | all, *text* |
| **Korrigieren und Buchstabieren** | | |
| Korrigier das | selects the last dictated piece; the next dictation replaces it | desktop |
| Korrigier ‹Wort oder Wörter› | selects those words; the next dictation replaces them | desktop |
| Schreib das · Buchstabier das | the next thing you say is taken letter by letter | all, *text* |
| Schreib ‹gross b a Bindestrich 5› | types the spelled letters: single letters, the spelling alphabet (Anton, Berta, Cäsar …), digits, "gross" before a capital, Bindestrich, Leerzeichen, Punkt, Komma, Unterstrich, At | all, *text* |
| Starte Buchstabiermodus / Buchstabiermodus beenden | every thought spelled until ended | all, *text* |
| **Satzzeichen** (said on their own) | | |
| Punkt · Komma · Ausrufezeichen · Fragezeichen · Doppelpunkt · Semikolon / Strichpunkt | . , ! ? : ; | all, *text* |
| Apostroph · Klammer auf / zu · Anführungszeichen (unten / oben) · Gedankenstrich · Bindestrich · Unterstrich | ' ( ) „ “ – - _ | all, *text* |
| At / Klammeraffe · Pluszeichen · Und-Zeichen · Paragraphenzeichen · Nummernzeichen · Prozentzeichen · Schrägstrich · Zwinkerndes Gesicht | @ + & § # % / ;-) | all, *text* |

## From the guide, not implemented

Windows, browser, e-mail, mouse and application commands ("Starte Microsoft
Word", "Maus nach oben", "Öffne die Top-Webseite für …"), "Zeige
Diktierfenster an", "Mache die vorherige Zeile fett", bullet lists
("mit/ohne Aufzählungszeichen"), "Zurückgehen", "Zeige mir Hilfe", and the
numbered correction menu ("Nimm 1"): the app has no such menu — mistakes are
taught in the Training card instead.
