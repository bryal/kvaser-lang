use lib::front::{error_exit, exit, SrcPos};
use lib::front::ast::{self, Expr, Pattern};
use lib::{map_of, set_of, ErrCode};
use llvm_sys;
use llvm_sys::prelude::*;
use llvm_sys::target::LLVMTargetDataRef;
use std::{fmt, mem};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::str::FromStr;
use super::llvm::*;
use self::CodegenErr::*;

/// Runtime errors
#[derive(PartialEq, Eq)]
enum RuntErr<'s> {
    NonExhaustPatts(SrcPos<'s>),
}

impl<'s> RuntErr<'s> {
    fn code(&self) -> ErrCode {
        fn e(n: usize) -> ErrCode {
            ErrCode {
                module: "RUNTIME",
                number: n,
            }
        }
        match *self {
            RuntErr::NonExhaustPatts(..) => e(0),
        }
    }

    fn to_string(&self) -> String {
        let code = self.code();
        match *self {
            RuntErr::NonExhaustPatts(ref pos) => pos.error_string(
                code,
                format!("Non-exhaustive patterns in match. Fell all the way through!"),
            ),
        }
    }
}

type Instantiations<'src> = BTreeSet<(Vec<ast::Type<'src>>, ast::Type<'src>)>;
type FreeVarInsts<'src> = BTreeMap<&'src str, Instantiations<'src>>;

fn type_generic_ptr(ctx: &Context) -> &Type {
    PointerType::new(Type::get::<u8>(ctx))
}

/// The type of a reference counted pointer to `contents`
///
/// `{i64, contents}*`
fn type_rc<'ctx>(ctx: &'ctx Context, contents: &'ctx Type) -> &'ctx Type {
    PointerType::new(StructType::new(
        ctx,
        &[Type::get::<u64>(ctx), contents],
        false,
    ))
}

fn free_vars_in_exprs<'a, 'src: 'a, T>(es: T) -> FreeVarInsts<'src>
where
    T: IntoIterator<Item = &'a ast::Expr<'src>>,
{
    let mut fvs = BTreeMap::new();
    for (k, v) in es.into_iter().flat_map(|e2| free_vars_in_expr(e2)) {
        fvs.entry(k).or_insert(BTreeSet::new()).extend(v)
    }
    fvs
}

/// Returns a map of the free variables in `e`, where each variable name is mapped to the
/// instantiations of the free variable in `e`
fn free_vars_in_expr<'src>(e: &ast::Expr<'src>) -> FreeVarInsts<'src> {
    use self::ast::Expr::*;
    match *e {
        Nil(_) | NumLit(_) | StrLit(_) | Bool(_) => FreeVarInsts::new(),
        Variable(ref v) => {
            map_of(
                v.ident.s,
                set_of((
                    v.typ.get_inst_args().unwrap_or(&[]).to_vec(),
                    // Apply instantiation to type to remove wrapping `App`
                    v.typ.canonicalize(),
                )),
            )
        }
        App(box ref a) => free_vars_in_exprs([&a.func, &a.arg].iter().cloned()),
        If(ref i) => free_vars_in_exprs(
            [&i.predicate, &i.consequent, &i.alternative]
                .iter()
                .cloned(),
        ),
        Lambda(box ref l) => free_vars_in_lambda(l),
        Let(box ref l) => {
            let mut es = vec![&l.body];
            for binding in l.bindings.bindings() {
                if binding.sig.is_monomorphic() {
                    es.push(&binding.val)
                } else {
                    es.extend(binding.mono_insts.values())
                }
            }
            let mut fvs = free_vars_in_exprs(es.iter().cloned());
            for b in l.bindings.bindings() {
                fvs.remove(b.ident.s);
            }
            fvs
        }
        TypeAscript(_) => panic!("free_vars_in_expr encountered TypeAscript"),
        Cons(box ref c) => free_vars_in_exprs([&c.car, &c.cdr].iter().cloned()),
        Car(box ref c) => free_vars_in_expr(&c.expr),
        Cdr(box ref c) => free_vars_in_expr(&c.expr),
        Cast(ref c) => free_vars_in_expr(&c.expr),
        New(ref n) => free_vars_in_exprs(&n.members),
        Match(ref m) => free_vars_in_match(m),
    }
}

/// Returns a map of the free variables in `e`, where each variable name is mapped to the
/// instantiations of the free variable in `e`
fn free_vars_in_lambda<'src>(lam: &ast::Lambda<'src>) -> FreeVarInsts<'src> {
    let mut free_vars = free_vars_in_expr(&lam.body);
    free_vars.remove(lam.param_ident.s);
    free_vars
}

/// Get free variables of `lam`, but filter out externs
fn free_vars_in_lambda_filter_globals<'src, 'ctx>(
    env: &Env<'src, 'ctx>,
    lam: &ast::Lambda<'src>,
) -> FreeVarInsts<'src> {
    free_vars_in_lambda(&lam)
        .into_iter()
        .filter(|&(k, _)| env.locals.contains_key(k))
        .collect::<FreeVarInsts>()
}

fn free_vars_in_match<'src>(m: &ast::Match<'src>) -> FreeVarInsts<'src> {
    let mut fvs = free_vars_in_expr(&m.expr);
    for case in &m.cases {
        let mut case_fvs = free_vars_in_expr(&case.body);
        for v in case.patt.variables() {
            case_fvs.remove(v.ident.s);
        }
        fvs.extend(case_fvs);
    }
    fvs
}

enum CodegenErr {
    NumParseErr(String),
    ICE(String),
}
impl CodegenErr {
    fn num_parse_err<T: fmt::Display>(s: T) -> CodegenErr {
        NumParseErr(format!("{}", s))
    }
}
impl fmt::Display for CodegenErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            NumParseErr(ref parse_as) => {
                write!(f, "Could not parse numeric literal as {}", parse_as)
            }
            ICE(ref s) => write!(f, "Internal compiler error: {}", s),
        }
    }
}

fn opt_set_name<'ctx>(v: &'ctx Value, name: Option<&str>) -> &'ctx Value {
    if let Some(name_) = name {
        v.set_name(name_);
    }
    v
}

/// A global / extern function
///
/// Naked function version is used for direct calls, and for externs
/// it's also used for external linkage. If function is used as a
/// value, e.g. put in a list together with arbitrary functions, we
/// have to wrap it in a closure so that it can be called in the same
/// way as any other function.
#[derive(Debug, Clone, Copy)]
struct GlobFunc<'ctx> {
    func: &'ctx Function,
    closure: &'ctx Value,
}

/// A global variable/function (includes externs)
#[derive(Debug, Clone, Copy)]
enum Global<'ctx> {
    Func(GlobFunc<'ctx>),
    Var(&'ctx GlobalVariable),
}

/// A variable in the environment
///
/// Either a global variable/function (includes externs), or
/// a local variable (includes both functions/closures and other
/// values).
enum Var<'ctx> {
    Global(Global<'ctx>),
    Local(&'ctx Value),
}

#[derive(Debug)]
struct Env<'src, 'ctx> {
    globs: BTreeMap<String, BTreeMap<Vec<ast::Type<'src>>, Global<'ctx>>>,
    locals: BTreeMap<String, Vec<BTreeMap<Vec<ast::Type<'src>>, &'ctx Value>>>,
}

impl<'src, 'ctx> Env<'src, 'ctx> {
    fn new() -> Self {
        Env {
            globs: BTreeMap::new(),
            locals: BTreeMap::new(),
        }
    }

    fn get_local(&self, s: &str, ts: &[ast::Type]) -> Option<&'ctx Value> {
        let scopes = self.locals.get(s)?;
        let insts = scopes.last()?;
        insts.get(ts).map(|&x| x)
    }

    fn get_global(&self, s: &str, ts: &[ast::Type]) -> Option<Global<'ctx>> {
        let insts = self.globs.get(s)?;
        insts.get(ts).cloned()
    }

    fn get_global_mono(&self, s: &str) -> Option<Global<'ctx>> {
        self.get_global(s, &[])
    }

    fn get(&self, s: &str, ts: &[ast::Type]) -> Option<Var<'ctx>> {
        self.get_local(s, ts)
            .map(|l| Var::Local(l))
            .or_else(|| self.get_global(s, ts).map(|g| Var::Global(g)))
    }

    fn add_global(&mut self, id: &str, var: BTreeMap<Vec<ast::Type<'src>>, Global<'ctx>>) {
        self.globs.insert(id.to_string(), var);
    }

    fn add_global_mono(&mut self, id: &str, var: Global<'ctx>) {
        self.add_global(id, map_of(vec![], var))
    }

    fn push_local(&mut self, id: &str, var: BTreeMap<Vec<ast::Type<'src>>, &'ctx Value>) {
        self.locals
            .entry(id.to_string())
            .or_insert(Vec::new())
            .push(var)
    }

    fn push_local_mono(&mut self, id: &str, var: &'ctx Value) {
        self.push_local(id, map_of(vec![], var))
    }

    fn add_global_inst(&mut self, id: &str, inst: Vec<ast::Type<'src>>, val: Global<'ctx>) {
        let insts = self.globs.entry(id.to_string()).or_insert(BTreeMap::new());
        if insts.insert(inst, val).is_some() {
            panic!("ICE: val already exists for inst")
        }
    }

    fn add_local_inst(&mut self, id: &str, inst: Vec<ast::Type<'src>>, val: &'ctx Value) {
        let v = self.locals.entry(id.to_string()).or_insert(Vec::new());
        if v.is_empty() {
            v.push(BTreeMap::new());
        }
        if v.last_mut().unwrap().insert(inst, val).is_some() {
            panic!("ICE: val already exists for inst")
        }
    }

    fn pop_local(&mut self, id: &str) -> BTreeMap<Vec<ast::Type<'src>>, &'ctx Value> {
        self.locals
            .get_mut(id)
            .expect("ICE: Var not in env")
            .pop()
            .expect("ICE: Popped empty `vars` of env")
    }
}

