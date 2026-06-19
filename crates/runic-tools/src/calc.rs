//! `calculator` — evaluate an arithmetic expression. LLMs are unreliable at
//! exact math; this gives them a precise calculator. A tiny recursive-descent
//! evaluator (no deps): `+ - * / %`, parentheses, unary minus, decimals.

use async_trait::async_trait;

use runic_tool::{Tool, ToolContext, ToolResult};

pub struct CalculatorTool;

#[async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }
    fn description(&self) -> &str {
        "Evaluate an arithmetic expression for an exact result. Supports \
         + - * / %, parentheses, and decimals, e.g. \"(3 + 4) * 2.5\"."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "expression": { "type": "string" } },
            "required": ["expression"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(expr) = args.get("expression").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("calculator requires `expression`"));
        };
        Ok(match eval(expr) {
            Ok(v) => ToolResult::ok(format_num(v)),
            Err(e) => ToolResult::error(e),
        })
    }
}

fn format_num(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[derive(Debug, Clone, Copy)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '+' => { toks.push(Tok::Plus); i += 1; }
            '-' => { toks.push(Tok::Minus); i += 1; }
            '*' => { toks.push(Tok::Star); i += 1; }
            '/' => { toks.push(Tok::Slash); i += 1; }
            '%' => { toks.push(Tok::Percent); i += 1; }
            '(' => { toks.push(Tok::LParen); i += 1; }
            ')' => { toks.push(Tok::RParen); i += 1; }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let num: String = chars[start..i].iter().collect();
                toks.push(Tok::Num(num.parse().map_err(|_| format!("invalid number '{num}'"))?));
            }
            other => return Err(format!("unexpected character '{other}'")),
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<Tok> {
        self.toks.get(self.pos).copied()
    }

    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        while let Some(t) = self.peek() {
            match t {
                Tok::Plus => { self.pos += 1; v += self.term()?; }
                Tok::Minus => { self.pos += 1; v -= self.term()?; }
                _ => break,
            }
        }
        Ok(v)
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.factor()?;
        while let Some(t) = self.peek() {
            match t {
                Tok::Star => { self.pos += 1; v *= self.factor()?; }
                Tok::Slash => {
                    self.pos += 1;
                    let d = self.factor()?;
                    if d == 0.0 {
                        return Err("division by zero".into());
                    }
                    v /= d;
                }
                Tok::Percent => {
                    self.pos += 1;
                    let d = self.factor()?;
                    if d == 0.0 {
                        return Err("modulo by zero".into());
                    }
                    v %= d;
                }
                _ => break,
            }
        }
        Ok(v)
    }

    fn factor(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Tok::Num(n)) => { self.pos += 1; Ok(n) }
            Some(Tok::Minus) => { self.pos += 1; Ok(-self.factor()?) }
            Some(Tok::Plus) => { self.pos += 1; self.factor() }
            Some(Tok::LParen) => {
                self.pos += 1;
                let v = self.expr()?;
                match self.peek() {
                    Some(Tok::RParen) => { self.pos += 1; Ok(v) }
                    _ => Err("expected ')'".into()),
                }
            }
            _ => Err("expected a number or '('".into()),
        }
    }
}

fn eval(input: &str) -> Result<f64, String> {
    if input.trim().is_empty() {
        return Err("empty expression".into());
    }
    let toks = tokenize(input)?;
    let mut p = Parser { toks, pos: 0 };
    let v = p.expr()?;
    if p.pos != p.toks.len() {
        return Err("unexpected trailing input".into());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates() {
        assert_eq!(eval("(3 + 4) * 2").unwrap(), 14.0);
        assert_eq!(eval("10 / 4").unwrap(), 2.5);
        assert_eq!(eval("-2 * -3").unwrap(), 6.0);
        assert_eq!(eval("17 % 5").unwrap(), 2.0);
        assert!(eval("1 / 0").is_err());
        assert!(eval("2 +").is_err());
        assert!(eval("2 3").is_err());
    }

    #[test]
    fn formats_integers_cleanly() {
        assert_eq!(format_num(14.0), "14");
        assert_eq!(format_num(2.5), "2.5");
    }
}
