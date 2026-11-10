//! The syntax tree and a recursive-descent parser for the subset of JavaScript Broxee runs:
//! statements, functions and arrows, objects and arrays, destructuring, template literals,
//! spread, optional chaining. Classes, generators, getters and setters are not parsed; a
//! script using them fails to parse and is skipped as a whole.

use std::sync::Arc;

use crate::lexer::{tokenize, Tok, Token};

pub type Name = Arc<str>;

#[derive(Debug)]
pub enum Expr {
    Num(f64),
    Str(Name),
    Bool(bool),
    Null,
    Template(Vec<Name>, Vec<Expr>),
    Regex(Name),
    Ident(Name),
    This,
    Array(Vec<Elem>),
    Object(Vec<Prop>),
    Function(Arc<Func>),
    Unary(&'static str, Box<Expr>),
    Update(&'static str, bool, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Logical(&'static str, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Assign(&'static str, Box<Expr>, Box<Expr>),
    /// Object, key, optional (`?.`).
    Member(Box<Expr>, Box<Expr>, bool),
    Call(Box<Expr>, Vec<Elem>, bool),
    New(Box<Expr>, Vec<Elem>),
    Seq(Vec<Expr>),
}

#[derive(Debug)]
pub enum Elem {
    Item(Expr),
    Spread(Expr),
    Hole,
}

#[derive(Debug)]
pub enum Prop {
    Init(Expr, Expr),
    Spread(Expr),
}

#[derive(Debug)]
pub enum Pattern {
    Name(Name),
    /// Keys with their patterns and defaults, and a rest name.
    Object(Vec<(Expr, Pattern, Option<Expr>)>, Option<Name>),
    Array(Vec<Option<(Pattern, Option<Expr>)>>, Option<Name>),
}

#[derive(Debug)]
pub struct Param {
    pub pattern: Pattern,
    pub default: Option<Expr>,
    pub rest: bool,
}

#[derive(Debug)]
pub struct Func {
    pub name: Option<Name>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub arrow: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug)]
pub enum Stmt {
    Expr(Expr),
    Var(VarKind, Vec<(Pattern, Option<Expr>)>),
    Function(Arc<Func>),
    Return(Option<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    Block(Vec<Stmt>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    /// Declaration kind (if declared), target, object, body; `true` for `of`.
    ForIn(Option<VarKind>, Pattern, Expr, Box<Stmt>, bool),
    Break(Option<Name>),
    Continue(Option<Name>),
    Throw(Expr),
    Try(
        Vec<Stmt>,
        Option<(Option<Pattern>, Vec<Stmt>)>,
        Option<Vec<Stmt>>,
    ),
    Switch(Expr, Vec<(Option<Expr>, Vec<Stmt>)>),
    Labeled(Name, Box<Stmt>),
    Empty,
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// Inside a `for (...;` head, where `in` is not an operator.
    no_in: bool,
    /// How deeply expressions nest, so a hostile script cannot exhaust the stack.
    depth: usize,
}

const NESTING_LIMIT: usize = 300;

pub fn parse(src: &str) -> Result<Vec<Stmt>, String> {
    let mut p = Parser {
        toks: tokenize(src)?,
        pos: 0,
        no_in: false,
        depth: 0,
    };
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.statement()?);
    }
    Ok(out)
}

const RESERVED: &[&str] = &[
    "var",
    "let",
    "const",
    "function",
    "return",
    "if",
    "else",
    "for",
    "while",
    "do",
    "break",
    "continue",
    "throw",
    "try",
    "catch",
    "finally",
    "switch",
    "case",
    "default",
    "new",
    "delete",
    "typeof",
    "void",
    "in",
    "instanceof",
    "this",
    "null",
    "true",
    "false",
    "class",
    "extends",
    "super",
    "import",
    "export",
];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.pos + k).min(self.toks.len() - 1)].tok
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn is(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Ident(n) if n == w)
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn error<T>(&self, what: &str) -> Result<T, String> {
        let t = &self.toks[self.pos];
        Err(format!("line {}: {what}, found {:?}", t.line, t.tok))
    }

    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.eat(p) {
            Ok(())
        } else {
            self.error(&format!("expected `{p}`"))
        }
    }

    fn name(&mut self) -> Result<Name, String> {
        match self.peek().clone() {
            Tok::Ident(n) if !RESERVED.contains(&n.as_str()) => {
                self.pos += 1;
                Ok(n.into())
            }
            _ => self.error("expected a name"),
        }
    }

    /// End of a statement: `;`, or an implied one before `}`, the end, or a line break.
    fn semicolon(&mut self) -> Result<(), String> {
        if self.eat(";") || self.is("}") || self.at_eof() || self.toks[self.pos].newline_before {
            Ok(())
        } else {
            self.error("expected `;`")
        }
    }

    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect("{")?;
        let mut out = Vec::new();
        while !self.eat("}") {
            if self.at_eof() {
                return self.error("expected `}`");
            }
            out.push(self.statement()?);
        }
        Ok(out)
    }

    fn var_kind(&self) -> Option<VarKind> {
        match self.peek() {
            Tok::Ident(w) if w == "var" => Some(VarKind::Var),
            Tok::Ident(w) if w == "const" => Some(VarKind::Const),
            // `let` as a declaration only when a name or pattern follows.
            Tok::Ident(w) if w == "let" => match self.peek_at(1) {
                Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{") => Some(VarKind::Let),
                _ => None,
            },
            _ => None,
        }
    }

    fn statement(&mut self) -> Result<Stmt, String> {
        self.depth += 1;
        if self.depth > NESTING_LIMIT {
            return self.error("statements nest too deeply");
        }
        let r = self.statement_inner();
        self.depth -= 1;
        r
    }

    fn statement_inner(&mut self) -> Result<Stmt, String> {
        if let Some(kind) = self.var_kind() {
            self.pos += 1;
            let decls = self.declarations()?;
            self.semicolon()?;
            return Ok(Stmt::Var(kind, decls));
        }
        let word = match self.peek() {
            Tok::Ident(w) => Some(w.clone()),
            Tok::Punct("{") => return Ok(Stmt::Block(self.block()?)),
            Tok::Punct(";") => {
                self.pos += 1;
                return Ok(Stmt::Empty);
            }
            _ => None,
        };
        // A label.
        if let (Some(w), Tok::Punct(":")) = (&word, self.peek_at(1)) {
            if !RESERVED.contains(&w.as_str()) {
                self.pos += 2;
                let body = self.statement()?;
                return Ok(Stmt::Labeled(w.as_str().into(), Box::new(body)));
            }
        }
        match word.as_deref() {
            Some("function") => {
                self.pos += 1;
                let f = self.function_rest(true)?;
                Ok(Stmt::Function(f))
            }
            Some("async") if matches!(self.peek_at(1), Tok::Ident(w) if w == "function") => {
                self.pos += 2;
                Ok(Stmt::Function(self.function_rest(true)?))
            }
            Some("return") => {
                self.pos += 1;
                let value = if self.is(";")
                    || self.is("}")
                    || self.at_eof()
                    || self.toks[self.pos].newline_before
                {
                    None
                } else {
                    Some(self.expression()?)
                };
                self.semicolon()?;
                Ok(Stmt::Return(value))
            }
            Some("if") => {
                self.pos += 1;
                self.expect("(")?;
                let test = self.expression()?;
                self.expect(")")?;
                let then = self.statement()?;
                let otherwise = if self.eat_word("else") {
                    Some(Box::new(self.statement()?))
                } else {
                    None
                };
                Ok(Stmt::If(test, Box::new(then), otherwise))
            }
            Some("while") => {
                self.pos += 1;
                self.expect("(")?;
                let test = self.expression()?;
                self.expect(")")?;
                Ok(Stmt::While(test, Box::new(self.statement()?)))
            }
            Some("do") => {
                self.pos += 1;
                let body = self.statement()?;
                if !self.eat_word("while") {
                    return self.error("expected `while`");
                }
                self.expect("(")?;
                let test = self.expression()?;
                self.expect(")")?;
                self.eat(";");
                Ok(Stmt::DoWhile(Box::new(body), test))
            }
            Some("for") => self.for_statement(),
            Some("break" | "continue") => {
                self.pos += 1;
                let label = match self.peek() {
                    Tok::Ident(l) if !self.toks[self.pos].newline_before => {
                        let l: Name = l.as_str().into();
                        self.pos += 1;
                        Some(l)
                    }
                    _ => None,
                };
                self.semicolon()?;
                Ok(if word.as_deref() == Some("break") {
                    Stmt::Break(label)
                } else {
                    Stmt::Continue(label)
                })
            }
            Some("throw") => {
                self.pos += 1;
                let e = self.expression()?;
                self.semicolon()?;
                Ok(Stmt::Throw(e))
            }
            Some("try") => {
                self.pos += 1;
                let body = self.block()?;
                let handler = if self.eat_word("catch") {
                    let param = if self.eat("(") {
                        let p = self.pattern()?;
                        self.expect(")")?;
                        Some(p)
                    } else {
                        None
                    };
                    Some((param, self.block()?))
                } else {
                    None
                };
                let finalizer = if self.eat_word("finally") {
                    Some(self.block()?)
                } else {
                    None
                };
                if handler.is_none() && finalizer.is_none() {
                    return self.error("expected `catch` or `finally`");
                }
                Ok(Stmt::Try(body, handler, finalizer))
            }
            Some("switch") => {
                self.pos += 1;
                self.expect("(")?;
                let subject = self.expression()?;
                self.expect(")")?;
                self.expect("{")?;
                let mut cases = Vec::new();
                while !self.eat("}") {
                    let test = if self.eat_word("case") {
                        Some(self.expression()?)
                    } else if self.eat_word("default") {
                        None
                    } else {
                        return self.error("expected `case`");
                    };
                    self.expect(":")?;
                    let mut body = Vec::new();
                    while !self.is("}") && !self.is_word("case") && !self.is_word("default") {
                        if self.at_eof() {
                            return self.error("expected `}`");
                        }
                        body.push(self.statement()?);
                    }
                    cases.push((test, body));
                }
                Ok(Stmt::Switch(subject, cases))
            }
            Some("class" | "import" | "export") => self.error("unsupported syntax"),
            _ => {
                let e = self.expression()?;
                self.semicolon()?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn declarations(&mut self) -> Result<Vec<(Pattern, Option<Expr>)>, String> {
        let mut decls = Vec::new();
        loop {
            let pattern = self.pattern()?;
            let init = if self.eat("=") {
                Some(self.assignment()?)
            } else {
                None
            };
            decls.push((pattern, init));
            if !self.eat(",") {
                return Ok(decls);
            }
        }
    }

    fn for_statement(&mut self) -> Result<Stmt, String> {
        self.pos += 1;
        self.eat_word("await");
        self.expect("(")?;
        let mut init = None;
        if let Some(kind) = self.var_kind() {
            self.pos += 1;
            let pattern = self.pattern()?;
            if self.is_word("of") || self.is_word("in") {
                let of = self.next() == Tok::Ident("of".into());
                let object = self.expression()?;
                self.expect(")")?;
                let body = self.statement()?;
                return Ok(Stmt::ForIn(Some(kind), pattern, object, Box::new(body), of));
            }
            let first_init = if self.eat("=") {
                self.no_in = true;
                let e = self.assignment();
                self.no_in = false;
                Some(e?)
            } else {
                None
            };
            let mut decls = vec![(pattern, first_init)];
            while self.eat(",") {
                let p = self.pattern()?;
                let e = if self.eat("=") {
                    self.no_in = true;
                    let e = self.assignment();
                    self.no_in = false;
                    Some(e?)
                } else {
                    None
                };
                decls.push((p, e));
            }
            init = Some(Box::new(Stmt::Var(kind, decls)));
        } else if !self.is(";") {
            self.no_in = true;
            let e = self.expression();
            self.no_in = false;
            let e = e?;
            if self.is_word("of") || self.is_word("in") {
                let of = self.next() == Tok::Ident("of".into());
                let target = match e {
                    Expr::Ident(n) => Pattern::Name(n),
                    _ => return self.error("unsupported loop target"),
                };
                let object = self.expression()?;
                self.expect(")")?;
                let body = self.statement()?;
                return Ok(Stmt::ForIn(None, target, object, Box::new(body), of));
            }
            init = Some(Box::new(Stmt::Expr(e)));
        }
        self.expect(";")?;
        let test = if self.is(";") {
            None
        } else {
            Some(self.expression()?)
        };
        self.expect(";")?;
        let update = if self.is(")") {
            None
        } else {
            Some(self.expression()?)
        };
        self.expect(")")?;
        let body = self.statement()?;
        Ok(Stmt::For(init, test, update, Box::new(body)))
    }

    fn pattern(&mut self) -> Result<Pattern, String> {
        if self.eat("{") {
            let mut props = Vec::new();
            let mut rest = None;
            while !self.eat("}") {
                if self.eat("...") {
                    rest = Some(self.name()?);
                } else {
                    let (key, shorthand) = self.property_key()?;
                    let target = if self.eat(":") {
                        self.pattern()?
                    } else {
                        match shorthand {
                            Some(n) => Pattern::Name(n),
                            None => return self.error("expected `:`"),
                        }
                    };
                    let default = if self.eat("=") {
                        Some(self.assignment()?)
                    } else {
                        None
                    };
                    props.push((key, target, default));
                }
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
            return Ok(Pattern::Object(props, rest));
        }
        if self.eat("[") {
            let mut items = Vec::new();
            let mut rest = None;
            while !self.eat("]") {
                if self.is(",") {
                    self.pos += 1;
                    items.push(None);
                    continue;
                }
                if self.eat("...") {
                    rest = Some(self.name()?);
                } else {
                    let p = self.pattern()?;
                    let default = if self.eat("=") {
                        Some(self.assignment()?)
                    } else {
                        None
                    };
                    items.push(Some((p, default)));
                }
                if !self.eat(",") {
                    self.expect("]")?;
                    break;
                }
            }
            return Ok(Pattern::Array(items, rest));
        }
        Ok(Pattern::Name(self.name()?))
    }

    /// An object key: a name (with itself for shorthand), string, number, or `[computed]`.
    fn property_key(&mut self) -> Result<(Expr, Option<Name>), String> {
        match self.next() {
            Tok::Ident(n) => {
                let name: Name = n.as_str().into();
                Ok((Expr::Str(name.clone()), Some(name)))
            }
            Tok::Str(s) => Ok((Expr::Str(s.as_str().into()), None)),
            Tok::Num(v) => Ok((Expr::Str(crate::interp::number_to_string(v).into()), None)),
            Tok::Punct("[") => {
                let e = self.assignment()?;
                self.expect("]")?;
                Ok((e, None))
            }
            _ => {
                self.pos -= 1;
                self.error("expected a property name")
            }
        }
    }

    /// After `function`: an optional name, parameters and body.
    fn function_rest(&mut self, needs_name: bool) -> Result<Arc<Func>, String> {
        if self.eat("*") {
            return self.error("generators are not supported");
        }
        let name = match self.peek() {
            Tok::Ident(_) if !self.is("(") => Some(self.name()?),
            _ if needs_name => return self.error("expected a function name"),
            _ => None,
        };
        let params = self.params()?;
        let body = self.block()?;
        Ok(Arc::new(Func {
            name,
            params,
            body,
            arrow: false,
        }))
    }

    fn params(&mut self) -> Result<Vec<Param>, String> {
        self.expect("(")?;
        let mut params = Vec::new();
        while !self.eat(")") {
            let rest = self.eat("...");
            let pattern = self.pattern()?;
            let default = if self.eat("=") {
                Some(self.assignment()?)
            } else {
                None
            };
            params.push(Param {
                pattern,
                default,
                rest,
            });
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        Ok(params)
    }

    pub fn expression(&mut self) -> Result<Expr, String> {
        let first = self.assignment()?;
        if !self.is(",") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat(",") {
            items.push(self.assignment()?);
        }
        Ok(Expr::Seq(items))
    }

    /// Whether the tokens from here are an arrow function's parameters: `x =>` or `(...) =>`.
    fn arrow_ahead(&self) -> bool {
        let mut k = 0;
        if matches!(self.peek_at(0), Tok::Ident(w) if w == "async")
            && !self.toks[(self.pos + 1).min(self.toks.len() - 1)].newline_before
            && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("("))
        {
            k = 1;
        }
        match self.peek_at(k) {
            Tok::Ident(_) => matches!(self.peek_at(k + 1), Tok::Punct("=>")),
            Tok::Punct("(") => {
                let mut depth = 0;
                let mut j = k;
                loop {
                    match self.peek_at(j) {
                        Tok::Punct("(" | "[" | "{") => depth += 1,
                        Tok::Punct(")" | "]" | "}") => {
                            depth -= 1;
                            if depth == 0 {
                                return matches!(self.peek_at(j + 1), Tok::Punct("=>"));
                            }
                        }
                        Tok::Eof => return false,
                        _ => {}
                    }
                    j += 1;
                }
            }
            _ => false,
        }
    }

    fn arrow(&mut self) -> Result<Expr, String> {
        if self.is_word("async")
            && !self.is_word("=>")
            && !matches!(self.peek_at(1), Tok::Punct("=>"))
        {
            self.pos += 1;
        }
        let params = if self.is("(") {
            self.params()?
        } else {
            vec![Param {
                pattern: Pattern::Name(self.name()?),
                default: None,
                rest: false,
            }]
        };
        self.expect("=>")?;
        let body = if self.is("{") {
            self.block()?
        } else {
            let saved = self.no_in;
            self.no_in = false;
            let e = self.assignment();
            self.no_in = saved;
            vec![Stmt::Return(Some(e?))]
        };
        Ok(Expr::Function(Arc::new(Func {
            name: None,
            params,
            body,
            arrow: true,
        })))
    }

    fn assignment(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > NESTING_LIMIT {
            return self.error("expressions nest too deeply");
        }
        let r = self.assignment_inner();
        self.depth -= 1;
        r
    }

    fn assignment_inner(&mut self) -> Result<Expr, String> {
        if self.arrow_ahead() {
            return self.arrow();
        }
        let target = self.conditional()?;
        const OPS: &[&str] = &[
            "=", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", ">>>=", "&=", "|=", "^=",
            "&&=", "||=", "??=",
        ];
        if let Tok::Punct(op) = self.peek() {
            if let Some(op) = OPS.iter().find(|o| *o == op) {
                if !matches!(target, Expr::Ident(_) | Expr::Member(..)) {
                    return self.error("invalid assignment target");
                }
                self.pos += 1;
                let value = self.assignment()?;
                return Ok(Expr::Assign(op, Box::new(target), Box::new(value)));
            }
        }
        Ok(target)
    }

    fn conditional(&mut self) -> Result<Expr, String> {
        let test = self.binary(0)?;
        if !self.eat("?") {
            return Ok(test);
        }
        let saved = self.no_in;
        self.no_in = false;
        let then = self.assignment();
        self.no_in = saved;
        let then = then?;
        self.expect(":")?;
        let otherwise = self.assignment()?;
        Ok(Expr::Cond(
            Box::new(test),
            Box::new(then),
            Box::new(otherwise),
        ))
    }

    fn binary_op(&self) -> Option<(&'static str, u8)> {
        let op: &'static str = match self.peek() {
            Tok::Punct(p) => p,
            Tok::Ident(w) if w == "instanceof" => "instanceof",
            Tok::Ident(w) if w == "in" && !self.no_in => "in",
            _ => return None,
        };
        let prec = match op {
            "??" => 1,
            "||" => 2,
            "&&" => 3,
            "|" => 4,
            "^" => 5,
            "&" => 6,
            "==" | "!=" | "===" | "!==" => 7,
            "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 8,
            "<<" | ">>" | ">>>" => 9,
            "+" | "-" => 10,
            "*" | "/" | "%" => 11,
            "**" => 12,
            _ => return None,
        };
        Some((op, prec))
    }

    fn binary(&mut self, min: u8) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while let Some((op, prec)) = self.binary_op() {
            if prec <= min && !(op == "**" && prec == min && min == 12) {
                break;
            }
            self.pos += 1;
            // `**` is right-associative.
            let right = if op == "**" {
                self.binary(prec - 1)?
            } else {
                self.binary(prec)?
            };
            left = if matches!(op, "&&" | "||" | "??") {
                Expr::Logical(op, Box::new(left), Box::new(right))
            } else {
                Expr::Binary(op, Box::new(left), Box::new(right))
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        let op: Option<&'static str> = match self.peek() {
            Tok::Punct(p @ ("!" | "-" | "+" | "~")) => Some(p),
            Tok::Ident(w) if w == "typeof" => Some("typeof"),
            Tok::Ident(w) if w == "void" => Some("void"),
            Tok::Ident(w) if w == "delete" => Some("delete"),
            Tok::Ident(w) if w == "await" => Some("await"),
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            let operand = self.unary()?;
            if op == "await" {
                // No promises: an awaited value is used as it is.
                return Ok(operand);
            }
            return Ok(Expr::Unary(op, Box::new(operand)));
        }
        if let Tok::Punct(p @ ("++" | "--")) = self.peek() {
            let p: &'static str = p;
            self.pos += 1;
            let target = self.unary()?;
            return Ok(Expr::Update(p, true, Box::new(target)));
        }
        let e = self.postfix()?;
        if let Tok::Punct(p @ ("++" | "--")) = self.peek() {
            if !self.toks[self.pos].newline_before {
                let p: &'static str = p;
                self.pos += 1;
                return Ok(Expr::Update(p, false, Box::new(e)));
            }
        }
        Ok(e)
    }

    fn arguments(&mut self) -> Result<Vec<Elem>, String> {
        self.expect("(")?;
        let saved = self.no_in;
        self.no_in = false;
        let mut args = Vec::new();
        while !self.eat(")") {
            if self.eat("...") {
                args.push(Elem::Spread(self.assignment()?));
            } else {
                args.push(Elem::Item(self.assignment()?));
            }
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        self.no_in = saved;
        Ok(args)
    }

    fn member_name(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Ident(n) => Ok(Expr::Str(n.as_str().into())),
            // Private names (`#x`) read as plain properties.
            Tok::Punct("#") => self.member_name(),
            _ => {
                self.pos -= 1;
                self.error("expected a property name")
            }
        }
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = if self.eat_word("new") {
            if self.eat(".") {
                // `new.target`
                self.name()?;
                Expr::Ident("undefined".into())
            } else {
                let callee = self.new_callee()?;
                let args = if self.is("(") {
                    self.arguments()?
                } else {
                    Vec::new()
                };
                Expr::New(Box::new(callee), args)
            }
        } else {
            self.primary()?
        };
        loop {
            if self.eat(".") {
                let key = self.member_name()?;
                e = Expr::Member(Box::new(e), Box::new(key), false);
            } else if self.eat("?.") {
                if self.is("(") {
                    let args = self.arguments()?;
                    e = Expr::Call(Box::new(e), args, true);
                } else if self.eat("[") {
                    let key = self.expression()?;
                    self.expect("]")?;
                    e = Expr::Member(Box::new(e), Box::new(key), true);
                } else {
                    let key = self.member_name()?;
                    e = Expr::Member(Box::new(e), Box::new(key), true);
                }
            } else if self.is("[") {
                self.pos += 1;
                let saved = self.no_in;
                self.no_in = false;
                let key = self.expression();
                self.no_in = saved;
                let key = key?;
                self.expect("]")?;
                e = Expr::Member(Box::new(e), Box::new(key), false);
            } else if self.is("(") {
                let args = self.arguments()?;
                e = Expr::Call(Box::new(e), args, false);
            } else if let Tok::Template(raw) = self.peek().clone() {
                // A tagged template: called with the strings and values.
                self.pos += 1;
                let (quasis, exprs) = self.template_parts(&raw)?;
                let strings = Expr::Array(
                    quasis
                        .into_iter()
                        .map(|q| Elem::Item(Expr::Str(q)))
                        .collect(),
                );
                let mut args = vec![Elem::Item(strings)];
                args.extend(exprs.into_iter().map(Elem::Item));
                e = Expr::Call(Box::new(e), args, false);
            } else {
                return Ok(e);
            }
        }
    }

    /// The callee of `new`: a member expression without calls.
    fn new_callee(&mut self) -> Result<Expr, String> {
        let mut e = if self.is_word("new") {
            self.postfix()?
        } else {
            self.primary()?
        };
        loop {
            if self.eat(".") {
                let key = self.member_name()?;
                e = Expr::Member(Box::new(e), Box::new(key), false);
            } else if self.eat("[") {
                let key = self.expression()?;
                self.expect("]")?;
                e = Expr::Member(Box::new(e), Box::new(key), false);
            } else {
                return Ok(e);
            }
        }
    }

    fn template_parts(&self, raw: &str) -> Result<(Vec<Name>, Vec<Expr>), String> {
        let mut quasis = Vec::new();
        let mut exprs = Vec::new();
        let mut text = String::new();
        let mut chars = raw.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    let Some((_, e)) = chars.next() else { break };
                    match e {
                        'n' => text.push('\n'),
                        't' => text.push('\t'),
                        'r' => text.push('\r'),
                        '\n' => {}
                        'u' => {
                            let rest = &raw[i + 2..];
                            let mut it = rest.char_indices();
                            text.push(crate::lexer::unicode_escape(&mut it));
                            let used = it.next().map_or(rest.len(), |(k, _)| k);
                            for _ in 0..rest[..used].chars().count() {
                                chars.next();
                            }
                        }
                        other => text.push(other),
                    }
                }
                '$' if chars.peek().map(|c| c.1) == Some('{') => {
                    chars.next();
                    let start = i + 2;
                    let mut depth = 1;
                    let mut end = raw.len();
                    for (k, c) in chars.by_ref() {
                        match c {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = k;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    quasis.push(std::mem::take(&mut text).into());
                    let mut inner = Parser {
                        toks: tokenize(&raw[start..end])?,
                        pos: 0,
                        no_in: false,
                        depth: self.depth + 1,
                    };
                    exprs.push(inner.expression()?);
                }
                c => text.push(c),
            }
        }
        quasis.push(text.into());
        Ok((quasis, exprs))
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Num(v) => Ok(Expr::Num(v)),
            Tok::Str(s) => Ok(Expr::Str(s.as_str().into())),
            Tok::Regex(r) => Ok(Expr::Regex(r.as_str().into())),
            Tok::Template(raw) => {
                let (quasis, exprs) = self.template_parts(&raw)?;
                Ok(Expr::Template(quasis, exprs))
            }
            Tok::Punct("(") => {
                let saved = self.no_in;
                self.no_in = false;
                let e = self.expression();
                self.no_in = saved;
                let e = e?;
                self.expect(")")?;
                Ok(e)
            }
            Tok::Punct("[") => {
                let saved = self.no_in;
                self.no_in = false;
                let mut items = Vec::new();
                while !self.eat("]") {
                    if self.is(",") {
                        self.pos += 1;
                        items.push(Elem::Hole);
                        continue;
                    }
                    if self.eat("...") {
                        items.push(Elem::Spread(self.assignment()?));
                    } else {
                        items.push(Elem::Item(self.assignment()?));
                    }
                    if !self.eat(",") {
                        self.expect("]")?;
                        break;
                    }
                }
                self.no_in = saved;
                Ok(Expr::Array(items))
            }
            Tok::Punct("{") => {
                let saved = self.no_in;
                self.no_in = false;
                let mut props = Vec::new();
                while !self.eat("}") {
                    if self.eat("...") {
                        props.push(Prop::Spread(self.assignment()?));
                    } else {
                        if matches!(self.peek(), Tok::Ident(w) if w == "get" || w == "set")
                            && !matches!(self.peek_at(1), Tok::Punct("," | ":" | "(" | "}"))
                        {
                            return self.error("getters and setters are not supported");
                        }
                        if self.is_word("async")
                            && !matches!(self.peek_at(1), Tok::Punct("," | ":" | "(" | "}"))
                        {
                            self.pos += 1;
                        }
                        let (key, shorthand) = self.property_key()?;
                        let value = if self.eat(":") {
                            self.assignment()?
                        } else if self.is("(") {
                            // A method.
                            let params = self.params()?;
                            let body = self.block()?;
                            let name = match &key {
                                Expr::Str(s) => Some(s.clone()),
                                _ => None,
                            };
                            Expr::Function(Arc::new(Func {
                                name,
                                params,
                                body,
                                arrow: false,
                            }))
                        } else {
                            match shorthand {
                                Some(n) => Expr::Ident(n),
                                None => return self.error("expected `:`"),
                            }
                        };
                        props.push(Prop::Init(key, value));
                    }
                    if !self.eat(",") {
                        self.expect("}")?;
                        break;
                    }
                }
                self.no_in = saved;
                Ok(Expr::Object(props))
            }
            Tok::Ident(w) => match w.as_str() {
                "function" => Ok(Expr::Function(self.function_rest(false)?)),
                "async" if self.is_word("function") => {
                    self.pos += 1;
                    Ok(Expr::Function(self.function_rest(false)?))
                }
                "this" => Ok(Expr::This),
                "null" => Ok(Expr::Null),
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "class" | "super" | "import" => {
                    self.pos -= 1;
                    self.error("unsupported syntax")
                }
                _ if RESERVED.contains(&w.as_str()) => {
                    self.pos -= 1;
                    self.error("unexpected keyword")
                }
                _ => Ok(Expr::Ident(w.as_str().into())),
            },
            _ => {
                self.pos -= 1;
                self.error("unexpected token")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_script_shapes() {
        let src = r#"
            var a = 1, b = [1, , 3], c = {x: 1, 'y': 2, [k]: 3, ...rest, m() { return 1 }};
            const {x, y: [z = 2]} = c
            let f = (p, q = 2, ...r) => p + q, g = async x => x * 2;
            function h(n) { if (n < 2) return n; else return h(n - 1) + h(n - 2) }
            for (let i = 0; i < 3; i++) { if (i in b) continue }
            for (const k of [1, 2]) {}
            label: while (true) { break label }
            try { throw new Error('x') } catch (e) { e.message } finally {}
            switch (a) { case 1: a++; break; default: a-- }
            a = b?.[0] ?? c?.x?.y; s = `v=${a + 1} and ${`nested ${b}`}`;
            x = /a[/]b/g.test("a/b") ? 1 : 2 ** 3 ** 2;
            document.querySelector('#id').addEventListener('click', function (ev) { ev.preventDefault() })
        "#;
        let program = parse(src).expect("parses");
        assert_eq!(program.len(), 13);
    }

    #[test]
    fn reports_what_it_does_not_support() {
        assert!(parse("class A {}").is_err());
        assert!(parse("var o = { get x() { return 1 } }").is_err());
        assert!(parse("function* g() {}").is_err());
        assert!(parse("var = 3").is_err());
        assert!(
            parse("a = 1 b = 2").is_err(),
            "no line break, no implied semicolon"
        );
        // Deep nesting is refused before it can exhaust a script thread's stack.
        std::thread::Builder::new()
            .stack_size(crate::STACK_SIZE)
            .spawn(
                || assert!(parse(&format!("{}1{}", "(".repeat(5000), ")".repeat(5000))).is_err()),
            )
            .expect("spawn")
            .join()
            .expect("refused");
    }
}
