//! Test helper: a left click at screen point x y (points, origin top-left),
//! with `shift` as third argument for a Shift-click.
//! `cargo run -p ikk-output --example click -- 100 200 [shift]`
fn main() {
    {
        let a: Vec<String> = std::env::args().collect();
        let x: f64 = a.get(1).and_then(|s| s.parse().ok()).expect("x");
        let y: f64 = a.get(2).and_then(|s| s.parse().ok()).expect("y");
        let shift = a.get(3).is_some_and(|s| s == "shift");
        // `click x y drag x2 y2`: press at (x, y), move, release at (x2, y2).
        if a.get(3).is_some_and(|s| s == "drag") {
            let x2: f64 = a.get(4).and_then(|s| s.parse().ok()).expect("x2");
            let y2: f64 = a.get(5).and_then(|s| s.parse().ok()).expect("y2");
            ikk_output::desktop::drag(x, y, x2, y2).expect("drag");
            return;
        }
        // `click x y double`: a double-click (selects the word under it).
        if a.get(3).is_some_and(|s| s == "double") {
            ikk_output::desktop::double_click_at(x, y).expect("double click");
            return;
        }
        ikk_output::desktop::click_at(x, y, shift).expect("click");
    }
}
