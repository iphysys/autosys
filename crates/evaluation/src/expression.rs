//! Reference expressions in benchmark outcomes: arithmetic over parameter values.
//!
//! An expression is data. It is parsed into a tree and evaluated by walking
//! it, never executed, and the grammar holds nothing but arithmetic:
//!
//! ```text
//! expr   = term (("+" | "-") term)*
//! term   = unary (("*" | "/") unary)*
//! unary  = "-" unary | power
//! power  = atom ["^" unary]          (right-associative; -x^2 is -(x^2))
//! atom   = number | name | "(" expr ")"
//! number = digits ["." digits] [("e" | "E") ["+" | "-"] digits]
//! name   = letter (letter | digit | "_")*      `pi`, or a variable
//! ```

use std::collections::{BTreeMap, BTreeSet};

/// Deeper nesting is refused, so a hostile file cannot exhaust the stack.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(f64),
    Pi,
    Variable(String),
    Neg(Box<Expr>),
    Binary(Op, Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("at character {position}: {message}")]
pub struct ParseError {
    pub position: usize,
    pub message: String,
}

/// Parses `text`, or reports the first position where it is not arithmetic.
pub fn parse(text: &str) -> Result<Expr, ParseError> {
    let tokens = tokenize(text)?;
    let mut parser = Parser {
        tokens: &tokens,
        next: 0,
        end: text.len(),
    };
    let expr = parser.expr(0)?;
    match parser.peek() {
        None => Ok(expr),
        Some((position, token)) => Err(ParseError {
            position,
            message: format!("unexpected {token}"),
        }),
    }
}

impl Expr {
    /// The variable names the expression uses.
    pub fn variables(&self) -> BTreeSet<&str> {
        let mut names = BTreeSet::new();
        self.collect(&mut names);
        names
    }

    fn collect<'a>(&'a self, names: &mut BTreeSet<&'a str>) {
        match self {
            Expr::Number(_) | Expr::Pi => {}
            Expr::Variable(name) => {
                names.insert(name);
            }
            Expr::Neg(inner) => inner.collect(names),
            Expr::Binary(_, left, right) => {
                left.collect(names);
                right.collect(names);
            }
        }
    }

    /// The value for the given variable values; `None` if a variable is
    /// missing or the result is not a finite number.
    pub fn evaluate(&self, values: &BTreeMap<&str, f64>) -> Option<f64> {
        let value = match self {
            Expr::Number(n) => *n,
            Expr::Pi => std::f64::consts::PI,
            Expr::Variable(name) => *values.get(name.as_str())?,
            Expr::Neg(inner) => -inner.evaluate(values)?,
            Expr::Binary(op, left, right) => {
                let (a, b) = (left.evaluate(values)?, right.evaluate(values)?);
                match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div => a / b,
                    Op::Pow => a.powf(b),
                }
            }
        };
        value.is_finite().then_some(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Name(String),
    Symbol(char),
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Number(n) => write!(f, "number {n}"),
            Token::Name(name) => write!(f, "name `{name}`"),
            Token::Symbol(c) => write!(f, "`{c}`"),
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<(usize, Token)>, ParseError> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while let Some(&(position, c)) = chars.get(i) {
        if c == ' ' {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            let digits = |i: &mut usize| {
                while chars.get(*i).is_some_and(|(_, c)| c.is_ascii_digit()) {
                    *i += 1;
                }
            };
            digits(&mut i);
            if chars.get(i).is_some_and(|(_, c)| *c == '.') {
                i += 1;
                if !chars.get(i).is_some_and(|(_, c)| c.is_ascii_digit()) {
                    return Err(error(
                        position,
                        "a decimal point must be followed by digits",
                    ));
                }
                digits(&mut i);
            }
            if chars.get(i).is_some_and(|(_, c)| matches!(c, 'e' | 'E')) {
                let mut j = i + 1;
                if chars.get(j).is_some_and(|(_, c)| matches!(c, '+' | '-')) {
                    j += 1;
                }
                if chars.get(j).is_some_and(|(_, c)| c.is_ascii_digit()) {
                    i = j;
                    digits(&mut i);
                }
            }
            let literal: String = chars[start..i].iter().map(|(_, c)| c).collect();
            let value = literal
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| error(position, &format!("`{literal}` is not a finite number")))?;
            tokens.push((position, Token::Number(value)));
        } else if c.is_ascii_alphabetic() {
            let start = i;
            while chars
                .get(i)
                .is_some_and(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
            {
                i += 1;
            }
            let name: String = chars[start..i].iter().map(|(_, c)| c).collect();
            tokens.push((position, Token::Name(name)));
        } else if "+-*/^()".contains(c) {
            tokens.push((position, Token::Symbol(c)));
            i += 1;
        } else {
            return Err(error(position, &format!("`{c}` is not allowed")));
        }
    }
    Ok(tokens)
}

fn error(position: usize, message: &str) -> ParseError {
    ParseError {
        position,
        message: message.to_owned(),
    }
}

