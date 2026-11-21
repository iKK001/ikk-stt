import 'package:flutter/material.dart';

import 'theme.dart';
import 'ui_strings.dart';

/// The popup behind the (i) of the button positions: how a position is
/// remembered — close to a screen edge by the distance to it, further in
/// by the share of the screen — with a drawing of each.
Future<void> showPositionInfo(BuildContext context, UiStrings t) {
  final body = Theme.of(context).textTheme.bodyLarge;
  final head = Theme.of(context).textTheme.titleMedium;
  return showDialog<void>(
    context: context,
    builder: (ctx) => AlertDialog(
      // Nearly as large as the window: the drawings need the room.
      insetPadding: const EdgeInsets.symmetric(horizontal: 24, vertical: 12),
      title: Text(t['positionInfoTitle']),
      content: SizedBox(
        // As wide as the window lets it be: the two cases stand side by
        // side, so everything fits without scrolling on a normal screen.
        width: 1680,
        child: LayoutBuilder(
          builder: (context, box) {
            Widget part(bool near) => Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Text(t[near ? 'positionInfoNearHead' : 'positionInfoFarHead'], style: head),
                    const SizedBox(height: 8),
                    _Drawing(near: near, big: t['positionInfoBig'], small: t['positionInfoSmall']),
                    const SizedBox(height: 10),
                    Text(t[near ? 'positionInfoNear' : 'positionInfoFar'], style: body, textAlign: TextAlign.justify),
                  ],
                );
            final wide = box.maxWidth >= 900;
            return SingleChildScrollView(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [
                  if (wide)
                    Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Expanded(child: part(true)),
                        const SizedBox(width: 32),
                        Expanded(child: part(false)),
                      ],
                    )
                  else ...[
                    part(true),
                    const SizedBox(height: 24),
                    part(false),
                  ],
                  const SizedBox(height: 16),
                  const Divider(height: 1),
                  const SizedBox(height: 16),
                  Text(t['positionInfoDotted'], style: body, textAlign: TextAlign.justify),
                  const SizedBox(height: 8),
                  Text(
                    t['positionInfoNote'],
                    style: Theme.of(context).textTheme.bodyMedium,
                    textAlign: TextAlign.justify,
                  ),
                ],
              ),
            );
          },
        ),
      ),
      actions: [FilledButton(onPressed: () => Navigator.pop(ctx), child: Text(t['close']))],
    ),
  );
}

class _Drawing extends StatelessWidget {
  const _Drawing({required this.near, required this.big, required this.small});
  final bool near;
  final String big, small;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return AspectRatio(
      aspectRatio: 2.2,
      child: CustomPaint(
        painter: _AnchorPainter(
          near: near,
          big: big,
          small: small,
          line: scheme.outline,
          fill: scheme.surfaceContainerHighest,
          label: Theme.of(context).textTheme.titleMedium ?? const TextStyle(fontSize: 16),
        ),
      ),
    );
  }
}

/// A large monitor on the left, a smaller one on the right, each with its
/// title above, the buttons on both. `near`: close to the right and the
/// bottom edge, the same distances x and y on both. Otherwise further in,
/// at the same share of the width and of the height on both. The dotted
/// line on each monitor is where the one rule gives way to the other.
class _AnchorPainter extends CustomPainter {
  _AnchorPainter({
    required this.near,
    required this.big,
    required this.small,
    required this.line,
    required this.fill,
    required this.label,
  });
  final bool near;
  final String big, small;
  final Color line, fill;
  final TextStyle label;

