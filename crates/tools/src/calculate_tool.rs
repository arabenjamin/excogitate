//! The `calculate` tool: arithmetic a model shouldn't do in its head.
//!
//! A small model gets sums, shares and percentages wrong, and a reply that
//! looks like a calculation isn't one. This evaluates an expression of
//! numbers only, deterministically: there are no names but a few functions
//! and constants, so nothing it's given can run or read anything.

use async_trait::async_trait;
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::Trust;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DESCRIPTION: &str = "Works out an arithmetic expression exactly: `+ - * /`, `^` for a \
power, `%` after a number for a percent (`18% * 2.40`), parentheses, and round(x) or round(x, \
digits), floor, ceil, abs, sqrt, min, max, pi and e. Numbers only: it can't count a list or \
read a result, so write each number out, from the question or a result (such as its `total`).";

const MAX_EXPRESSION: usize = 500;
/// How deep parentheses and unary signs may nest.
const MAX_DEPTH: usize = 64;
const MAX_DIGITS: i32 = 12;

/// What the model asks.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CalculateArgs {
    /// The expression, such as `(1074 - 207) / 1074 * 100`.
    #[schemars(length(min = 1, max = MAX_EXPRESSION))]
    expression: String,
}

/// What it worked out.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct CalculateResult {
    /// The expression, as given.
    #[schemars(length(max = MAX_EXPRESSION))]
    expression: String,
    /// Its value.
    value: f64,
    /// The value written out, to 12 significant digits.
    #[schemars(length(max = 64))]
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Number(f64),
    Name(usize, usize),
    Op(char),
}

fn tokens(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(&c) = chars.get(at) {
        if c.is_whitespace() || c == '_' {
            at += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let start = at;
            while chars
                .get(at)
                .is_some_and(|c| c.is_ascii_digit() || *c == '.' || *c == '_')
            {
                at += 1;
            }
            // An exponent: 1e3, 2.5E-4.
            if chars.get(at).is_some_and(|c| *c == 'e' || *c == 'E')
                && chars
                    .get(at + 1)
                    .is_some_and(|c| c.is_ascii_digit() || *c == '-' || *c == '+')
            {
                at += 2;
                while chars.get(at).is_some_and(char::is_ascii_digit) {
                    at += 1;
                }
            }
            let written: String = chars
                .get(start..at)
                .unwrap_or_default()
                .iter()
                .filter(|c| **c != '_')
                .collect();
            let number = written
                .parse::<f64>()
                .map_err(|_| format!("`{written}` isn't a number"))?;
            out.push(Token::Number(number));
        } else if c.is_ascii_alphabetic() {
            let start = at;
            while chars.get(at).is_some_and(char::is_ascii_alphanumeric) {
                at += 1;
            }
            out.push(Token::Name(start, at));
        } else if "+-*/^%(),".contains(c) {
            out.push(Token::Op(c));
            at += 1;
        } else if c == '×' {
            out.push(Token::Op('*'));
            at += 1;
        } else if c == '÷' {
            out.push(Token::Op('/'));
            at += 1;
        } else {
            return Err(format!("`{c}` isn't part of arithmetic"));
        }
    }
    Ok(out)
}