struct NamedTypes<'ctx, 'src> {
    nil: &'ctx Type,
    real_world: &'ctx Type,
    rc_generic: &'ctx Type,
    adts: BTreeMap<(&'src str, Vec<ast::Type<'src>>), &'ctx Type>,
    adts_inner: BTreeMap<(&'src str, Vec<ast::Type<'src>>), &'ctx Type>,
}

type MonoFuncBinding<'src, 'ast> = (&'src str, &'ast [ast::Type<'src>], &'ast ast::Lambda<'src>);
type MonoVarBinding<'src, 'ast> = (&'src str, &'ast [ast::Type<'src>], &'ast Expr<'src>);

fn separate_func_bindings_mono<'src, 'ast>(
    bindings: &[&'ast ast::Binding<'src>],
) -> (
    Vec<MonoFuncBinding<'src, 'ast>>,
    Vec<MonoVarBinding<'src, 'ast>>,
) {
    // Flatten with regards to mono insts
    let mut func_binding_insts = Vec::<MonoFuncBinding>::new();
    let mut var_binding_insts = Vec::<MonoVarBinding>::new();
    for binding in bindings {
        let mono_insts = if binding.sig.is_monomorphic() {
            vec![(binding.ident.s, &[][..], &binding.val)]
        } else {
            binding
                .mono_insts
                .iter()
                .map(|(inst_ts, val_inst)| (binding.ident.s, inst_ts.as_slice(), val_inst))
                .collect()
        };
        for (name, inst, val) in mono_insts {
            match val {
                ast::Expr::Lambda(ref lam) => func_binding_insts.push((name, inst, lam)),
                ref e => var_binding_insts.push((name, inst, e)),
            }
        }
    }
    (func_binding_insts, var_binding_insts)
}

fn is_arithm_binop(op_name: &str) -> bool {
    let arithm_binops = hashset!{ "add", "sub", "mul", "div" };
    arithm_binops.contains(op_name)
}

fn is_relational_binop(op_name: &str) -> bool {
    let relational_binops = hashset!{ "eq", "lt" };
    relational_binops.contains(op_name)
}

/// A codegenerator that visits all nodes in the AST, wherein it builds expressions
pub struct CodeGenerator<'ctx, 'src> {
    ctx: &'ctx Context,
    pub module: &'ctx Module,
    builder: &'ctx Builder,
    /// The function currently being built
    current_func: RefCell<Option<&'ctx Function>>,
    current_block: RefCell<Option<&'ctx BasicBlock>>,
    named_types: NamedTypes<'ctx, 'src>,
    adts: ast::Adts<'src>,
}

impl<'src: 'ast, 'ast, 'ctx> CodeGenerator<'ctx, 'src> {
    pub fn new(
        ctx: &'ctx Context,
        builder: &'ctx Builder,
        module: &'ctx Module,
        adts: ast::Adts<'src>,
    ) -> Self {
        let rc_generic_inner = StructType::new_named(
            ctx,
            "rc_gen_in",
            &[Type::get::<u64>(ctx), Type::get::<u8>(ctx)],
            false,
        );
        let named_types = NamedTypes {
            real_world: StructType::new_named(ctx, "RealWorld", &[], false),
            nil: StructType::new_named(ctx, "Nil", &[], false),
            rc_generic: PointerType::new(rc_generic_inner),
            adts: BTreeMap::new(),
            adts_inner: BTreeMap::new(),
        };
        CodeGenerator {
            ctx,
            module,
            builder,
            current_func: RefCell::new(None),
            current_block: RefCell::new(None),
            named_types,
            adts,
        }
    }

    fn new_nil_val(&self) -> &'ctx Value {
        Value::new_undef(self.named_types.nil)
    }

    fn new_real_world_val(&self) -> &'ctx Value {
        Value::new_undef(self.named_types.real_world)
    }

    fn target_data(&self) -> &'ctx TargetData {
        unsafe {
            let module_ref = mem::transmute::<&Module, LLVMModuleRef>(self.module);
            let target_data = mem::transmute::<LLVMTargetDataRef, &TargetData>(
                llvm_sys::target::LLVMGetModuleDataLayout(module_ref),
            );
            target_data
        }
    }

    fn size_of(&self, t: &Type) -> u64 {
        self.target_data().size_of(t)
    }

    fn ptr_size_bytes(&self) -> usize {
        self.target_data().get_pointer_size()
    }

    fn ptr_size_bits(&self) -> usize {
        self.ptr_size_bytes() * 8
    }

    fn gen_int_ptr_type(&self) -> &'ctx Type {
        match self.ptr_size_bits() {
            8 => Type::get::<i8>(self.ctx),
            16 => Type::get::<i16>(self.ctx),
            32 => Type::get::<i32>(self.ctx),
            64 => Type::get::<i64>(self.ctx),
            e => panic!("ICE: Platform has unsupported pointer size of {} bit", e),
        }
    }

    fn gen_func_type(&mut self, arg: &ast::Type<'src>, ret: &ast::Type<'src>) -> &'ctx Type {
        FunctionType::new(
            self.gen_type(ret),
            &[type_generic_ptr(self.ctx), self.gen_type(arg)],
        )
    }

    fn gen_type(&mut self, typ: &'ast ast::Type<'src>) -> &'ctx Type {
        match typ.canonicalize() {
            ast::Type::Const("Int8", _) => Type::get::<i8>(self.ctx),
            ast::Type::Const("Int16", _) => Type::get::<i16>(self.ctx),
            ast::Type::Const("Int32", _) => Type::get::<i32>(self.ctx),
            ast::Type::Const("Int64", _) => Type::get::<i64>(self.ctx),
            ast::Type::Const("IntPtr", _) | ast::Type::Const("UIntPtr", _) => {
                self.gen_int_ptr_type()
            }
            ast::Type::Const("UInt8", _) => Type::get::<u8>(self.ctx),
            ast::Type::Const("UInt16", _) => Type::get::<u16>(self.ctx),
            ast::Type::Const("UInt32", _) => Type::get::<u32>(self.ctx),
            ast::Type::Const("UInt64", _) => Type::get::<u64>(self.ctx),
            ast::Type::Const("Bool", _) => Type::get::<bool>(self.ctx),
            ast::Type::Const("Float32", _) => Type::get::<f32>(self.ctx),
            ast::Type::Const("Float64", _) => Type::get::<f64>(self.ctx),
            ast::Type::Const("Nil", _) => self.named_types.nil,
            ast::Type::Const("RealWorld", _) => self.named_types.real_world,
            // It's not a builtin type, which means it has to be a user-defined
            // algebraic data type, unless bug in typechecker.
            ast::Type::Const(name, _) if self.adts.defs.contains_key(name) => {
                self.get_or_gen_adt_by_name_and_inst(name, &[])
            }
            ast::Type::App(box ast::TypeFunc::Const(s), ref ts) => match s {
                "->" => {
                    let fp = PointerType::new(self.gen_func_type(&ts[0], &ts[1]));
                    let captures = self.named_types.rc_generic;
                    StructType::new(self.ctx, &[fp, captures], false)
                }
                "Cons" => StructType::new(
                    self.ctx,
                    &[self.gen_type(&ts[0]), self.gen_type(&ts[1])],
                    false,
                ),
                "Ptr" => PointerType::new(self.gen_type(&ts[0])),
                // It's not a builtin type function, which means it
                // has to be a user-defined algebraic data type,
                // unless bug in typechecker.
                name if self.adts.defs.contains_key(name) => {
                    self.get_or_gen_adt_by_name_and_inst(name, ts)
                }
                _ => panic!(
                    "ICE: Type function `{}` is not implemented for codegen in gen_type",
                    s
                ),
            },
            _ => panic!(
                "ICE: Type `{}` is not implemented for codegen in gen_type",
                typ
            ),
        }
    }

    /// Generate the LLVM Type of an algebraic data type
    ///
    /// ADTs are equivalent to tagged unions, and are represented as a pair of a 16 bit tag,
    /// and the type of the largest variant.
    fn gen_adt(&mut self, adt: &ast::AdtDef<'src>, inst: &[ast::Type<'src>]) -> &'ctx Type {
        let tag_type = Type::get::<i16>(self.ctx);
        let largest_type = self.gen_largest_adt_variant_type(adt, inst);
        StructType::new_named(self.ctx, adt.name.s, &[tag_type, largest_type], false)
    }

    /// When ADT is recursive, it needs to be kept behind pointer
    /// as to not become infinitely large
    fn populate_recursive_adt(
        &mut self,
        adt: &ast::AdtDef<'src>,
        inst: &[ast::Type<'src>],
        inner_struct_type: &'ctx StructType,
    ) {
        let tag_type = Type::get::<i16>(self.ctx);
        let variant_types = adt.variants
            .iter()
            .map(|v| {
                let t = self.adts
                    .type_with_inst_of_variant(v, inst)
                    .expect("ICE: get_variant_type_with_inst failed in populate_recursive_adt");
                self.gen_type(&t)
            })
            .collect::<Vec<_>>();
        let largest_type = variant_types
            .into_iter()
            .max_by_key(|t| self.size_of(t))
            .unwrap_or(self.named_types.nil);
        inner_struct_type
            .set_elements(&[tag_type, largest_type], false)
            .expect("ICE: non-opaque struct in populate_recursive_adt");
    }

    fn get_or_gen_adt_by_name_and_inst(
        &mut self,
        name: &'src str,
        inst: &[ast::Type<'src>],
    ) -> &'ctx Type {
        if let Some(t) = self.named_types.adts.get(&(name, inst.to_vec())).cloned() {
            t
        } else {
            let adt = self.adts
                .defs
                .get(name)
                .unwrap_or_else(|| {
                    panic!(
                        "ICE: No adt of name `{}` in get_or_gen_adt_by_name_and_inst",
                        name
                    )
                })
                .clone();
            if self.adts.adt_is_recursive(&adt) {
                let inner = StructType::new_opaque(self.ctx, &format!("{}_in", name));
                self.named_types
                    .adts_inner
                    .insert((name, inst.to_vec()), inner);
                let rc = type_rc(self.ctx, inner);
                self.named_types.adts.insert((name, inst.to_vec()), rc);
                self.populate_recursive_adt(&adt, inst, inner);
            } else {
                let t = self.gen_adt(&adt, inst);
                self.named_types.adts.insert((name, inst.to_vec()), t);
            }
            self.get_or_gen_adt_by_name_and_inst(name, inst)
        }
    }

    /// Returns the type of the free variables when captured
    fn captures_type_of_free_vars(&mut self, free_vars: &FreeVarInsts<'src>) -> &'ctx Type {
        let mut captures_types = Vec::new();
        for (_, insts) in free_vars {
            for &(_, ref typ) in insts {
                captures_types.push(self.gen_type(typ));
            }
        }
        StructType::new(self.ctx, &captures_types, false)
    }

    /// Generate an external function declaration
    fn gen_func_decl(&mut self, id: &str, typ: &ast::Type<'src>) -> &'ctx mut Function {
        assert!(
            self.current_block.borrow().is_none(),
            "ICE: External function declarations may only be generated first"
        );
        let (at, rt) = typ.get_func()
            .unwrap_or_else(|| panic!("ICE: Invalid function type `{}`", typ));
        let (arg_type, ret_type) = (self.gen_type(at), self.gen_type(rt));
        let func_type = FunctionType::new(ret_type, &[arg_type]);
        self.module.add_function(id, func_type)
    }

    /// Generate a dummy closure value that wraps a call to a plain function
    fn gen_wrapping_closure(
        &mut self,
        func: &'ctx Function,
        id: &str,
        func_type: &ast::Type<'src>,
    ) -> &'ctx Value {
        let (at, rt) = func_type
            .get_func()
            .unwrap_or_else(|| panic!("ICE: Invalid function type `{}`", func_type));
        let (arg_type, ret_type) = (self.gen_type(at), self.gen_type(rt));
        let clos_typ = FunctionType::new(ret_type, &[type_generic_ptr(self.ctx), arg_type]);
        let closure_func = self.module
            .add_function(&format!("closure_func_{}", id), &clos_typ);
        let entry = closure_func.append("entry");
        self.builder.position_at_end(entry);
        closure_func[0].set_name("DUMMY-CAPTURES");
        let param = &*closure_func[1];
        let r = self.builder.build_call(func, &[param]);
        self.builder.build_ret(r);
        let closure_val = Value::new_struct(
            self.ctx,
            &[closure_func, Value::new_undef(self.named_types.rc_generic)],
            false,
        );
        let closure = self.module
            .add_global_const_variable(&format!("closure_{}", id), closure_val);
        closure
    }

    /// Generate an external function declaration and matching closure-wrapping
    fn gen_extern_func(&mut self, id: &str, typ: &ast::Type<'src>) -> GlobFunc<'ctx> {
        assert!(
            self.current_block.borrow().is_none(),
            "ICE: External function declarations may only be generated first"
        );
        let func = self.gen_func_decl(id, typ);
        let closure = self.gen_wrapping_closure(func, id, typ);
        GlobFunc { func, closure }
    }

    /// Generates a simple binop function of the instruction built by `build_instr`
    fn gen_binop_func(
        &mut self,
        func_name: &str,
        typ: &ast::Type<'src>,
        build_instr: fn(&'ctx Builder, &'ctx Value, &'ctx Value) -> &'ctx Value,
    ) -> GlobFunc<'ctx> {
        let func = self.gen_func_decl(func_name, typ);
        let entry = func.append("entry");
        self.builder.position_at_end(entry);
        let a = self.builder.build_extract_value(&*func[0], 0);
        let b = self.builder.build_extract_value(&*func[0], 1);
        let r = build_instr(self.builder, a, b);
        self.builder.build_ret(r);
        let closure = self.gen_wrapping_closure(func, func_name, typ);
        GlobFunc { func, closure }
    }

    fn gen_core_funcs(&mut self, env: &mut Env<'src, 'ctx>) {
        type BinopBuilder<'ctx> = fn(&'ctx Builder, &'ctx Value, &'ctx Value) -> &'ctx Value;
        assert!(
            self.current_block.borrow().is_none(),
            "ICE: Core functions may only be generated before main"
        );

        // Generate arithmeric, relational, and logic binops, e.g. addition
        let int_types = ["Int8", "Int16", "Int32", "Int64"];
        let uint_types = ["UInt8", "UInt16", "UInt32", "UInt64"];
        let float_types = ["Float32", "Float64"];
        let arithm_binops = [
            ("add", Builder::build_add as BinopBuilder<'ctx>),
            ("sub", Builder::build_sub),
            ("mul", Builder::build_mul),
        ];
        let int_arithm_binops = [("div", Builder::build_sdiv as BinopBuilder<'ctx>)];
        let uint_arithm_binops = [("div", Builder::build_udiv as BinopBuilder<'ctx>)];
        let float_arithm_binops = [("div", Builder::build_fdiv as BinopBuilder<'ctx>)];
        let relational_binops = [
            ("eq", Builder::build_eq as BinopBuilder<'ctx>),
            ("lt", Builder::build_lt),
        ];
        let num_classes_with_extras = [
            (&int_types[..], &int_arithm_binops[..]),
            (&uint_types[..], &uint_arithm_binops[..]),
            (&float_types[..], &float_arithm_binops[..]),
        ];
        for &(num_class, extras) in &num_classes_with_extras {
            for type_name in num_class {
                let arithms_with_div = arithm_binops
                    .iter()
                    .chain(extras)
                    .cloned()
                    .collect::<Vec<_>>();
                let typ = ast::Type::Const(type_name, None);
                let binop_type = ast::Type::new_binop(typ.clone());
                let relational_binop_type = ast::Type::new_relational_binop(typ);
                let ops_with_type = [
                    (&arithms_with_div[..], binop_type),
                    (&relational_binops[..], relational_binop_type),
                ];
                for &(ops, ref op_type) in &ops_with_type {
                    for &(op_name, build_op) in ops {
                        let func_name = format!("{}-{}", op_name, type_name);
                        let func = self.gen_binop_func(&func_name, op_type, build_op);
                        env.add_global_mono(&func_name, Global::Func(func))
                    }
                }
            }
        }
    }

    fn gen_extern_decls(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        externs: &BTreeMap<&'src str, ast::ExternDecl<'src>>,
    ) {
        for (id, decl) in externs.iter() {
            // TODO: External non-function variable declarations?
            if decl.typ.get_func().is_none() {
                decl.pos
                    .error_exit("Non-function externs not yet implemented!")
            }
            let func = self.gen_extern_func(id, &decl.typ);
            env.add_global_mono(id, Global::Func(func))
        }
        // Heap allocation is required for core-functionality. Unless user has already
        // explicitly declared the allocator, add it.
        if !externs.contains_key("malloc") {
            let malloc_type = ast::Type::new_func(
                ast::Type::Const("UIntPtr", None),
                ast::Type::new_ptr(ast::Type::Const("UInt8", None)),
            );
            let malloc_func = self.gen_extern_func("malloc", &malloc_type);
            env.add_global_mono("malloc", Global::Func(malloc_func));
        }
    }

    fn parse_gen_lit<I>(&self, lit: &str, typ: &ast::Type<'src>, pos: &SrcPos<'src>) -> &'ctx Value
    where
        I: Compile<'ctx> + FromStr,
    {
        lit.parse::<I>()
            .map(|n| n.compile(self.ctx))
            .unwrap_or_else(|_| pos.error_exit(CodegenErr::num_parse_err(typ)))
    }

    fn gen_num(&mut self, num: &ast::NumLit) -> &'ctx Value {
        let parser = match num.typ {
            ast::Type::Const("Int8", _) => CodeGenerator::parse_gen_lit::<i8>,
            ast::Type::Const("Int16", _) => CodeGenerator::parse_gen_lit::<i16>,
            ast::Type::Const("Int32", _) => CodeGenerator::parse_gen_lit::<i32>,
            ast::Type::Const("Int64", _) => CodeGenerator::parse_gen_lit::<i64>,
            ast::Type::Const("IntPtr", _) => CodeGenerator::parse_gen_lit::<isize>,
            ast::Type::Const("UInt8", _) => CodeGenerator::parse_gen_lit::<u8>,
            ast::Type::Const("UInt16", _) => CodeGenerator::parse_gen_lit::<u16>,
            ast::Type::Const("UInt32", _) => CodeGenerator::parse_gen_lit::<u32>,
            ast::Type::Const("UInt64", _) => CodeGenerator::parse_gen_lit::<u64>,
            ast::Type::Const("UIntPtr", _) => CodeGenerator::parse_gen_lit::<usize>,
            ast::Type::Const("Bool", _) => CodeGenerator::parse_gen_lit::<bool>,
            ast::Type::Const("Float32", _) => CodeGenerator::parse_gen_lit::<f32>,
            ast::Type::Const("Float64", _) => CodeGenerator::parse_gen_lit::<f64>,
            _ => num.pos
                .error_exit(ICE("type of numeric literal is not numeric".into())),
        };
        parser(self, &num.lit, &num.typ, &num.pos)
    }

    fn gen_str_(&self, env: &mut Env<'src, 'ctx>, s: &str) -> &'ctx Value {
        let str_lit_ll = Value::new_string(self.ctx, s, true);
        let str_const = self.module.add_global_const_variable("str_lit", str_lit_ll);
        let str_ptr = self.builder.build_gep(
            str_const,
            &[0usize.compile(self.ctx), 0usize.compile(self.ctx)],
        );
        let s = self.build_struct(&[s.len().compile(self.ctx), str_ptr]);
        s.set_name("str-lit");
        let r = match env.get_global_mono("str_lit_to_string") {
            Some(Global::Func(glob)) => self.builder.build_call(glob.func, &[s]),
            _ => panic!("ICE: No global function str_lit_to_string found"),
        };
        r.set_name("str");
        r
    }

    fn gen_str(&self, env: &mut Env<'src, 'ctx>, lit: &'ast ast::StrLit<'src>) -> &'ctx Value {
        self.gen_str_(env, &lit.lit)
    }

    /// Generate IR for a variable used as an r-value
    fn gen_variable(&mut self, env: &mut Env<'src, 'ctx>, var: &'ast ast::Variable) -> &'ctx Value {
        let inst = var.typ.get_inst_args().unwrap_or(&[]);
        let type_canon = var.typ.canonicalize();
        match env.get(var.ident.s, inst) {
            // NOTE: Ugly hack to fix generic codegen for some binops
            Some(Var::Global(_)) if is_arithm_binop(var.ident.s) => {
                let maybe_op_typ = type_canon.get_cons_binop().map(|t| t.var_to_int64());
                let op_typ = maybe_op_typ
                    .unwrap_or_else(|| panic!("ICE: binop has bad type {}", type_canon));
                assert!(
                    op_typ.is_int() || op_typ.is_uint() || op_typ.is_float(),
                    "ICE: binop has bad type {}",
                    type_canon
                );
                let typ = ast::Type::new_binop(op_typ.clone());
                let f = format!("{}-{}", var.ident.s, op_typ.get_const().unwrap());
                let mut var2 = var.clone();
                var2.typ = typ;
                var2.ident.s = &f;
                self.gen_variable(env, &var2)
            }
            Some(Var::Global(_)) if is_relational_binop(var.ident.s) => {
                let maybe_op_typ = type_canon
                    .get_cons_relational_binop()
                    .map(|t| t.var_to_int64());
                let op_typ = maybe_op_typ.unwrap_or_else(|| {
                    panic!("ICE: binary relational op has bad type {}", type_canon)
                });
                assert!(
                    op_typ.is_int() || op_typ.is_uint() || op_typ.is_float(),
                    "ICE: relational binop has bad type {}",
                    type_canon
                );
                let typ = ast::Type::new_relational_binop(op_typ.clone());
                let f = format!("{}-{}", var.ident.s, op_typ.get_const().unwrap());
                let mut var2 = var.clone();
                var2.typ = typ;
                var2.ident.s = &f;
                self.gen_variable(env, &var2)
            }
            Some(Var::Global(Global::Func(glob))) => self.builder.build_load(glob.closure),
            Some(Var::Global(Global::Var(var))) => self.builder.build_load(var),
            Some(Var::Local(val)) => val,
            // Undefined variables are caught during type check/inference
            None => panic!(
                "ICE: Undefined variable at codegen: `{}`, inst `{:?}`\nglobals: {:?}\nlocals: {:?}",
                var.ident.s, inst, env.globs.get(var.ident.s), env.locals.get(var.ident.s).and_then(|l| l.last())
            ),
        }
    }

    fn gen_if(&mut self, env: &mut Env<'src, 'ctx>, cond: &'ast ast::If<'src>) -> &'ctx Value {
        let pred = self.gen_expr(env, &cond.predicate, None);
        let parent_func = self.current_func.borrow().unwrap();
        let then_br = parent_func.append("cond_then");
        let else_br = parent_func.append("cond_else");
        let next_br = parent_func.append("cond_next");
        self.builder.build_cond_br(pred, then_br, else_br);
        let mut phi_nodes = vec![];

        self.builder.position_at_end(then_br);
        *self.current_block.borrow_mut() = Some(then_br);
        let then_val = self.gen_expr(env, &cond.consequent, None);
        let then_last_block = self.current_block.borrow().unwrap();
        phi_nodes.push((then_val, then_last_block));
        self.builder.build_br(next_br);

        self.builder.position_at_end(else_br);
        *self.current_block.borrow_mut() = Some(else_br);
        let else_val = self.gen_expr(env, &cond.alternative, None);
        let else_last_block = self.current_block.borrow().unwrap();
        phi_nodes.push((else_val, else_last_block));
        self.builder.build_br(next_br);

        self.builder.position_at_end(next_br);
        *self.current_block.borrow_mut() = Some(next_br);
        self.builder.build_phi(then_val.get_type(), &phi_nodes)
    }

    /// Build the application of a function to an argument
    fn build_app(&self, closure: &'ctx Value, arg: &'ctx Value) -> &'ctx Value {
        let func_val = self.builder.build_extract_value(closure, 0);
        func_val.set_name("func");
        let captures_rc = self.builder.build_extract_value(closure, 1);
        captures_rc.set_name("capts-rc");
        let captures_ptr = self.build_gep_rc_contents_generic(captures_rc);
        captures_ptr.set_name("capts-ptr");
        let func = Function::from_super(func_val).expect("ICE: Failed to cast func to &Function");
        self.builder.build_call(func, &[captures_ptr, arg])
    }

    // TODO: Tail call optimization
    /// Generates IR code for a function application.
    fn gen_app(&mut self, env: &mut Env<'src, 'ctx>, app: &'ast ast::App<'src>) -> &'ctx Value {
        let typ = app.func.get_type();
        let inst = typ.get_inst_args().unwrap_or(&[]);
        let arg = self.gen_expr(env, &app.arg, Some("app-arg"));
        // If it's a direct application of an global function: call it
        // as a function, otherwise treat it normally (call it as a closure)
        let maybe_glob = app.func
            .as_var()
            .and_then(|v| env.get(v.ident.s, inst).map(|x| (v.ident.s, x)));
        if let Some((name, Var::Global(Global::Func(g)))) = maybe_glob {
            if !is_arithm_binop(name) && !is_relational_binop(name) {
                return self.builder.build_call(g.func, &[arg]);
            }
        }
        let func = self.gen_expr(env, &app.func, Some("app-func"));
        self.build_app(func, arg)
    }

    /// Build a call for the function/closure of name `name`, given the argument as a compiled value
    ///
    /// Global/external functions are called without the closure overhead.
    fn build_call_named(
        &self,
        env: &Env<'src, 'ctx>,
        name: &str,
        inst: &[ast::Type<'src>],
        arg: &'ctx Value,
    ) -> &'ctx Value {
        match env.get(name, inst) {
            Some(Var::Global(Global::Func(g))) => self.builder.build_call(g.func, &[arg]),
            Some(Var::Global(Global::Var(g))) => self.build_app(self.builder.build_load(g), arg),
            Some(Var::Local(v)) => self.build_app(v, arg),
            None => panic!("ICE: No function `{}` defined or declared", name),
        }
    }

    fn build_call_named_mono(
        &self,
        env: &Env<'src, 'ctx>,
        name: &str,
        arg: &'ctx Value,
    ) -> &'ctx Value {
        self.build_call_named(env, name, &[], arg)
    }

    /// Generate a call to the heap allocator to allocate `n` bytes of heap memory.
    ///
    /// Returns a heap pointer of generic type `i8*`, analogous to the way
    /// `malloc` in C returns a void pointer.
    ///
    /// Will call whatever function is bound to `malloc`.
    fn build_malloc(&self, env: &mut Env<'src, 'ctx>, n: usize) -> &'ctx Value {
        let ptr = self.build_call_named_mono(env, "malloc", n.compile(self.ctx));
        ptr.set_name("malloc-ptr");
        ptr
    }

    /// Generate a call to the heap allocator to allocate heap space for a value of type `typ`
    ///
    /// Returns a heap pointer of type pointer to `typ`.
    fn build_malloc_of_type(&self, env: &mut Env<'src, 'ctx>, typ: &Type) -> &'ctx Value {
        let type_size = self.size_of(typ);
        let p = self.build_malloc(env, type_size as usize);
        self.builder.build_bit_cast(p, PointerType::new(typ))
    }

    /// Put a value on the heap
    ///
    /// Returns a pointer pointing to `val` on the heap
    fn build_val_on_heap(&self, env: &mut Env<'src, 'ctx>, val: &Value) -> &'ctx Value {
        let p = self.build_malloc_of_type(env, val.get_type());
        if let Some(name) = val.get_name() {
            p.set_name(&format!("{}-heap", name));
        }
        self.builder.build_store(val, p);
        p
    }

    /// Build a sequence of instructions that create a struct of type
    /// `typ` in register and stores values in it
    fn build_struct_of_type(&self, vals: &[&'ctx Value], typ: &'ctx Type) -> &'ctx Value {
        let mut out_struct = Value::new_undef(typ);
        for (i, val) in vals.iter().enumerate() {
            out_struct = self.builder.build_insert_value(out_struct, val, i);
        }
        out_struct
    }

    /// Build a sequence of instructions that create a struct in
    /// register and stores values in it
    pub fn build_struct(&self, vals: &[&'ctx Value]) -> &'ctx Value {
        let typ = StructType::new(
            self.ctx,
            &vals.iter().map(|v| v.get_type()).collect::<Vec<_>>(),
            false,
        );
        self.build_struct_of_type(vals, typ)
    }

    /// Build a reference counting pointer with contents `val`
    ///
    /// Count starts at 1
    fn build_rc(&self, env: &mut Env<'src, 'ctx>, val: &'ctx Value) -> &'ctx Value {
        let s = self.build_struct(&[1u64.compile(self.ctx), val]);
        self.build_val_on_heap(env, s)
    }

    /// Get a generic pointer to the contents of a rc pointer
    fn build_gep_rc_contents_generic(&self, rc: &Value) -> &'ctx Value {
        let rc_generic = self.builder.build_bit_cast(
            rc,
            PointerType::new(StructType::new(
                self.ctx,
                &[Type::get::<u64>(self.ctx), Type::get::<u8>(self.ctx)],
                false,
            )),
        );
        let name = rc.get_name().unwrap_or("rc");
        rc_generic.set_name(&format!("{}-gen", name));
        self.build_gep_rc_contents(rc_generic)
    }

    /// Get a pointer to the contents of a rc pointer
    fn build_gep_rc_contents(&self, rc: &Value) -> &'ctx Value {
        self.builder
            .build_gep(rc, &[0usize.compile(self.ctx), 1u32.compile(self.ctx)])
    }

    /// Build instructions to cast a pointer value to a generic
    /// reference counted pointer `{i64, i8}*`
    pub fn build_as_generic_rc(&self, val: &Value) -> &'ctx Value {
        self.builder
            .build_bit_cast(val, self.named_types.rc_generic)
    }

    /// Given a pointer to a pair, load the first value of the pair
    pub fn build_load_car(&self, consptr: &Value) -> &'ctx Value {
        let carptr = self.builder
            .build_gep(consptr, &[0usize.compile(self.ctx), 0u32.compile(self.ctx)]);
        self.builder.build_load(carptr)
    }

    pub fn build_extract_car(&self, cons: &Value) -> &'ctx Value {
        self.builder.build_extract_value(cons, 0)
    }

    pub fn build_extract_cdr(&self, cons: &Value) -> &'ctx Value {
        self.builder.build_extract_value(cons, 1)
    }

    /// Bitcast arbitrary (i.e. potentially aggregate) value in register to other type
    pub fn build_cast(&self, val: &'ctx Value, typ: &'ctx Type) -> &'ctx Value {
        let val_type = val.get_type();
        assert!(
            self.size_of(val_type) <= self.size_of(typ),
            "ICE: Tried to `build_cast` to smaller target type. from sizeof({:?})={} to sizeof({:?})={}",
            val_type, self.size_of(val_type),
            typ, self.size_of(typ)
        );
        let target_stack = self.builder.build_alloca(typ);
        target_stack.set_name("build-cast_target-stack");
        let val_ptr_type = PointerType::new(val_type);
        let val_stack = self.builder.build_bit_cast(target_stack, val_ptr_type);
        val_stack.set_name("build-cast_val-stack");
        self.builder.build_store(val, val_stack);
        let r = self.builder.build_load(target_stack);
        r.set_name("build-cast_target");
        r
    }

    fn build_panic(&self, env: &mut Env<'src, 'ctx>, s: &str) {
        let sc = self.gen_str_(env, s);
        self.build_call_named_mono(env, "_panic", sc);
    }

    /// Generate a function declaration with closure environment argument included
    fn gen_closure_func_decl(&mut self, id: String, typ: &ast::Type<'src>) -> &'ctx mut Function {
        let (arg, ret) = typ.get_func()
            .unwrap_or_else(|| panic!("ICE: Invalid function type `{}`", typ));
        let func_typ = self.gen_func_type(arg, ret);
        self.module.add_function(&id, func_typ)
    }

    /// Generate the anonymous function of a closure
    ///
    /// A closure is represented as a structure of the environment it captures, and
    /// a function to pass this environment to, together with the argument, when the closure
    /// is applied to an argument.
    fn gen_closure_func(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        free_vars: &FreeVarInsts<'src>,
        lam: &'ast ast::Lambda<'src>,
        name: &str,
    ) -> &'ctx Function {
        let parent_name = self.current_func
            .borrow()
            .deref()
            .and_then(|f| f.get_name().map(str::to_string))
            .unwrap_or("global".to_string());
        let lambda_name = format!("lambda_{}_{}", parent_name, name);
        let func = self.gen_closure_func_decl(lambda_name, &lam.typ);
        let parent_func = mem::replace(&mut *self.current_func.borrow_mut(), Some(func));
        let entry = func.append("entry");
        let parent_block = mem::replace(&mut *self.current_block.borrow_mut(), Some(entry));
        self.builder.position_at_end(entry);

        let captures_ptr_type = PointerType::new(self.captures_type_of_free_vars(free_vars));
        let captures_ptr_generic = &*func[0];
        captures_ptr_generic.set_name("captures_generic");
        let captures_ptr = self.builder
            .build_bit_cast(captures_ptr_generic, captures_ptr_type);
        captures_ptr.set_name("captures");
        let param = &*func[1];
        param.set_name(lam.param_ident.s);

        // Create function local environment of only parameter + captures
        let mut local_env = map_of(lam.param_ident.s.to_string(), vec![map_of(vec![], param)]);

        // Extract individual free variable captures from captures pointer and add to local env
        let mut i: u32 = 0;
        for (&fv_id, fv_insts) in free_vars.iter() {
            local_env
                .entry(fv_id.to_string())
                .or_insert(vec![BTreeMap::new()]);
            for (fv_inst, _) in fv_insts.iter().cloned() {
                let fv_ptr = self.builder.build_gep(
                    captures_ptr,
                    &[0usize.compile(self.ctx), i.compile(self.ctx)],
                );
                fv_ptr.set_name(&format!("capture_{}", fv_id));
                let fv_loaded = self.builder.build_load(fv_ptr);
                fv_loaded.set_name(fv_id);
                local_env
                    .get_mut(fv_id)
                    .expect("ICE: fv_id not in local_env")
                    .last_mut()
                    .unwrap()
                    .insert(fv_inst, fv_loaded);
                i += 1;
            }
        }
        let old_locals = mem::replace(&mut env.locals, local_env);

        let e = self.gen_expr(env, &lam.body, None);
        if e.get_name().is_none() {
            e.set_name("return-val")
        }
        self.builder.build_ret(e);

        // Restore state of code generator
        env.locals = old_locals;
        *self.current_func.borrow_mut() = parent_func;
        *self.current_block.borrow_mut() = parent_block;
        self.builder
            .position_at_end(self.current_block.borrow().expect("ICE: no current_block"));

        func
    }

    /// Generate the LLVM representation of a lambda expression, but with the contents
    /// of the closure capture left as allocated, but undefined, space
    ///
    /// For use when generating bindings with recursive references
    fn gen_closure_without_captures(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        lam: &'ast ast::Lambda<'src>,
        name: &str,
    ) -> (&'ctx Value, FreeVarInsts<'src>) {
        let free_vars = free_vars_in_lambda_filter_globals(&env, &lam);
        let func_ptr = self.gen_closure_func(env, &free_vars, lam, name);
        let captures_type = self.captures_type_of_free_vars(&free_vars);
        let undef_heap_captures = self.build_malloc(env, self.size_of(captures_type) as usize);
        let undef_heap_captures_generic_rc = self.build_as_generic_rc(undef_heap_captures);
        undef_heap_captures_generic_rc.set_name(&format!("{}-undef-capts-gen", name));
        let closure = self.build_struct(&[func_ptr, undef_heap_captures_generic_rc]);
        closure.set_name(&format!("{}-clos", name));
        (closure, free_vars)
    }

    /// Capture the free variables `free_vars` of some lambda from the environment `env`
    ///
    /// Returns a LLVM structure of each captured variable
    fn gen_closure_env_capture(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        free_vars: &FreeVarInsts<'src>,
        name: &str,
    ) -> &'ctx Value {
        let mut captures_vals = Vec::new();
        for (&fv, insts) in free_vars {
            for &(ref inst, _) in insts {
                captures_vals.push(env.get_local(fv, inst).unwrap_or_else(|| {
                    panic!(
                        "ICE: Free var not found in env\n\
                         var: {}, inst: {:?}\n\
                         env: {:?}",
                        fv, inst, env
                    )
                }));
            }
        }
        let r = self.build_struct(&captures_vals);
        r.set_name(&format!("{}-capts", name));
        r
    }

    /// For a closure that was created without environment capture,
    /// capture free variables and insert into captures member of struct
    ///
    /// May supply map of the free variables of the lambda, if saved from previously,
    /// to avoid unecessary recalculations.
    fn closure_capture_env(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        closure: &Value,
        free_vars: FreeVarInsts<'src>,
        name: &str,
    ) {
        let captures = self.gen_closure_env_capture(env, &free_vars, name);
        let closure_captures_rc_generic = self.builder.build_extract_value(closure, 1);
        closure_captures_rc_generic.set_name(&format!("{}-clos-capts-rc-gen", name));
        let closure_captures_rc = self.builder.build_bit_cast(
            closure_captures_rc_generic,
            type_rc(self.ctx, captures.get_type()),
        );
        closure_captures_rc.set_name(&format!("{}-clos-capts-rc", name));
        let closure_captures_ptr = self.builder.build_gep(
            closure_captures_rc,
            &[0usize.compile(self.ctx), 1u32.compile(self.ctx)],
        );
        closure_captures_ptr.set_name(&format!("{}-clos-capts-ptr", name));
        self.builder.build_store(captures, closure_captures_ptr);
    }

    /// Generate the LLVM representation of a lambda expression
    fn gen_lambda(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        lam: &'ast ast::Lambda<'src>,
        name: &str,
    ) -> &'ctx Value {
        let free_vars = free_vars_in_lambda_filter_globals(&env, &lam);
        let func_ptr = self.gen_closure_func(env, &free_vars, lam, name);
        let captures = self.gen_closure_env_capture(env, &free_vars, name);
        let captures_rc = self.build_rc(env, captures);
        captures_rc.set_name(&format!("{}-capts-rc", name));
        let captures_rc_generic = self.builder
            .build_bit_cast(captures_rc, self.named_types.rc_generic);
        captures_rc_generic.set_name(&format!("{}-capts-rc-gen", name));
        let r = self.build_struct(&[func_ptr, captures_rc_generic]);
        r.set_name(&format!("{}-clos", name));
        r
    }

    /// Generate LLVM definitions for the variable/function bindings `bs`
    ///
    /// Assumes that the variable bindings in `bs` are in reverse
    /// topologically order for the relation: "depends on".
    fn gen_bindings(&mut self, env: &mut Env<'src, 'ctx>, bindings: &[&'ast ast::Binding<'src>]) {
        // To solve the problem of recursive references in closure
        // captures, e.g. two mutually recursive functions that need
        // to capture each other: First create closures where captures
        // are left as allocated, but undefined space. Second, fill in
        // captures with all closures with pointers available to refer
        // to.
        let empty_vec = vec![];
        // Flatten with regards to mono insts
        let mut bindings_insts: Vec<(_, &Vec<ast::Type>, _)> = Vec::new();
        for binding in bindings {
            env.push_local(binding.ident.s, BTreeMap::new());
            if binding.sig.is_monomorphic() {
                bindings_insts.push((binding.ident.s, &empty_vec, &binding.val));
            } else {
                for (inst_ts, val_inst) in &binding.mono_insts {
                    bindings_insts.push((binding.ident.s, &inst_ts, val_inst));
                }
            }
        }
        let mut lambdas_free_vars = VecDeque::new();
        for &(name, inst, val) in &bindings_insts {
            match *val {
                ast::Expr::Lambda(ref lam) => {
                    let (closure, free_vars) = self.gen_closure_without_captures(env, lam, name);
                    env.add_local_inst(name, inst.clone(), closure);
                    lambdas_free_vars.push_back(free_vars);
                }
                _ => (),
            }
        }
        for &(name, inst, val) in &bindings_insts {
            match val {
                &ast::Expr::Lambda(_) => {
                    let closure = env.get_local(name, &inst)
                        .expect("ICE: variable dissapeared");
                    let free_vars = lambdas_free_vars.pop_front().unwrap();
                    self.closure_capture_env(env, closure, free_vars, name);
                }
                expr => {
                    let var = self.gen_expr(env, expr, Some(name));
                    var.set_name(name);
                    env.add_local_inst(name, inst.clone(), var);
                }
            }
        }
    }

    /// Generate LLVM IR for a `let` special form
    fn gen_let(&mut self, env: &mut Env<'src, 'ctx>, l: &'ast ast::Let<'src>) -> &'ctx Value {
        let bindings = l.bindings.bindings().rev().collect::<Vec<_>>();
        self.gen_bindings(env, &bindings);
        let v = self.gen_expr(env, &l.body, None);
        for b in bindings {
            env.pop_local(b.ident.s);
        }
        v
    }

    /// Generate LLVM IR for the construction of a `cons` pair
    fn gen_cons(&mut self, env: &mut Env<'src, 'ctx>, cons: &'ast ast::Cons<'src>) -> &'ctx Value {
        let members = [
            self.gen_expr(env, &cons.car, Some("car")),
            self.gen_expr(env, &cons.cdr, Some("cdr")),
        ];
        self.build_struct(&members)
    }

    /// Generate LLVM IR for the extraction of the first element of a `cons` pair
    fn gen_car(&mut self, env: &mut Env<'src, 'ctx>, c: &'ast ast::Car<'src>) -> &'ctx Value {
        let cons = self.gen_expr(env, &c.expr, None);
        let r = self.builder.build_extract_value(cons, 0);
        r.set_name("car");
        r
    }

    /// Generate LLVM IR for the extraction of the second element of a `cons` pair
    fn gen_cdr(&mut self, env: &mut Env<'src, 'ctx>, c: &'ast ast::Cdr<'src>) -> &'ctx Value {
        let cons = self.gen_expr(env, &c.expr, None);
        let r = self.builder.build_extract_value(cons, 1);
        r.set_name("cdr");
        r
    }

    /// Generate LLVM IR for the cast of an expression to a type
    fn gen_cast(&mut self, env: &mut Env<'src, 'ctx>, c: &'ast ast::Cast<'src>) -> &'ctx Value {
        let ptr_size = self.ptr_size_bits();
        let from_type = c.expr.get_type();
        let to_type = &c.typ;
        let to_type_ll = self.gen_type(to_type);
        let from_expr = self.gen_expr(env, &c.expr, None);
        let res = if let Some(from_size) = from_type.int_size(ptr_size) {
            // Casting from signed integer
            if let Some(to_size) = to_type.int_size(ptr_size).or(to_type.uint_size(ptr_size)) {
                // to some integer type
                if from_size < to_size {
                    Some(self.builder.build_sext(from_expr, to_type_ll))
                } else if from_size > to_size {
                    Some(self.builder.build_trunc(from_expr, to_type_ll))
                } else {
                    Some(from_expr)
                }
            } else if to_type.is_float() {
                // to some float type
                Some(self.builder.build_si_to_fp(from_expr, to_type_ll))
            } else {
                None
            }
        } else if let Some(from_size) = from_type.uint_size(ptr_size) {
            // Casting from unsigned integer
            if let Some(to_size) = to_type.int_size(ptr_size).or(to_type.uint_size(ptr_size)) {
                // to some integer type
                if from_size < to_size {
                    Some(self.builder.build_zext(from_expr, to_type_ll))
                } else if from_size > to_size {
                    Some(self.builder.build_trunc(from_expr, to_type_ll))
                } else {
                    Some(from_expr)
                }
            } else if to_type.is_float() {
                // to some float type
                Some(self.builder.build_ui_to_fp(from_expr, to_type_ll))
            } else {
                None
            }
        } else if from_type.is_float() {
            // Casting from float
            if to_type.is_float() {
                // to some float type
                Some(self.builder.build_fpcast(from_expr, to_type_ll))
            } else if to_type.is_int() {
                // to some signed integer type
                Some(self.builder.build_fp_to_si(from_expr, to_type_ll))
            } else if to_type.is_uint() {
                // to some unsigned integer type
                Some(self.builder.build_fp_to_ui(from_expr, to_type_ll))
            } else {
                None
            }
        } else {
            None
        };
        res.unwrap_or_else(|| {
            c.pos.error_exit(format!(
                "Invalid cast\nCannot cast from {} to {}",
                from_type, to_type
            ))
        })
    }

    fn build_of_variant(&mut self, val: &'ctx Value, variant: &str) -> &'ctx Value {
        let e_i = self.adts
            .variant_index(variant)
            .expect("ICE: No variant_index in gen_of_variant");
        let expected_i = (e_i as u16).compile(self.ctx);
        let found_i = if self.adts.adt_of_variant_is_recursive(variant) {
            // If ADT is recursive, it's also behind a refcount pointer
            let val_ = self.build_gep_rc_contents(val);
            val_.set_name("of-variant_inner-ptr");
            self.build_load_car(val_)
        } else {
            self.build_extract_car(val)
        };
        found_i.set_name("of-variant_found-variant-i");
        let r = self.builder.build_eq(expected_i, found_i);
        r.set_name("of-variant_is-of-expected-variant");
        r
    }

    fn build_as_variant(
        &mut self,
        wrapped: &'ctx Value,
        variant: &'src str,
        inst: &[ast::Type<'src>],
    ) -> &'ctx Value {
        let wrapped_ptr = if self.adts.adt_of_variant_is_recursive(variant) {
            // If ADT is recursive, it's also behind a refcount pointer
            self.build_gep_rc_contents(wrapped)
        } else {
            let wrapped_stack = self.builder.build_alloca(wrapped.get_type());
            self.builder.build_store(wrapped, wrapped_stack);
            wrapped_stack
        };
        wrapped_ptr.set_name("as-variant_wrapped-ptr");
        let unwrapped_ptr_of_largest_member_type = self.builder.build_gep(
            wrapped_ptr,
            &[0usize.compile(self.ctx), 1u32.compile(self.ctx)],
        );
        unwrapped_ptr_of_largest_member_type.set_name("as-variant_unwrapped-largest");
        let variant_type = self.adts
            .type_with_inst_of_variant_with_name(variant, inst)
            .expect("ICE: No type_of_variant in gen_as_variant");
        let unwrapped_type = self.gen_type(&variant_type);
        let unwrapped_ptr = self.builder.build_bit_cast(
            unwrapped_ptr_of_largest_member_type,
            PointerType::new(unwrapped_type),
        );
        unwrapped_ptr.set_name("as-variant_unwrapped-ptr");
        let r = self.builder.build_load(unwrapped_ptr);
        r.set_name("as-variant_unwrapped");
        r
    }

    fn gen_tuple(&mut self, env: &mut Env<'src, 'ctx>, es: &[Expr<'src>]) -> &'ctx Value {
        if let Some((last, init)) = es.split_last() {
            let last_val = self.gen_expr(env, last, None);
            last_val.set_name("gen-tuple_last");
            init.iter().rev().fold(last_val, |acc, e| {
                let members = [self.gen_expr(env, e, Some("gen-tuple_car")), acc];
                let r = self.build_struct(&members);
                r.set_name("gen-tuple_cons");
                r
            })
        } else {
            self.new_nil_val()
        }
    }

    fn gen_largest_adt_variant_type(
        &mut self,
        adt: &ast::AdtDef<'src>,
        inst: &[ast::Type<'src>],
    ) -> &'ctx Type {
        let variant_types = adt.variants
            .iter()
            .map(|v| {
                let t = self.adts
                    .type_with_inst_of_variant(v, inst)
                    .expect("ICE: get_variant_type_with_inst failed in populate_recursive_adt");
                self.gen_type(&t)
            })
            .collect::<Vec<_>>();
        variant_types
            .into_iter()
            .max_by_key(|t| self.size_of(t))
            .unwrap_or(self.named_types.nil)
    }

    fn gen_new(&mut self, env: &mut Env<'src, 'ctx>, n: &'ast ast::New<'src>) -> &'ctx Value {
        // { tag: i16, data: LARGEST-TYPE }
        let variant = n.constr.s;
        let adt = self.adts
            .parent_adt_of_variant(variant)
            .expect("ICE: No parent_adt_of_variant in gen_new")
            .clone();
        let i = adt.variant_index(variant)
            .expect("ICE: No variant_index in gen_new");
        let tag = (i as u16).compile(self.ctx);
        let adt_inst = n.typ.get_adt_inst_args().unwrap_or(&[]);
        let largest_type = self.gen_largest_adt_variant_type(&adt, adt_inst);
        let unwrapped = self.gen_tuple(env, &n.members);
        unwrapped.set_name("gen-new_unwrapped");
        let unwrapped_largest = self.build_cast(unwrapped, largest_type);
        unwrapped_largest.set_name("gen-new_unwrapped-larg");

        if self.adts.adt_is_recursive(&adt) {
            let adt_inner_type = self.named_types
                .adts_inner
                .get(&(adt.name.s, adt_inst.to_vec()))
                .unwrap_or_else(|| {
                    panic!("ICE: No adts_inner type in gen_new\nadt {} of inst {:?} not in adts_inner: {:?}",
                                 adt.name.s,
                                 adt_inst,
                                 self.named_types.adts_inner)
                });
            let wrapped_largest =
                self.build_struct_of_type(&[tag, unwrapped_largest], adt_inner_type);
            wrapped_largest.set_name("gen-new_wrapped-larg");
            let wrapped_largest_rc = self.build_rc(env, wrapped_largest);
            wrapped_largest_rc.set_name("gen-new_wrapped-larg-rc");
            wrapped_largest_rc
        } else {
            let adt_type = self.get_or_gen_adt_by_name_and_inst(adt.name.s, adt_inst);
            let wrapped_largest = self.build_struct_of_type(&[tag, unwrapped_largest], adt_type);
            wrapped_largest.set_name("gen-new_wrapped-larg");
            wrapped_largest
        }
    }

    fn gen_match_case_(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        bindings: &mut BTreeMap<&'src str, (&'ast SrcPos<'src>, &'ctx Value)>,
        matchee: &'ctx Value,
        matchee_adt_inst: &[ast::Type<'src>],
        patt: &'ast Pattern<'src>,
        body_type: &'ctx Type,
        next_branch: &'ctx BasicBlock,
    ) {
        match *patt {
            Pattern::Nil(_) => (),
            Pattern::NumLit(ref lit) => {
                let n = self.gen_num(lit);
                let eq = self.builder.build_eq(matchee, n);
                let parent_func = self.current_func.borrow().unwrap();
                let then_br = parent_func.append("cond_then");
                self.builder.build_cond_br(eq, then_br, next_branch);
                self.builder.position_at_end(then_br);
                *self.current_block.borrow_mut() = Some(then_br);
            }
            Pattern::StrLit(_) => unimplemented!(),
            Pattern::Variable(ref var) => {
                if var.ident.s != "_" {
                    if let Some((_, _)) = bindings.insert(var.ident.s, (&var.ident.pos, matchee)) {
                        unimplemented!("multiple occurences of identifier")
                    }
                }
            }
            Pattern::Deconstr(ref deconst) => {
                let variant = deconst.constr.s;
                let variant_member_types = self.adts
                    .members_with_inst_of_variant_with_name(variant, matchee_adt_inst)
                    .unwrap();
                let of_variant = self.build_of_variant(matchee, variant);
                let parent_func = self.current_func.borrow().unwrap();
                let then_br = parent_func.append("cond_then");
                self.builder.build_cond_br(of_variant, then_br, next_branch);
                self.builder.position_at_end(then_br);
                *self.current_block.borrow_mut() = Some(then_br);
                if let Some((last_sub, subs)) = deconst.subpatts.split_last() {
                    let (last_member_t, member_ts) = variant_member_types.split_last().unwrap();
                    let inner = self.build_as_variant(matchee, variant, matchee_adt_inst);
                    let mut remaining = inner;
                    for (sub, member_t) in subs.into_iter().zip(member_ts) {
                        let sub_matchee = self.build_extract_car(remaining);
                        let sub_matchee_adt_inst = member_t.get_adt_inst_args().unwrap_or(&[]);
                        self.gen_match_case_(
                            env,
                            bindings,
                            sub_matchee,
                            sub_matchee_adt_inst,
                            sub,
                            body_type,
                            next_branch,
                        );
                        remaining = self.build_extract_cdr(remaining);
                    }
                    let sub_matchee = remaining;
                    let sub_matchee_adt_inst = last_member_t.get_adt_inst_args().unwrap_or(&[]);
                    self.gen_match_case_(
                        env,
                        bindings,
                        sub_matchee,
                        sub_matchee_adt_inst,
                        last_sub,
                        body_type,
                        next_branch,
                    );
                }
            }
        }
    }

    fn gen_match_case(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        matchee: &'ctx Value,
        matchee_adt_inst: &[ast::Type<'src>],
        case: &ast::Case<'src>,
        next_branch: &'ctx BasicBlock,
    ) -> &'ctx Value {
        let mut patt_bindings = BTreeMap::new();
        let body_type = self.gen_type(case.body.get_type());
        self.gen_match_case_(
            env,
            &mut patt_bindings,
            matchee,
            matchee_adt_inst,
            &case.patt,
            body_type,
            next_branch,
        );
        for (var, &(_, val)) in &patt_bindings {
            env.push_local_mono(var, val);
        }
        let r = self.gen_expr(env, &case.body, Some("case_body"));
        for (var, _) in patt_bindings {
            env.pop_local(var);
        }
        r
    }

    fn gen_match(&mut self, env: &mut Env<'src, 'ctx>, m: &'ast ast::Match<'src>) -> &'ctx Value {
        let expr = self.gen_expr(env, &m.expr, Some("matchee"));
        let expr_adt_inst = m.expr.get_type().get_adt_inst_args().unwrap_or(&[]);
        let parent_func = self.current_func.borrow().unwrap();

        let case_blocks = m.cases
            .iter()
            .enumerate()
            .map(|(i, case)| (case, parent_func.append(&format!("case_{}", i))))
            .collect::<Vec<_>>();
        let default_block = parent_func.append("case_default");
        let final_block = parent_func.append("case_final");
        let mut case_phi_nodes = Vec::new();

        let first_case_block = case_blocks.get(0).expect("ICE: No cases in gen_match").1;
        self.builder.build_br(first_case_block);

        let mut it = case_blocks.iter().peekable();
        while let Some(&(case, block)) = it.next() {
            self.builder.position_at_end(block);
            *self.current_block.borrow_mut() = Some(block);
            let next_block = it.peek().map(|&&(_, b)| b).unwrap_or(default_block);
            let case_val = self.gen_match_case(env, expr, expr_adt_inst, &case, next_block);
            // The block jumped from to `final_block` on successful match.
            // I.e., the one to use in the phi node
            let case_last_block = self.current_block.borrow().unwrap();
            self.builder.build_br(final_block);
            case_phi_nodes.push((case_val, case_last_block));
        }

        // Default block. What to do when all user-defined cases fell through (runtime error)
        self.builder.position_at_end(default_block);
        *self.current_block.borrow_mut() = Some(default_block);
        self.build_panic(env, &RuntErr::NonExhaustPatts(m.pos.clone()).to_string());
        self.builder.build_br(final_block);
        let ret_type = self.gen_type(&m.typ);
        case_phi_nodes.push((Value::new_undef(ret_type), default_block));

        self.builder.position_at_end(final_block);
        *self.current_block.borrow_mut() = Some(final_block);
        self.builder
            .build_phi(self.gen_type(&m.typ), &case_phi_nodes)
    }

    /// Generate llvm code for an expression and return its llvm Value.
    fn gen_expr(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        expr: &'ast Expr<'src>,
        name: Option<&str>,
    ) -> &'ctx Value {
        match *expr {
            // Represent Nil as the empty struct, unit
            Expr::Nil(_) => self.new_nil_val(),
            Expr::NumLit(ref n) => self.gen_num(n),
            Expr::StrLit(ref s) => self.gen_str(env, s),
            Expr::Bool(ref b) => b.val.compile(self.ctx),
            Expr::Variable(ref var) => self.gen_variable(env, var),
            Expr::App(ref app) => opt_set_name(self.gen_app(env, app), name),
            Expr::If(ref cond) => opt_set_name(self.gen_if(env, cond), name),
            Expr::Lambda(ref lam) => self.gen_lambda(env, lam, name.unwrap_or("lam")),
            Expr::Let(ref l) => opt_set_name(self.gen_let(env, l), name),
            // All type ascriptions should be replaced at this stage
            Expr::TypeAscript(_) => unreachable!(),
            Expr::Cons(ref c) => opt_set_name(self.gen_cons(env, c), name),
            Expr::Car(ref c) => opt_set_name(self.gen_car(env, c), name),
            Expr::Cdr(ref c) => opt_set_name(self.gen_cdr(env, c), name),
            Expr::Cast(ref c) => opt_set_name(self.gen_cast(env, c), name),
            Expr::New(ref n) => opt_set_name(self.gen_new(env, n), name),
            Expr::Match(ref m) => opt_set_name(self.gen_match(env, m), name),
        }
    }

    fn gen_glob_var_decl(&mut self, name: &str, typ: &ast::Type<'src>) -> &'ctx GlobalVariable {
        let undef = Value::new_undef(self.gen_type(typ));
        self.module.add_global_variable(name, undef)
    }

    /// Generate uninitialized declarations for all global
    /// variables.
    ///
    /// As they may be initialized by executing arbitrary
    /// functions, they must (for now) be initialized at runtime. The
    /// initialization happens in a wrapper around `main`.
    fn gen_glob_var_decls(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        var_bindings: &[MonoVarBinding<'src, 'ast>],
    ) {
        for (name, inst, val) in var_bindings {
            let var = self.gen_glob_var_decl(name, val.get_type());
            env.add_global_inst(name, inst.to_vec(), Global::Var(var));
        }
    }

    fn gen_glob_var_inits(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        var_bindings: &[MonoVarBinding<'src, 'ast>],
    ) {
        for (name, inst, expr) in var_bindings {
            let glob = env.get_global(name, inst)
                .expect("ICE: Global variable declaration dissapeared");
            let glob_var = match glob {
                Global::Var(v) => v,
                _ => panic!("ICE: Global var to init was not a global var"),
            };
            let v = self.gen_expr(env, expr, None);
            self.builder.build_store(v, glob_var);
        }
    }

    fn gen_func_def(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        func: &'ctx Function,
        lam: &ast::Lambda<'src>,
    ) {
        let parent_func = mem::replace(&mut *self.current_func.borrow_mut(), Some(func));
        let entry = func.append("entry");
        let parent_block = mem::replace(&mut *self.current_block.borrow_mut(), Some(entry));
        self.builder.position_at_end(entry);
        let param = &*func[0];
        param.set_name(lam.param_ident.s);
        let local_env = map_of(lam.param_ident.s.to_string(), vec![map_of(vec![], param)]);
        let old_locals = mem::replace(&mut env.locals, local_env);
        let r = self.gen_expr(env, &lam.body, None);
        self.builder.build_ret(r);
        env.locals = old_locals;
        *self.current_func.borrow_mut() = parent_func;
        *self.current_block.borrow_mut() = parent_block;
        if let Some(block) = *self.current_block.borrow() {
            self.builder.position_at_end(block);
        }
    }

    /// For global functions, generates plain function definitions and
    /// matching closure-wrappers
    ///
    /// Where a "closure-wrapper" is a function wrapped around the
    /// plain function with an additional param for dummy closure
    /// environment. This is used so that global functions and local
    /// closures can be represented the same way, size-wise and all.
    fn gen_glob_funcs(
        &mut self,
        env: &mut Env<'src, 'ctx>,
        bindings: &[MonoFuncBinding<'src, 'ast>],
    ) {
        let mut funcs = Vec::new();
        for (name, inst, lam) in bindings {
            let func = self.gen_func_decl(name, &lam.typ);
            let closure = self.gen_wrapping_closure(func, name, &lam.typ);
            let glob_func = GlobFunc { func, closure };
            funcs.push(&*func);
            env.add_global_inst(name, inst.to_vec(), Global::Func(glob_func));
        }
        for ((_, _, lam), func) in bindings.into_iter().zip(funcs) {
            self.gen_func_def(env, func, lam);
        }
    }

    /// Generate LLVM IR for the executable application defined in `module`.
    ///
    /// Declare external functions, define global variabled and functions, and define
    /// an entry-point that makes the program binary executable.
    ///
    /// To allow for run-time operations, e.g. heap allocation, in "constant" global definitions,
    /// wrap the definitions in the entry-point function. This moves initialization to run-time,
    /// whish in turn allows for these operations.
    ///
    /// Basically, transform this:
    /// ```
    /// (define foo ...)
    /// (define bar ...)
    /// (define (main) ...)
    /// ```
    /// to this:
    /// ```
    /// (define (main)
    ///   (let ((foo ...)
    ///         (bar ...)
    ///         (main' ...))
    ///     (main')))
    /// ```
    /// where `main'` is the user defined `main`, and `main` is a simple, C-abi compatible function.
    pub fn gen_executable(&mut self, ast: &ast::Ast<'src>) {
        // Assert that `main` exists and is monomorphic of type `(-> Nil Nil)`
        {
            let main = ast.globals
                .bindings()
                .find(|b| b.ident.s == "main")
                .unwrap_or_else(|| error_exit("main function not found"));
            let expect = ast::Type::new_io(ast::TYPE_NIL.clone());
            if main.sig.body != expect {
                let error_msg = format!(
                    "main function has wrong type. Expected type `{}`, found type `{}`",
                    expect, main.sig
                );
                if main.sig.is_monomorphic() {
                    main.pos.error_exit(error_msg)
                } else {
                    main.pos.print_error(ErrCode::undefined(), error_msg);
                    main.pos.print_help(
                        "Try adding type annotations to enforce correct type \
                         during type-checking.\n\
                         E.g. `(define: main (-> RealWorld (Cons Nil RealWorld)) ...)`",
                    );
                    exit()
                }
            }
        }

        // Create wrapping, entry-point `main` function. Must be
        // declared before the user-defined main so that it gets the
        // correct name.
        let outer_main_type =
            FunctionType::new(Type::get::<i32>(self.ctx), &[self.named_types.nil]);
        let main_wrapper = self.module.add_function("main", &outer_main_type);

        let mut env = Env::new();

        self.gen_core_funcs(&mut env);
        self.gen_extern_decls(&mut env, &ast.externs);
        let glob_bindings = ast.globals.bindings().rev().collect::<Vec<_>>();
        for binding in &glob_bindings {
            env.globs
                .insert(binding.ident.s.to_string(), BTreeMap::new());
        }
        let (glob_func_bindings, glob_var_bindings) = separate_func_bindings_mono(&glob_bindings);
        self.gen_glob_var_decls(&mut env, &glob_var_bindings);
        self.gen_glob_funcs(&mut env, &glob_func_bindings);

        // Populate the outer, wrapping `main` with glob var
        // initialization and calling of user-defined `main`.
        let entry = main_wrapper.append("entry");
        self.builder.position_at_end(entry);
        *self.current_func.borrow_mut() = Some(main_wrapper);
        *self.current_block.borrow_mut() = Some(entry);
        self.gen_glob_var_inits(&mut env, &glob_var_bindings);
        self.build_call_named_mono(&mut env, "main", self.new_real_world_val());
        self.builder.build_ret(0i32.compile(self.ctx));
    }
}
