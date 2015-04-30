// The MIT License (MIT)
//
// Copyright (c) 2015 Johan Johansson
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.	

use lex::Token;

#[derive(Debug, Clone)]
enum Type {
	Basic(String),
	Construct(String, Vec<Type>),
}
impl Type {
	/// Parse a type from tokens. On success, return parsed type and number of tokens used
	fn parse(tokens: &[Token]) -> Result<(Type, usize), String> {
		if tokens.len() == 0 {
			return Err("Type::parse: no tokens".into())
		}
		match &tokens[0] {
			&Token::Ident(s) => Ok((Type::Basic(s.into()), 1)),
			&Token::LT if tokens.len() > 2 => if let Token::Ident(ident) = tokens[1] {
				find_closing_delim(Token::LT, &tokens[2..])
					.map(|delim_i| delim_i + 2)
					.ok_or("Type::parse: failed to find closing angle bracket".into())
					.and_then(|delim_i| parse_types(&tokens[2..delim_i])
						.map(|tys| (Type::Construct(ident.into(), tys), delim_i + 1)))
			} else {
				Err(format!(
					"Type::parse: first token in angle brackets was not type ident: `{:?}`",
					tokens[1]
				))
			},
			_ => Err(format!("Type::parse: no valid type siganture in tokens: {:?}", tokens))
		}
	}
}

fn parse_types(tokens: &[Token]) -> Result<Vec<Type>, String> {
	let mut tys = Vec::new();

	let mut i = 0;
	while i < tokens.len() {
		match Type::parse(&tokens[i..]) {
			Ok((ty, len)) => {
				tys.push(ty);
				i += len;
			},
			Err(e) => return Err(e),
		}
	}

	Ok(tys)
}

#[derive(Debug, Clone)]
struct TypedBinding {
	ident: String,
	type_sig: Option<Type>,
}

fn parse_typed_bindings(tokens: &[Token]) -> Result<Vec<TypedBinding>, String> {
	let mut bindings = Vec::new();

	let mut i = 0;
	while let Some(token) = tokens.get(i) {
		if let &Token::Ident(ident) = token {
			let (type_sig, type_len) = if let Some(&Token::Colon) = tokens.get(i + 1) {
				Type::parse(&tokens[2..])
					.map(|(t, tl)| (Some(t), tl))
					.unwrap_or((None, 0))
			} else {
				(None, 0)
			};

			bindings.push(TypedBinding{ ident: ident.into(), type_sig: type_sig });
			i += 2 + type_len; // (ident + colon) + type_len
		} else {
			return Err(format!("parse_typed_bindings: unexpected token `{:?}`", token));
		}
	}

	Ok(bindings)
}

#[derive(Debug, Clone)]
struct Lambda {
	arg_bindings: Vec<TypedBinding>,
	body: ExprMeta
}
impl Lambda {
	fn parse(tokens: &[Token]) -> Result<Lambda, String> {
		if let Some(&Token::LParen) = tokens.get(0) {
			find_closing_delim(Token::LParen, &tokens[1..])
				.map(|delim_i| delim_i + 1)
				.ok_or("Lambda::parse: failed to find closing paren".into())
				.and_then(|delim_i| parse_typed_bindings(&tokens[1..delim_i])
					.map(|binds| (binds, delim_i + 1)))
				.and_then(|(binds, body_i)| parse_expr(&tokens[body_i..])
					.map(|(body, _)| Lambda{ arg_bindings: binds, body: body }))
		} else {
			Err(format!("Lambda::parse: expected parenthesized bindings, found `{:?}`",
				tokens.get(0)
			))
		}
	}
}

#[derive(Debug, Clone)]
struct SExpr {
	func: ExprMeta,
	args: Vec<ExprMeta>,
}
impl SExpr {
	fn parse(tokens: &[Token]) -> Result<SExpr, String> {
		parse_exprs(tokens).map(|exprs| SExpr{ func: exprs[0].clone(), args: exprs[1..].to_vec() })
	}
}

#[derive(Debug, Clone)]
struct Definition {
	binding: TypedBinding,
	arg_bindings: Vec<TypedBinding>,
	body: ExprMeta,
}
impl Definition {
	fn parse(tokens: &[Token]) -> Result<Definition, String> {
		if let Some(&Token::LParen) = tokens.get(0) {
			find_closing_delim(Token::LParen, &tokens[1..])
				.map(|delim_i| delim_i + 1)
				.ok_or("Definition::parse: failed to find closing paren".into())
				.and_then(|delim_i| parse_typed_bindings(&tokens[1..delim_i])
					.map(|binds| (binds.into_iter(), delim_i + 1)))
				.and_then(|(mut binds, body_i)| parse_expr(&tokens[body_i..])
					.and_then(|(body, _)| binds.next()
						.map(|binding| Definition{
							binding: binding,
							arg_bindings: binds.collect(),
							body: body
						})
						.ok_or(format!("Definition::parse: no function binding given"))))

		} else {
			Err(format!("Definition::parse: expected parenthesized bindings, found `{:?}`",
				tokens.get(0)
			))
		}
	}
}

#[derive(Debug, Clone)]
struct Cond {
	clauses: Vec<(ExprMeta, ExprMeta)>,
}
impl Cond {
	fn parse(tokens: &[Token]) -> Result<Cond, String> {
		let mut clauses = Vec::with_capacity(2);

		let mut i = 0;
		while let Some(token) = tokens.get(i) {
			let result = match token {
				&Token::LParen => find_closing_delim(Token::LParen, &tokens[1..])
					.map(|delim_i| delim_i + 1)
					.ok_or("Cond::parse: failed to find closing paren".into())
					.and_then(|delim_i| parse_exprs(&tokens[1..delim_i])
						.map(|expr| (expr, delim_i + 1))),
				t => Err(format!("Cond::parse: unexpected token `{:?}`", t)),
			};

			if let Ok((mut exprs, new_i)) = result {
				if exprs.len() == 2 {
					let second = exprs.pop().unwrap();
					clauses.push((exprs.pop().unwrap(), second));
				} else {
					return Err(
						format!("Cond::parse: clause is not pair of expressions: `{:?}`", exprs)
					);
				}
				i = new_i;
			} else if let Err(e) = result {
				return Err(e);
			}
		}

		Ok(Cond{ clauses: clauses })
	}
}

