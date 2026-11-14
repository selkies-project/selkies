/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! SPDX license expressions: parsed, printed in one spelling, and ranked.

use std::collections::BTreeSet;

/// License categories, ordered so the worst of an `AND` is the larger one.
///
/// File-level copyleft (MPL-2.0) is accepted wherever permissive terms are: a modified MPL
/// file stays MPL, the larger work around it may be under any license, and Selkies itself is
/// MPL-2.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    Permissive,
    FileLevel,
    Weak,
    Copyleft,
    Unknown,
}

impl Rank {
    pub fn label(self) -> &'static str {
        match self {
            Self::Permissive => "permissive",
            Self::FileLevel | Self::Weak => "weak copyleft",
            Self::Copyleft => "copyleft",
            Self::Unknown => "unknown",
        }
    }
}

const PERMISSIVE_IDS: &[&str] = &[
    "MIT",
    "MIT-0",
    "Apache-2.0",
    "BSD-1-Clause",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "0BSD",
    "ISC",
    "Zlib",
    "BSL-1.0",
    "CC0-1.0",
    "CC-BY-4.0",
    "Unlicense",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "WTFPL",
    "PSF-2.0",
    "Python-2.0",
    "NCSA",
    "OFL-1.1",
    "BlueOak-1.0.0",
    "IJG",
    "MIT-CMU",
    "X11",
    "OpenSSL",
    "Linux-syscall-note",
    "Artistic-2.0",
    "CDLA-Permissive-2.0",
    "SSLeay-standalone",
    "FTL",
    "libpng-2.0",
    "bzip2-1.0.6",
    "curl",
    "HPND",
    "ICU",
    "Beerware",
    "Fair",
];
const FILE_LEVEL_IDS: &[&str] = &[
    "MPL-2.0", "MPL-1.1", "CDDL-1.0", "CDDL-1.1", "EPL-1.0", "EPL-2.0", "CPL-1.0",
];
const WEAK_PREFIXES: &[&str] = &["LGPL-"];
const COPYLEFT_PREFIXES: &[&str] = &[
    "GPL-",
    "AGPL-",
    "SSPL-",
    "EUPL-",
    "OSL-",
    "CC-BY-SA-",
    "CC-BY-NC",
    "CECILL",
    "RPL-",
    "CPAL-",
    "QPL-",
    "Sleepycat",
];
/// Exceptions that lift the copyleft of their base license for the linked work.
///
/// An exception for what a tool generates (Bison's parser skeleton, Autoconf's configure
/// script) or for a document embedding a font lifts nothing for linked code, so it is not one.
const LINKING_EXCEPTIONS: &[&str] = &[
    "GCC-exception-3.1",
    "GCC-exception-2.0",
    "Classpath-exception-2.0",
    "LLVM-exception",
    "Linux-syscall-note",
    "GPL-3.0-linking-exception",
    "LGPL-3.0-linking-exception",
    "mif-exception",
];
/// The terms most crates offer; an expression with one of them as a complete choice is
/// listed on the page without naming its crates.
const DEFAULT_TERMS: &[&str] = &["MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception"];

/// Rank one SPDX license identifier; an identifier not classified here ranks as unknown.
fn classify_id(id: &str) -> Rank {
    if PERMISSIVE_IDS.contains(&id) {
        Rank::Permissive
    } else if FILE_LEVEL_IDS.contains(&id) {
        Rank::FileLevel
    } else if WEAK_PREFIXES.iter().any(|prefix| id.starts_with(prefix)) {
        Rank::Weak
    } else if COPYLEFT_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        Rank::Copyleft
    } else {
        Rank::Unknown
    }
}

/// `AND` or `OR`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    And,
    Or,
}

impl Op {
    fn word(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
        }
    }
}

/// A parsed expression: a license (`MIT`, `Apache-2.0 WITH LLVM-exception`) or an operator
/// over its operands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    License(String),
    Operator(Op, Vec<Expr>),
}

