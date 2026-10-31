# German word list

`de.txt.gz`: one lower-case word form per line, sorted, gzip. Used by
`ikk_text::dictionary` to tell a real German word (or a compound of real
words) from a recogniser's invented token. Nothing is downloaded at run
time; the list is embedded in the core.

Source: the German list of the FrequencyWords project by Hermit Dave
(https://github.com/hermitdave/FrequencyWords, OpenSubtitles 2018 corpus),
licensed CC BY-SA 4.0 — letter-only forms seen at least ten times, so
that subtitle typos stay out. Regenerate with `ops/make_wordlist.py`.
