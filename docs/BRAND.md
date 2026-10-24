# Brand colours

Primary colour is the Ideen Kaffee Korner blue **#0099CC**. Every shell
(reference app and any other shell, CLI where colour is used) derives from these
tokens. The shades are tuned for WCAG contrast.

| Token | Hex | Use |
|---|---|---|
| `brand` | `#0099CC` | Seed colour for the Material 3 palette; logo, splash |
| `primary` | `#0086B3` | Mic button, switches, accents: white-on-blue reaches ~4:1 |
| `ink` | `#006688` | Text on light button fills, ≥ 4.5:1 (AA normal text) |
| `primaryContainer` | `#D4EDFC` | Icon background, drawer header, soft surfaces |
| `background` | `#F6FAFE` | App background; native splash colour must match |

Status colours stay semantic and unbranded: success `#22C55E`, warning
`#F59E0B`, error `#EF4444`.

Flutter:

```dart
const brandBlue = Color(0xFF0099CC);
const primaryBlue = Color(0xFF0086B3);
const buttonInk = Color(0xFF006688);
const iconBackgroundBlue = Color(0xFFD4EDFC);
final scheme = ColorScheme.fromSeed(seedColor: brandBlue)
    .copyWith(primary: primaryBlue, primaryContainer: iconBackgroundBlue);
```
