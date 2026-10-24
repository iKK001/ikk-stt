// Brand tokens from docs/public/BRAND.md (Ideen Kaffee Korner blue).
import 'package:flutter/material.dart';

const brandBlue = Color(0xFF0099CC);
const primaryBlue = Color(0xFF0086B3);
const buttonInk = Color(0xFF006688);
const iconBackgroundBlue = Color(0xFFD4EDFC);
const appBackground = Color(0xFFF6FAFE);

ThemeData ikkTheme() {
  final scheme = ColorScheme.fromSeed(seedColor: brandBlue).copyWith(
    primary: primaryBlue,
    primaryContainer: iconBackgroundBlue,
    surface: appBackground,
  );
  return ThemeData(colorScheme: scheme, useMaterial3: true, scaffoldBackgroundColor: appBackground);
}
