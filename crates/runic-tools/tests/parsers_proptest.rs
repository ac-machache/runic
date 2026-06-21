//! Property tests for the pure parsers — recovers the "never panics on
//! arbitrary input" coverage that fuzzing would give, but in plain `cargo
//! test` (no cargo-fuzz / nightly). Plus a real correctness property for the
//! calculator (evaluate a generated expression tree, compare to a reference).

use proptest::prelude::*;

// ── a generated arithmetic expression, with its own reference value ─────────

#[derive(Debug, Clone)]
enum Expr {
    Num(i64),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Reference value (exact i64; leaves and depth are bounded so it can't
    /// overflow — see `expr()`).
    fn value(&self) -> i64 {
        match self {
            Expr::Num(n) => *n,
            Expr::Add(a, b) => a.value() + b.value(),
            Expr::Sub(a, b) => a.value() - b.value(),
            Expr::Mul(a, b) => a.value() * b.value(),
        }
    }

    /// Fully parenthesized rendering — the string structure exactly mirrors the
    /// tree, so the calculator's result must equal `value()`.
    fn render(&self) -> String {
        match self {
            Expr::Num(n) => n.to_string(),
            Expr::Add(a, b) => format!("({} + {})", a.render(), b.render()),
            Expr::Sub(a, b) => format!("({} - {})", a.render(), b.render()),
            Expr::Mul(a, b) => format!("({} * {})", a.render(), b.render()),
        }
    }
}

/// Small leaves (0..10) + shallow depth → exact integer arithmetic that stays
/// well within i64 and f64's exact range, so comparison is exact.
fn expr() -> impl Strategy<Value = Expr> {
    let leaf = (0i64..10).prop_map(Expr::Num);
    leaf.prop_recursive(3, 16, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::Add(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::Sub(Box::new(a), Box::new(b))),
            (inner.clone(), inner).prop_map(|(a, b)| Expr::Mul(Box::new(a), Box::new(b))),
        ]
    })
}

proptest! {
    // ── never-panic (the fuzz replacement) ──────────────────────────────────

    #[test]
    fn decode_entities_never_panics(s in any::<String>()) {
        let _ = runic_tools::decode_entities(&s);
    }

    #[test]
    fn html_to_text_never_panics(s in any::<String>()) {
        let _ = runic_tools::html_to_text(&s);
    }

    #[test]
    fn eval_never_panics(s in any::<String>()) {
        let _ = runic_tools::eval_calc(&s);
    }

    // ── correctness ─────────────────────────────────────────────────────────

    /// A string with no `&` is returned unchanged (incl. multibyte text).
    #[test]
    fn decode_entities_noop_without_amp(s in "[^&]{0,120}") {
        prop_assert_eq!(runic_tools::decode_entities(&s), s);
    }

    /// The calculator evaluates any generated expression tree to its reference
    /// value (validates parsing + arithmetic over arbitrary nesting).
    #[test]
    fn eval_matches_reference(e in expr()) {
        let got = runic_tools::eval_calc(&e.render()).expect("generated expr is valid");
        prop_assert!((got - e.value() as f64).abs() < 1e-9, "{} → {got}, want {}", e.render(), e.value());
    }
}
