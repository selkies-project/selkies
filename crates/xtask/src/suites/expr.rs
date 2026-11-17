/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The expressions `--match` takes: words joined by `and`, `or`, `not` and parentheses, `not`
//! binding tightest and `or` loosest; a word holds where it is a case-insensitive part of a case's
//! id, or names its tier.

/// A parsed expression.
#[derive(Debug)]
pub enum Expr {
    Word(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Whether the expression holds for the case `id` of `tier`.
    pub fn matches(&self, id: &str, tier: &str) -> bool {
        match self {
            Expr::Word(w) => {
                id.to_lowercase().contains(&w.to_lowercase()) || w.eq_ignore_ascii_case(tier)
            }
            Expr::Not(e) => !e.matches(id, tier),
            Expr::And(a, b) => a.matches(id, tier) && b.matches(id, tier),
            Expr::Or(a, b) => a.matches(id, tier) || b.matches(id, tier),
        }
    }
}

fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() || c == '(' || c == ')' {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            if c != ' ' && !c.is_whitespace() {
                out.push(c.to_string());
            }
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

struct Parser {
    tokens: Vec<String>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.at).map(String::as_str)
    }

    fn take(&mut self) -> Option<String> {
        let t = self.tokens.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.peek() == Some("or") {
            self.at += 1;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.not()?;
        while self.peek() == Some("and") {
            self.at += 1;
            left = Expr::And(Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, String> {
        if self.peek() == Some("not") {
            self.at += 1;
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Expr, String> {
        match self.take().as_deref() {
            Some("(") => {
                let inner = self.or()?;
                if self.take().as_deref() != Some(")") {
                    return Err("an unclosed parenthesis".into());
                }
                Ok(inner)
            }
            Some(")" | "and" | "or") | None => Err("a word missing".into()),
            Some(word) => Ok(Expr::Word(word.to_owned())),
        }
    }
}

/// `text` as an expression.
///
/// # Errors
///
/// Where it is not one.
pub fn parse(text: &str) -> Result<Expr, String> {
    let mut p = Parser {
        tokens: tokens(text),
        at: 0,
    };
    let e = p.or().map_err(|why| format!("--match {text:?}: {why}"))?;
    if p.at < p.tokens.len() {
        return Err(format!("--match {text:?}: more after the expression"));
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn words_join_as_pytest_reads_them() {
        let e = parse("portal or (codecs and not ws-x11)").expect("an expression");
        assert!(e.matches("e2e:test_wayland_host_portal-gnome", "e2e"));
        assert!(e.matches("e2e:test_codecs-wr-x11", "e2e"));
        assert!(!e.matches("e2e:test_codecs-ws-x11-chromium", "e2e"));
        assert!(
            parse("unit")
                .expect("a word")
                .matches("unit:test_i18n_keys", "unit")
        );
        assert!(parse("(a").is_err());
        assert!(parse("a or").is_err());
    }
}