#[derive(Debug, Clone)]
enum Expr {
	Cond(Cond),
	SExpr(SExpr),
	NumLit(String),
	StrLit(String),
	Binding(String),
	Lambda(Lambda),
	// ArrayLit(String),
	Block(Vec<ExprMeta>),
	Definition(Definition),
	Nil,
}

/// An expression with additional attributes such as type information
#[derive(Debug, Clone)]
struct ExprMeta {
	value: Box<Expr>,
	coerce_type: Option<Type>
}
impl ExprMeta {
	fn new(value: Expr, coerce_type: Option<Type>) -> ExprMeta {
		ExprMeta{ value: Box::new(value), coerce_type: coerce_type }
	} 
}

fn find_closing_delim(open_token: Token, tokens: &[Token]) -> Option<usize> {
	let delim = match open_token {
		Token::LParen => Token::RParen,
		Token::LBracket => Token::RBracket,
		Token::LBrace => Token::RBrace,
		Token::LT => Token::GT,
		_ => return None,
	};

	let mut opens = 0u64;
	for (i, token) in tokens.iter().enumerate() {
		if *token == open_token {
			opens += 1;
		} else if *token == delim {
			if opens == 0 {
				return Some(i)
			}
			opens -= 1;
		}
	}
	None
}

/// Parse an expression from tokens within parentheses
fn parenthesized_to_expr(tokens: &[Token]) -> Result<Expr, String> {
	if tokens.len() == 0 {
		return Ok(Expr::Nil);
	}

	match tokens[0] {
		Token::Ident("cond") => Cond::parse(&tokens[1..]).map(|c| Expr::Cond(c)),
		Token::Ident("lambda") => Lambda::parse(&tokens[1..]).map(|λ| Expr::Lambda(λ)),
		Token::Ident("block") => parse_exprs(&tokens[1..]).map(|exprs| Expr::Block(exprs)),
		Token::Ident("define") => Definition::parse(&tokens[1..]).map(|def| Expr::Definition(def)),
		_ => SExpr::parse(tokens).map(|se| Expr::SExpr(se)),
	}
}

// Parse the tokens until a full expresion is found.
// Return the expr and how many tokens were used for it.
fn parse_expr(tokens: &[Token]) -> Result<(ExprMeta, usize), String> {
	if tokens.len() == 0 {
		return Err("parse_expr: no tokens".into());
	}

	let result = match tokens[0] {
		Token::LParen => find_closing_delim(Token::LParen, &tokens[1..])
			.map(|delim_i| delim_i + 1)
			.ok_or("parse_exprs: failed to find closing paren".into())
			.and_then(|delim_i| parenthesized_to_expr(&tokens[1..delim_i])
				.map(|expr| (expr, delim_i + 1))),
		Token::RParen => Err("parse_expr: unexpected right paren".into()),
		Token::LBracket => panic!("NOT YET IMPLEMENTED"),
		Token::RBracket => Err("parse_expr: unexpected right bracket".into()),
		Token::LBrace => panic!("NOT YET IMPLEMENTED"),
		Token::RBrace => Err("parse_expr: unexpected right brace".into()),
		Token::String(s) => Ok((Expr::StrLit(s.into()), 1)),
		Token::Number(n) => Ok((Expr::NumLit(n.into()), 1)),
		Token::Ident(ident) => Ok((Expr::Binding(ident.into()), 1)),
		Token::LT => Ok((Expr::Binding("<".into()), 1)),
		Token::GT => Ok((Expr::Binding(">".into()), 1)),
		Token::Eq => Ok((Expr::Binding("=".into()), 1)),
		Token::Exclamation => Ok((Expr::Binding("!".into()), 1)),
		Token::Colon => Err("parse_expr: unexpected colon".into()),
	};

	
	result.and_then(|(expr, expr_len)| {
		let (maybe_type, type_len) = match tokens.get(expr_len) {
			Some(&Token::Colon) => if let Ok((ty, tl)) = Type::parse(&tokens[expr_len + 1 ..]) {
				(Some(ty), tl)
			} else {
				return Err("parse_expr: colon not followed by a type".into());
			},
			_ => (None, 0),
		};
		let tokens_used = expr_len + if type_len != 0 { 1 + type_len } else { 0 };
		Ok((ExprMeta::new(expr, maybe_type), tokens_used))
	}) 
}

/// Parse tokens to items of some expr.
/// E.g. might be function binding and operands in a SExpr, might be clauses in a Cond
fn parse_exprs(tokens: &[Token]) -> Result<Vec<ExprMeta>, String> {
	let mut exprs = Vec::new();

	let mut i = 0;
	while i < tokens.len() {
		match parse_expr(&tokens[i..]) {
			Ok((expr, len)) => {
				exprs.push(expr);
				i += len;
			},
			Err(e) => return Err(e),
		}
	}

	Ok(exprs)
}

#[derive(Debug, Clone)]
pub struct AST {
	nodes: Vec<ExprMeta>,
}

impl AST {
	pub fn parse(tokens: &[Token]) -> Result<AST, String> {
		parse_exprs(tokens).map(|exprs| AST{ nodes: exprs })
	}
}