/// A recursive-descent reading of the tokens:
/// expr = term (+|- term)*; term = unary (*|/ unary)*;
/// unary = (+|-) unary | power; power = postfix (^ unary)?;
/// postfix = primary %*; primary = number | name [( args )] | ( expr ).
struct Parser<'a> {
    text: Vec<char>,
    tokens: &'a [Token],
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.at).copied()
    }

    fn eat(&mut self, op: char) -> bool {
        if self.peek() == Some(Token::Op(op)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn deeper(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("the expression nests too deeply".to_owned());
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<f64, String> {
        let mut value = self.term()?;
        loop {
            if self.eat('+') {
                value += self.term()?;
            } else if self.eat('-') {
                value -= self.term()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut value = self.unary()?;
        loop {
            if self.eat('*') {
                value *= self.unary()?;
            } else if self.eat('/') {
                let divisor = self.unary()?;
                if divisor == 0.0 {
                    return Err("division by zero".to_owned());
                }
                value /= divisor;
            } else {
                return Ok(value);
            }
        }
    }

    fn unary(&mut self) -> Result<f64, String> {
        self.deeper()?;
        let value = if self.eat('-') {
            -self.unary()?
        } else if self.eat('+') {
            self.unary()?
        } else {
            self.power()?
        };
        self.depth -= 1;
        Ok(value)
    }

    fn power(&mut self) -> Result<f64, String> {
        let base = self.postfix()?;
        if self.eat('^') {
            return Ok(base.powf(self.unary()?));
        }
        Ok(base)
    }

    fn postfix(&mut self) -> Result<f64, String> {
        let mut value = self.primary()?;
        while self.eat('%') {
            value /= 100.0;
        }
        Ok(value)
    }

    fn primary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Token::Number(number)) => {
                self.at += 1;
                Ok(number)
            }
            Some(Token::Op('(')) => {
                self.at += 1;
                self.deeper()?;
                let value = self.expr()?;
                self.depth -= 1;
                if !self.eat(')') {
                    return Err("a `(` isn't closed".to_owned());
                }
                Ok(value)
            }
            Some(Token::Name(start, end)) => {
                self.at += 1;
                let name: String = self
                    .text
                    .get(start..end)
                    .unwrap_or_default()
                    .iter()
                    .collect::<String>()
                    .to_lowercase();
                self.named(&name)
            }
            Some(Token::Op(op)) => Err(format!("`{op}` where a number belongs")),
            None => Err("the expression ends where a number belongs".to_owned()),
        }
    }

    fn arguments(&mut self) -> Result<Vec<f64>, String> {
        if !self.eat('(') {
            return Err("a function needs its arguments in parentheses".to_owned());
        }
        self.deeper()?;
        let mut values = vec![self.expr()?];
        while self.eat(',') {
            values.push(self.expr()?);
        }
        self.depth -= 1;
        if !self.eat(')') {
            return Err("a `(` isn't closed".to_owned());
        }
        Ok(values)
    }

    fn named(&mut self, name: &str) -> Result<f64, String> {
        match name {
            "pi" => return Ok(std::f64::consts::PI),
            "e" => return Ok(std::f64::consts::E),
            _ => {}
        }
        let args = self.arguments()?;
        let one = |args: &[f64]| match args {
            [x] => Ok(*x),
            _ => Err(format!("{name} takes one number")),
        };
        match name {
            "abs" => Ok(one(&args)?.abs()),
            "floor" => Ok(one(&args)?.floor()),
            "ceil" => Ok(one(&args)?.ceil()),
            "sqrt" => {
                let x = one(&args)?;
                if x < 0.0 {
                    return Err("sqrt of a negative number".to_owned());
                }
                Ok(x.sqrt())
            }
            "round" => match args.as_slice() {
                [x] => Ok(x.round()),
                [x, digits] if digits.fract() == 0.0 && (0.0..=12.0).contains(digits) => {
                    let scale = 10f64.powf(*digits);
                    Ok((x * scale).round() / scale)
                }
                _ => Err("round takes a number, and digits from 0 to 12".to_owned()),
            },
            "min" => Ok(args.into_iter().fold(f64::INFINITY, f64::min)),
            "max" => Ok(args.into_iter().fold(f64::NEG_INFINITY, f64::max)),
            other => Err(format!("`{other}` isn't a function this knows")),
        }
    }
}

/// The value of `expression`, or why there isn't one.
///
/// # Errors
///
/// Why it can't be worked out: a character or name that isn't arithmetic,
/// a malformed expression, division by zero, or a result too large.
pub(crate) fn evaluate(expression: &str) -> Result<f64, String> {
    if expression.chars().count() > MAX_EXPRESSION {
        return Err(format!(
            "the expression is over {MAX_EXPRESSION} characters"
        ));
    }
    let tokens = tokens(expression)?;
    let mut parser = Parser {
        text: expression.chars().collect(),
        tokens: &tokens,
        at: 0,
        depth: 0,
    };
    let value = parser.expr()?;
    if let Some(left) = parser.peek() {
        return Err(match left {
            Token::Op(op) => format!("`{op}` where the expression should end"),
            _ => "two values with nothing between them".to_owned(),
        });
    }
    if !value.is_finite() {
        return Err("the result isn't a finite number".to_owned());
    }
    Ok(value)
}

/// `value` to 12 significant digits, without trailing zeros.
fn written(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a finite value's base-10 exponent is within ±310"
    )]
    let magnitude = value.abs().log10().floor() as i32;
    let decimals = usize::try_from((MAX_DIGITS - 1 - magnitude).max(0)).unwrap_or(0);
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