struct Parser<'a> {
    tokens: &'a [(usize, Token)],
    next: usize,
    end: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<(usize, &Token)> {
        self.tokens.get(self.next).map(|(p, t)| (*p, t))
    }

    fn eat(&mut self, symbol: char) -> bool {
        let found = matches!(self.peek(), Some((_, Token::Symbol(c))) if *c == symbol);
        if found {
            self.next += 1;
        }
        found
    }

    fn deeper(depth: usize, position: usize) -> Result<usize, ParseError> {
        if depth >= MAX_DEPTH {
            return Err(error(position, "the expression is nested too deeply"));
        }
        Ok(depth + 1)
    }

    fn position(&self) -> usize {
        self.peek().map_or(self.end, |(p, _)| p)
    }

    fn expr(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let depth = Self::deeper(depth, self.position())?;
        let mut left = self.term(depth)?;
        loop {
            let op = if self.eat('+') {
                Op::Add
            } else if self.eat('-') {
                Op::Sub
            } else {
                return Ok(left);
            };
            left = Expr::Binary(op, Box::new(left), Box::new(self.term(depth)?));
        }
    }

    fn term(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let mut left = self.unary(depth)?;
        loop {
            let op = if self.eat('*') {
                Op::Mul
            } else if self.eat('/') {
                Op::Div
            } else {
                return Ok(left);
            };
            left = Expr::Binary(op, Box::new(left), Box::new(self.unary(depth)?));
        }
    }

    fn unary(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let depth = Self::deeper(depth, self.position())?;
        if self.eat('-') {
            return Ok(Expr::Neg(Box::new(self.unary(depth)?)));
        }
        let base = self.atom(depth)?;
        if self.eat('^') {
            let exponent = self.unary(depth)?;
            return Ok(Expr::Binary(Op::Pow, Box::new(base), Box::new(exponent)));
        }
        Ok(base)
    }

    fn atom(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let position = self.position();
        let Some((_, token)) = self.peek() else {
            return Err(error(position, "the expression ends too early"));
        };
        let token = token.clone();
        self.next += 1;
        match token {
            Token::Number(n) => Ok(Expr::Number(n)),
            Token::Name(name) if name == "pi" => Ok(Expr::Pi),
            Token::Name(name) => Ok(Expr::Variable(name)),
            Token::Symbol('(') => {
                let inner = self.expr(depth)?;
                if !self.eat(')') {
                    return Err(error(self.position(), "expected `)`"));
                }
                Ok(inner)
            }
            other => Err(error(position, &format!("unexpected {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(text: &str, values: &[(&str, f64)]) -> Option<f64> {
        parse(text)
            .unwrap()
            .evaluate(&values.iter().copied().collect())
    }

    #[test]
    fn precedence_and_associativity() {
        assert_eq!(eval("1 + 2 * 3", &[]), Some(7.0));
        assert_eq!(eval("(1 + 2) * 3", &[]), Some(9.0));
        assert_eq!(eval("8 / 4 / 2", &[]), Some(1.0));
        assert_eq!(eval("2 ^ 3 ^ 2", &[]), Some(512.0));
        assert_eq!(eval("-3 ^ 2", &[]), Some(-9.0));
        assert_eq!(eval("2 ^ -1", &[]), Some(0.5));
        assert_eq!(eval("10 - 4 - 3", &[]), Some(3.0));
    }

    #[test]
    fn numbers_constants_and_variables() {
        assert_eq!(eval("2.70e-3", &[]), Some(2.70e-3));
        assert_eq!(eval("1E+2", &[]), Some(100.0));
        assert_eq!(eval("pi", &[]), Some(std::f64::consts::PI));
        assert_eq!(eval("L * W_2", &[("L", 3.0), ("W_2", 4.0)]), Some(12.0));
        assert_eq!(
            parse("a * (b + a) - pi").unwrap().variables(),
            BTreeSet::from(["a", "b"])
        );
    }

    #[test]
    fn missing_variables_and_non_finite_results_have_no_value() {
        assert_eq!(eval("L * 2", &[]), None);
        assert_eq!(eval("1 / 0", &[]), None);
        assert_eq!(eval("10 ^ 400", &[]), None);
    }

    #[test]
    fn anything_but_arithmetic_is_refused() {
        for text in [
            "",
            "2 *",
            "(1 + 2",
            "1 + 2)",
            "2L",
            "__import__('os')",
            "f(1)",
            "1 2",
            "1.",
            "a.b",
            "1e999",
            "x == 1",
        ] {
            assert!(parse(text).is_err(), "{text:?} parsed");
        }
    }

    #[test]
    fn deep_nesting_is_refused() {
        let text = format!("{}1{}", "(".repeat(200), ")".repeat(200));
        assert!(parse(&text).is_err());
        let text = format!("{}1", "-".repeat(200));
        assert!(parse(&text).is_err());
    }
}