  @override
  void paint(Canvas canvas, Size size) {
    final w = size.width;
    // A band for the titles, the monitors below it.
    const band = 32.0;
    final h = size.height - band;
    final bigR = Rect.fromLTWH(0, band, w * 0.52, h);
    final smallR = Rect.fromLTWH(w * 0.65, band + h * 0.16, w * 0.35, h * 0.68);
    // The buttons and their distances to the two edges.
    // (Three columns of one, two and one button: 3 wide, 2 high.)
    // Small enough that "further in" is true on the small monitor too.
    // The dotted box is one button-row width in from every edge, so the
    // smaller the buttons are drawn, the larger the box.
    final bw = w * 0.062, bh = bw * (2 + _gap) / (3 + 2 * _gap);
    final dRight = w * 0.03, dBottom = w * 0.026;

    final screenFill = Paint()..color = fill;
    final screenLine = Paint()
      ..color = line
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.5;
    for (final (s, title) in [(bigR, big), (smallR, small)]) {
      final r = RRect.fromRectAndRadius(s, const Radius.circular(8));
      canvas.drawRRect(r, screenFill);
      canvas.drawRRect(r, screenLine);
      // Where the rule changes: while the buttons have less free room to
      // an edge than they are wide, the distance to that edge counts;
      // with more room, the share of the screen.
      _dashed(canvas, RRect.fromRectAndRadius(s.deflate(bw), const Radius.circular(4)), line);
      final tp = TextPainter(
        text: TextSpan(text: title, style: label),
        textDirection: TextDirection.ltr,
        maxLines: 1,
        ellipsis: '…',
      )..layout(maxWidth: s.width);
      tp.paint(canvas, Offset(s.center.dx - tp.width / 2, s.top - tp.height - 6));
      final name = label.copyWith(color: brandBlue, fontStyle: FontStyle.italic, fontWeight: FontWeight.w700);
      void write(String text, Offset Function(TextPainter) at) {
        final tp = TextPainter(
          text: TextSpan(text: text, style: name),
          textDirection: TextDirection.ltr,
        )..layout();
        tp.paint(canvas, at(tp));
      }

      if (near) {
        final b = Rect.fromLTWH(s.right - dRight - bw, s.bottom - dBottom - bh, bw, bh);
        _buttons(canvas, b, brandBlue);
        // From the right edge and from the bottom edge to the buttons:
        // the same x and the same y on both monitors.
        _arrow(canvas, Offset(s.right, b.center.dy), Offset(b.right + 2, b.center.dy), brandBlue, 2.5);
        _arrow(canvas, Offset(b.center.dx, s.bottom), Offset(b.center.dx, b.bottom + 2), brandBlue, 2.5);
        write('x', (tp) => Offset((b.right + s.right) / 2 - tp.width / 2, b.center.dy - tp.height - 3));
        write('y', (tp) => Offset(b.center.dx + 7, (b.bottom + s.bottom) / 2 - tp.height / 2));
      } else {
        // The centre at 40 % of the width and 45 % of the height.
        final c = Offset(s.left + s.width * 0.40, s.top + s.height * 0.45);
        final b = Rect.fromCenter(center: c, width: bw, height: bh);
        _buttons(canvas, b, brandBlue);
        _arrow(canvas, Offset(s.left, c.dy), Offset(b.left - 2, c.dy), brandBlue, 2.5);
        _arrow(canvas, Offset(c.dx, s.top), Offset(c.dx, b.top - 2), brandBlue, 2.5);
        write('40 %', (tp) => Offset((s.left + b.left) / 2 - tp.width / 2, c.dy - tp.height - 3));
        write('45 %', (tp) => Offset(c.dx + 7, (s.top + b.top) / 2 - tp.height / 2));
      }
    }
    // From one monitor to the other.
    final mid = band + h / 2;
    _arrow(canvas, Offset(bigR.right + w * 0.025, mid), Offset(smallR.left - w * 0.025, mid), line, 2.5);
  }

  /// Gap between two buttons, in button diameters.
  static const _gap = 0.16;

  /// The floating buttons as they are: the microphone, the column of the
  /// range button above the start button, and the switch — the outer two
  /// beside the middle of the column.
  void _buttons(Canvas canvas, Rect r, Color c) {
    final d = r.width / (3 + 2 * _gap);
    final radius = d / 2;
    final paint = Paint()..color = c;
    final col = [for (var i = 0; i < 3; i++) r.left + radius + i * d * (1 + _gap)];
    canvas.drawCircle(Offset(col[0], r.center.dy), radius, paint);
    canvas.drawCircle(Offset(col[1], r.top + radius), radius, paint);
    canvas.drawCircle(Offset(col[1], r.bottom - radius), radius, paint);
    canvas.drawCircle(Offset(col[2], r.center.dy), radius, paint);
  }

  void _dashed(Canvas canvas, RRect r, Color c) {
    final paint = Paint()
      ..color = c
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.5
      ..strokeCap = StrokeCap.round;
    const dash = 2.0, space = 6.0;
    for (final m in (Path()..addRRect(r)).computeMetrics()) {
      for (var d = 0.0; d < m.length; d += dash + space) {
        canvas.drawPath(m.extractPath(d, d + dash), paint);
      }
    }
  }

  void _arrow(Canvas canvas, Offset from, Offset to, Color c, double width) {
    final d = to - from;
    final len = d.distance;
    if (len < 1) return;
    final u = d / len;
    final n = Offset(-u.dy, u.dx);
    final head = len < 14 ? len * 0.5 : 7.0;
    final paint = Paint()
      ..color = c
      ..strokeWidth = width
      ..strokeCap = StrokeCap.round;
    canvas.drawLine(from, to - u * (head * 0.6), paint);
    final path = Path()
      ..moveTo(to.dx, to.dy)
      ..lineTo(to.dx - u.dx * head + n.dx * head * 0.55, to.dy - u.dy * head + n.dy * head * 0.55)
      ..lineTo(to.dx - u.dx * head - n.dx * head * 0.55, to.dy - u.dy * head - n.dy * head * 0.55)
      ..close();
    canvas.drawPath(path, Paint()..color = c);
  }

  @override
  bool shouldRepaint(_AnchorPainter old) =>
      old.near != near ||
      old.big != big ||
      old.small != small ||
      old.line != line ||
      old.fill != fill ||
      old.label != label;
}