/// Works out arithmetic.
#[derive(Debug, Clone, Default)]
pub struct CalculateTool;

impl CalculateTool {
    /// The tool.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The tool's input and output schemas, as published.
    #[must_use]
    pub fn schemas() -> (Value, Value) {
        (
            schema::schema_of::<CalculateArgs>(),
            schema::schema_of::<CalculateResult>(),
        )
    }
}

#[async_trait]
impl Tool for CalculateTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::for_types::<CalculateArgs, CalculateResult>(
            "calculate",
            DESCRIPTION,
            Effect::Read,
        )
    }

    async fn run(
        &self,
        arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolFailure> {
        let args: CalculateArgs = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        let value = evaluate(&args.expression).map_err(ToolFailure::new)?;
        let found = CalculateResult {
            text: written(value),
            expression: args.expression,
            value,
        };
        let value = serde_json::to_value(found)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        // Worked out here, from numbers alone: nothing in it can instruct.
        Ok(ToolOutput::new(value, Trust::Trusted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is(expression: &str, expected: f64) {
        let got = evaluate(expression).unwrap();
        assert!(
            (got - expected).abs() < 1e-9,
            "{expression} = {got}, not {expected}"
        );
    }

    #[test]
    fn arithmetic_is_worked_out_in_the_usual_order() {
        is("1 + 2 * 3", 7.0);
        is("(1 + 2) * 3", 9.0);
        is("2 ^ 3 ^ 2", 512.0);
        is("-2 ^ 2", -4.0);
        is("10 / 4", 2.5);
        is("18% * 2.40", 0.432);
        is("(1074 - 207) / 1074 * 100", 80.726_256_983_240_22);
        is("round(2 / 3, 2)", 0.67);
        is("max(3, 9, 4) - min(3, 9, 4) + abs(-1) + sqrt(16)", 11.0);
        is("1_074 + 1e3", 2074.0);
        is("2 × 3 ÷ 4", 1.5);
        // π to 4 places, as a whole number: clippy rightly doubts a literal π.
        is("round(pi, 4) * 10000", 31416.0);
    }

    #[test]
    fn what_isnt_arithmetic_is_refused_and_says_why() {
        for (expression, why) in [
            ("1 / 0", "division by zero"),
            ("1 +", "ends"),
            ("(1 + 2", "isn't closed"),
            ("2 3", "two values"),
            ("system(1)", "isn't a function"),
            ("1; drop", "isn't part of arithmetic"),
            ("sqrt(-1)", "negative"),
            ("10 ^ 400", "finite"),
            ("round(1, 99)", "digits"),
        ] {
            let error = evaluate(expression).unwrap_err();
            assert!(error.contains(why), "{expression}: {error}");
        }
        assert!(evaluate(&"(".repeat(100)).unwrap_err().contains("deeply"));
        assert!(evaluate(&"1+".repeat(300)).unwrap_err().contains("over"));
    }

    #[test]
    fn a_value_is_written_to_twelve_significant_digits() {
        assert_eq!(written(0.432), "0.432");
        assert_eq!(written(80.726_256_983_240_22), "80.7262569832");
        assert_eq!(written(2074.0), "2074");
        assert_eq!(written(-0.5), "-0.5");
        assert_eq!(written(0.0), "0");
    }
}