/// Parse an SPDX expression, the legacy `/` separator and a bare `+` suffix included.
///
/// Operands of one operator are flattened, deduplicated, and sorted, so every spelling of an
/// expression (`MIT/Apache-2.0`, `Apache-2.0 OR MIT`) parses to the same value. The error is
/// the sentence that says what does not parse.
pub fn parse(expression: &str) -> Result<Expr, String> {
    let spaced = expression
        .replace('/', " OR ")
        .replace('(', " ( ")
        .replace(')', " ) ");
    let tokens: Vec<&str> = spaced.split_whitespace().collect();
    if tokens.is_empty() {
        return Err("empty license expression".into());
    }
    let mut parser = Parser {
        expression,
        tokens,
        at: 0,
    };
    let parsed = parser.binary(Op::Or)?;
    if parser.at != parser.tokens.len() {
        return Err(format!("trailing tokens in '{expression}'"));
    }
    Ok(parsed)
}

struct Parser<'a> {
    expression: &'a str,
    tokens: Vec<&'a str>,
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.at).copied()
    }

    fn peek_is(&self, word: &str) -> bool {
        self.peek()
            .is_some_and(|token| token.eq_ignore_ascii_case(word))
    }

    /// A run of operands joined by `op`: `OR` over `AND` over atoms.
    fn binary(&mut self, op: Op) -> Result<Expr, String> {
        let operand = |parser: &mut Self| match op {
            Op::Or => parser.binary(Op::And),
            Op::And => parser.atom(),
        };
        let mut operands = vec![operand(self)?];
        while self.peek_is(op.word()) {
            self.at += 1;
            operands.push(operand(self)?);
        }
        let mut flat: Vec<Expr> = Vec::new();
        for operand in operands {
            let inner = match operand {
                Expr::Operator(inner_op, inner) if inner_op == op => inner,
                other => vec![other],
            };
            for expr in inner {
                if !flat.contains(&expr) {
                    flat.push(expr);
                }
            }
        }
        if flat.len() == 1 {
            return Ok(flat.remove(0));
        }
        flat.sort_by_key(Expr::render);
        Ok(Expr::Operator(op, flat))
    }

    fn atom(&mut self) -> Result<Expr, String> {
        let expression = self.expression;
        let Some(token) = self.peek() else {
            return Err(format!("unexpected end of '{expression}'"));
        };
        let mut token = token.to_string();
        self.at += 1;
        if token == "(" {
            let inner = self.binary(Op::Or)?;
            if self.peek() != Some(")") {
                return Err(format!("unbalanced parentheses in '{expression}'"));
            }
            self.at += 1;
            return Ok(inner);
        }
        if token == ")"
            || ["AND", "OR", "WITH"]
                .iter()
                .any(|word| token.eq_ignore_ascii_case(word))
        {
            return Err(format!("unexpected '{token}' in '{expression}'"));
        }
        if let Some(base) = token.strip_suffix('+') {
            token = format!("{base}-or-later");
        }
        if self.peek_is("WITH") {
            self.at += 1;
            let Some(exception) = self.peek() else {
                return Err(format!("dangling WITH in '{expression}'"));
            };
            token = format!("{token} WITH {exception}");
            self.at += 1;
        }
        Ok(Expr::License(token))
    }
}

impl Expr {
    /// The expression in its one spelling.
    pub fn render(&self) -> String {
        self.render_under(None)
    }

    /// Print the expression, parenthesizing an operator nested in a different one.
    fn render_under(&self, parent: Option<Op>) -> String {
        match self {
            Self::License(license) => license.clone(),
            Self::Operator(op, operands) => {
                let operands: Vec<String> = operands
                    .iter()
                    .map(|operand| operand.render_under(Some(*op)))
                    .collect();
                let text = operands.join(&format!(" {} ", op.word()));
                if parent.is_some_and(|parent| parent != *op) {
                    format!("({text})")
                } else {
                    text
                }
            }
        }
    }

    /// Rank the expression: `OR` takes the licensee's best choice, `AND` the worst.
    pub fn rank(&self) -> Rank {
        match self {
            Self::License(license) => {
                let (base, exception) = license.split_once(" WITH ").unwrap_or((license, ""));
                let rank = classify_id(base);
                if LINKING_EXCEPTIONS.contains(&exception)
                    && matches!(rank, Rank::Weak | Rank::Copyleft)
                {
                    Rank::Permissive
                } else {
                    rank
                }
            }
            Self::Operator(Op::Or, operands) => operands
                .iter()
                .map(Self::rank)
                .min()
                .unwrap_or(Rank::Unknown),
            Self::Operator(Op::And, operands) => operands
                .iter()
                .map(Self::rank)
                .max()
                .unwrap_or(Rank::Unknown),
        }
    }

