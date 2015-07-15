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

// TODO: Macro hygiene. Prevent shadowing and such, maybe by letting vars introduced inside the
//       macro be located inside a module generated by the macro.
//       So something like:
//           (def-macro m ()
//               (def-var a (:U64 42))
//               (set a (inc a)))
//       Would expand to:
//           (module m
//               (def-var a (:U64 42)))
//           (unsafe (set m\a (inc m\a)))

use std::collections::{ HashMap, HashSet };
use std::iter::once;

use lib::ScopeStack;
use super::lex::{ TokenTree, TokenTreeMeta, SrcPos };

type Macros<'a> = ScopeStack<&'a str, MacroRules<'a>>;

/// A pattern to be matched against a `TokenTree` as part of macro expansion.
///
/// A `MacroPattern` created as part of a macro definition is guaranteed to be valid
#[derive(Clone, Debug)]
enum MacroPattern<'a> {
	Ident(&'a str),
	List(Vec<MacroPattern<'a>>),
}
impl<'a> MacroPattern<'a> {
	/// Construct a new `MacroPattern` corresponding to a `TokenTree`
	fn new(ttm: TokenTreeMeta) -> MacroPattern {
		match ttm.tt {
			TokenTree::Ident(ident) => MacroPattern::Ident(ident),
			TokenTree::List(list) => MacroPattern::List(
				list.into_iter().map(MacroPattern::new).collect()),
			_ => src_error_panic!(ttm.pos, "Expected list or ident")
		}
	}

	/// Check whether the provided `TokenTree` matches the pattern of `self`
	fn matches(&self, arg: &TokenTree<'a>, literals: &HashSet<&'a str>) -> bool {
		match (self, arg) {
			(&MacroPattern::Ident(ref pi), &TokenTree::Ident(ref ti)) if literals.contains(pi) =>
				pi == ti,
			(&MacroPattern::Ident(_), _) => true,
			(&MacroPattern::List(ref patts), &TokenTree::List(ref sub_args))
				if patts.len() == sub_args.len()
			=>
				patts.iter().zip(sub_args).all(|(patt, sub_arg)|
					patt.matches(&sub_arg.tt, literals)),
			_ => false,
		}
	}

	/// Bind the `TokenTree`, `arg`, to the pattern `self`
	///
	/// # Panics
	/// Panics on pattern mismatch and on invalid pattern
	fn bind(&self, arg: TokenTreeMeta<'a>, literals: &HashSet<&'a str>)
		-> HashMap<&'a str, TokenTreeMeta<'a>>
	{
		let mut map = HashMap::with_capacity(1);

		match *self {
			MacroPattern::Ident(pi) => match arg.tt {
				TokenTree::Ident(ti) if literals.contains(pi) && pi == ti => (),
				_ if literals.contains(pi) => unreachable!(),
				_ => { map.insert(pi, arg); },
			},
			MacroPattern::List(ref patts) => if let TokenTree::List(sub_args) = arg.tt {
				map.extend(patts.iter()
					.zip(sub_args)
					.flat_map(|(patt, sub_arg)| patt.bind(sub_arg, literals)))
			} else {
				src_error_panic!(arg.pos, "Pattern mismatch. Expected list")
			},
		}
		map
	}
}

/// A definition of a macro through a series of rules, which are pattern matching cases.
#[derive(Clone, Debug)]
struct MacroRules<'a> {
	literals: HashSet<&'a str>,
	rules: Vec<(MacroPattern<'a>, TokenTreeMeta<'a>)>,
}
impl<'a> MacroRules<'a> {
	/// Construct a new `MacroRules` structure from token trees representing literals and rules
	fn new<I, T>(maybe_literals: Vec<TokenTreeMeta<'a>>, maybe_rules: T) -> MacroRules<'a>
		where
			I: Iterator<Item=TokenTreeMeta<'a>>,
			T: IntoIterator<IntoIter=I, Item=TokenTreeMeta<'a>>
	{
		let literals = maybe_literals.into_iter()
			.map(|item| match item.tt {
				TokenTree::Ident(lit) => lit,
				_ => src_error_panic!(item.pos, "Expected literal identifier"),
			})
			.collect();

		let mut rules = Vec::with_capacity(1);

		for maybe_rule in maybe_rules {
			if let TokenTree::List(mut rule) = maybe_rule.tt {
				if rule.len() != 2 {
					src_error_panic!(
						maybe_rule.pos,
						format!("Expected pattern and template"))
				}

				let template = rule.pop().unwrap();
				let pattern = MacroPattern::new(rule.pop().unwrap());

				rules.push((pattern, template))
			} else {
				src_error_panic!(maybe_rule.pos, "Expected list")
			}
		}

		MacroRules{ literals: literals, rules: rules }
	}

	/// Apply a macro to some arguments.
	fn apply_to(&self, args: Vec<TokenTreeMeta<'a>>, pos: SrcPos<'a>, macros: &mut Macros<'a>)
		-> TokenTreeMeta<'a>
	{
		let args = TokenTree::List(args);
		for &(ref pattern, ref template) in &self.rules {
			if ! pattern.matches(&args, &self.literals) {
				continue;
			}

			return template.clone()
				.relocate(pos)
				.expand_macros(macros, &pattern.bind(TokenTreeMeta::new(args, pos), &self.literals))
		}

		src_error_panic!(pos, "No rule matched in macro invocation")
	}
}

impl<'a> TokenTreeMeta<'a> {
	fn substitute_syntax_vars(self, syntax_vars: &HashMap<&str, TokenTreeMeta<'a>>) -> Self {
		match self.tt {
			TokenTree::Ident(ident) => syntax_vars.get(ident).cloned().unwrap_or(self),
			TokenTree::List(list) => TokenTreeMeta::new(
				TokenTree::List(list.map_in_place(|e| e.substitute_syntax_vars(syntax_vars))),
				self.pos),
			_ => self,
		}
	}

	fn expand_macros(self,
		macros: &mut Macros<'a>,
		syntax_vars: &HashMap<&'a str, TokenTreeMeta<'a>>
	) -> TokenTreeMeta<'a> {
		match self.tt {
			TokenTree::Ident(ident) if syntax_vars.contains_key(ident) =>
				TokenTreeMeta::new(syntax_vars[ident].tt.clone(), self.pos)
					.expand_macros(macros, &HashMap::new()),
			TokenTree::List(ref l) if l.len() == 0 =>
				TokenTreeMeta::new_list(vec![], self.pos),
			TokenTree::List(mut sexpr) => match sexpr[0].tt {
				TokenTree::Ident("quote") => TokenTreeMeta::new_list(sexpr, self.pos),
				TokenTree::Ident("begin") => TokenTreeMeta::new_list(once(sexpr[0].clone())
						.chain(expand_macros_in_scope(sexpr.drain(1..), macros, syntax_vars))
						.collect(),
					self.pos),
				TokenTree::Ident(macro_name) if macros.contains_key(macro_name) => {
					let macro_rules = macros.get(macro_name).unwrap().0.clone();

					let args = sexpr.drain(1..)
						.map(|arg| arg.substitute_syntax_vars(syntax_vars))
						.collect();
					macro_rules.apply_to(args, self.pos, macros)
				},
				_ => TokenTreeMeta::new_list(sexpr.into_iter()
						.map(|arg| arg.expand_macros(macros, syntax_vars))
						.collect(),
					self.pos),
			},
			_ => self,
		}
	}
}

// Expand macros in a block (lexical scope) of token trees
fn expand_macros_in_scope<'a, I, T>(
	scope_items: T,
	macros: &mut Macros<'a>,
	syntax_vars: &HashMap<&'a str, TokenTreeMeta<'a>>
) -> Vec<TokenTreeMeta<'a>>
	where I: Iterator<Item=TokenTreeMeta<'a>>, T: IntoIterator<IntoIter=I, Item=TokenTreeMeta<'a>>
{
	let scope_items = scope_items.into_iter();

	let mut local_macros = HashMap::new();
	// Expressions in block with macro definitions filtered out
	let mut exprs = Vec::new();

	for item in scope_items {
		if let TokenTree::List(mut sexpr) = item.tt {
			if let Some(&TokenTree::Ident("def-macro")) = sexpr.first().map(|ttm| &ttm.tt) {
				let mut parts = sexpr.drain(1..);

				let name = if let Some(name_tree_meta) = parts.next() {
					match name_tree_meta.tt {
						TokenTree::Ident(name) => name,
						_ => src_error_panic!(name_tree_meta.pos, "Expected identifier")
					}
				} else {
					src_error_panic!(item.pos, "Arity mismatch. Expected 3, found 0")
				};

				let literals = if let Some(lits_tree_meta) = parts.next() {
					match lits_tree_meta.tt {
						TokenTree::List(lits) => lits,
						_ => src_error_panic!(lits_tree_meta.pos, "Expected list")
					}
				} else {
					src_error_panic!(item.pos, "Arity mismatch. Expected 3, found 1")
				};

				if local_macros.insert(name, MacroRules::new(literals, parts)).is_some() {
					src_error_panic!(item.pos, format!("Duplicate definition of macro `{}`", name))
				}
			} else {
				exprs.push(TokenTreeMeta::new_list(sexpr, item.pos))
			}
		} else {
			exprs.push(item)
		}
	}

	let mut macros = macros.push_local(&mut local_macros);

	exprs.into_iter()
		.map(|ttm| ttm.expand_macros(&mut macros, syntax_vars))
		.collect()
}

pub fn expand_macros<'a>(tts: Vec<TokenTreeMeta<'a>>) -> Vec<TokenTreeMeta<'a>> {
	expand_macros_in_scope(tts, &mut Macros::new(), &HashMap::new())
}