    /// The sets of licenses one of which a licensee satisfies in full.
    fn choices(&self) -> Vec<BTreeSet<&str>> {
        match self {
            Self::License(license) => vec![BTreeSet::from([license.as_str()])],
            Self::Operator(Op::Or, operands) => operands.iter().flat_map(Self::choices).collect(),
            Self::Operator(Op::And, operands) => {
                operands.iter().fold(vec![BTreeSet::new()], |all, operand| {
                    let choices = operand.choices();
                    all.iter()
                        .flat_map(|a| choices.iter().map(move |b| a.union(b).copied().collect()))
                        .collect()
                })
            }
        }
    }

    /// Whether MIT or Apache-2.0 alone is one of the expression's choices.
    pub fn offers_default_terms(&self) -> bool {
        self.choices()
            .iter()
            .any(|choice| choice.iter().all(|license| DEFAULT_TERMS.contains(license)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(expression: &str) -> String {
        parse(expression).unwrap().render()
    }

    #[test]
    fn every_spelling_of_an_expression_parses_to_one() {
        assert_eq!(rendered("MIT/Apache-2.0"), "Apache-2.0 OR MIT");
        assert_eq!(rendered("MIT OR Apache-2.0 or MIT"), "Apache-2.0 OR MIT");
        assert_eq!(
            rendered("(MIT OR Apache-2.0) AND Unicode-3.0"),
            "(Apache-2.0 OR MIT) AND Unicode-3.0"
        );
        assert_eq!(
            rendered("MIT OR (Apache-2.0 OR Zlib)"),
            "Apache-2.0 OR MIT OR Zlib"
        );
        assert_eq!(rendered("GPL-2.0+"), "GPL-2.0-or-later");
        assert_eq!(
            rendered("Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT"),
            "Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR MIT"
        );
    }

    #[test]
    fn what_does_not_parse_says_why() {
        for broken in [
            "",
            "MIT OR",
            "(MIT",
            "MIT)",
            "AND MIT",
            "MIT WITH",
            "MIT Apache-2.0",
        ] {
            assert!(parse(broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn or_ranks_by_the_best_choice_and_and_by_the_worst() {
        let rank = |expression: &str| parse(expression).unwrap().rank();
        assert_eq!(rank("MIT OR GPL-3.0-only"), Rank::Permissive);
        assert_eq!(rank("MIT AND GPL-3.0-only"), Rank::Copyleft);
        assert_eq!(rank("MPL-2.0"), Rank::FileLevel);
        assert_eq!(rank("LGPL-2.1-or-later"), Rank::Weak);
        assert_eq!(
            rank("GPL-3.0-or-later WITH GCC-exception-3.1"),
            Rank::Permissive
        );
        assert_eq!(
            rank("GPL-3.0-or-later WITH Bison-exception-2.2"),
            Rank::Copyleft
        );
        assert_eq!(
            rank("GPL-2.0-or-later WITH mif-exception"),
            Rank::Permissive
        );
        assert_eq!(rank("Sleepycat"), Rank::Copyleft);
        assert_eq!(rank("CPL-1.0"), Rank::FileLevel);
        assert_eq!(rank("ICU"), Rank::Permissive);
        assert_eq!(rank("Unknown-1.0"), Rank::Unknown);
    }

    #[test]
    fn default_terms_have_to_be_a_whole_choice() {
        let offers = |expression: &str| parse(expression).unwrap().offers_default_terms();
        assert!(offers("MIT OR Unlicense"));
        assert!(offers("MIT AND Apache-2.0"));
        assert!(offers("(MIT OR Zlib) AND Apache-2.0"));
        assert!(!offers("MIT AND Unicode-3.0"));
        assert!(!offers("BSD-3-Clause"));
    }
}
