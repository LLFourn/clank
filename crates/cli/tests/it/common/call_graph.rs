//! A call graph of a crate's production code, for gates that must hold
//! over everything an entry point REACHES rather than over named files.
//!
//! It over-approximates: a wrong answer costs a false failure, never a
//! silent pass.
//!
//! - A path call reaches every function the path can resolve to, through
//!   `use`s, globs and re-exports.
//! - A method call reaches the methods of its receiver's type when the type
//!   follows from declarations: parameters, annotated or constructed locals,
//!   declared return types, struct fields, generic bounds. A receiver of a
//!   trait the crate does not define reaches every method the crate
//!   implements for that trait, since its provided methods call them.
//! - A method call whose receiver's type does not follow reaches the method
//!   of that name on every type a value in the function can have: one it
//!   names, one its callees return, or one either of those holds.
//! - A function named as a value is an edge from where it is named, as if
//!   called there: a callback runs no code its namer could not.
//!
//! Types are identified by their full path, so two types or traits that
//! share a name in different modules are never confused.
//!
//! What cannot be followed is reported as a [`Refusal`], never skipped.
//! That includes every macro whose arguments are not parsed as the
//! expressions of a known formatting or assertion macro, and every macro
//! the crate defines: an expansion can call anything, whatever its
//! arguments say. Calls in a refused macro's tokens are still followed.
//! Not modelled: calls the compiler inserts (`Drop`, `Deref`, `?`'s `From`,
//! formatting's `Display`), and callbacks stored in statics by code the walk
//! does not reach.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;

use super::is_cfg_test;

/// Macros whose arguments are expressions the graph parses and follows.
const EXPRESSION_MACROS: &[&str] = &[
    "format",
    "format_args",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "panic",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "unreachable",
    "todo",
    "unimplemented",
    "anyhow",
    "bail",
    "ensure",
    "vec",
    "dbg",
];

/// Macros that expand to constants and take no expressions.
const CONSTANT_MACROS: &[&str] = &[
    "concat",
    "stringify",
    "include_str",
    "include_bytes",
    "env",
    "option_env",
    "line",
    "file",
    "column",
    "module_path",
    "cfg",
];

/// Crates every crate can name without depending on them.
const BUILTIN_CRATES: &[&str] = &["std", "core", "alloc"];

/// Prelude names a path may start with.
const PRELUDE: &[&str] = &[
    "Some",
    "None",
    "Ok",
    "Err",
    "Box",
    "Vec",
    "String",
    "Option",
    "Result",
    "Default",
    "From",
    "Into",
    "Iterator",
    "ToString",
    "ToOwned",
    "Clone",
    "drop",
    "PartialEq",
    "Ord",
    "PartialOrd",
    "AsRef",
    "TryFrom",
    "TryInto",
    "char",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "f32",
    "f64",
    "bool",
    "str",
    "IntoIterator",
    "Extend",
    "FromIterator",
    "Eq",
];

/// Wrappers a method call sees through to the type they hold.
const TRANSPARENT: &[&str] = &["Box", "Rc", "Arc", "Pin", "RefCell", "Mutex", "RwLock"];

/// Methods of std types that hand back the receiver's own type.
const SAME_TYPE: &[&str] = &[
    "clone",
    "to_owned",
    "as_ref",
    "as_mut",
    "borrow",
    "borrow_mut",
];

/// Methods of `Option`/`Result` that hand back what they hold.
const UNWRAP: &[&str] = &[
    "unwrap",
    "expect",
    "unwrap_or_default",
    "unwrap_or",
    "unwrap_or_else",
];

/// Calls that deserialize a value into a type.
const DESERIALIZERS: &[&str] = &["from_value", "from_str", "from_slice", "from_reader"];

/// Methods that walk a collection's entries without naming them.
const ITERATING: &[&str] = &[
    "iter",
    "iter_mut",
    "into_iter",
    "keys",
    "values",
    "values_mut",
    "into_keys",
    "into_values",
    "drain",
];

/// Types that hold a JSON document whole.
const RAW_DOCUMENTS: &[&str] = &["Value", "Map", "Document"];

/// Which argument of a known macro is its message, by macro.
fn message_arg(name: &str) -> Option<usize> {
    match name {
        "anyhow" | "bail" | "panic" | "unreachable" | "todo" | "unimplemented" | "print"
        | "println" | "eprint" | "eprintln" => Some(0),
        "write" | "writeln" | "ensure" | "assert" | "debug_assert" => Some(1),
        "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => Some(2),
        _ => None,
    }
}

/// Keywords that end a path in unparsed macro tokens.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "else", "enum", "fn", "for", "if",
    "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "static",
    "struct", "trait", "type", "unsafe", "use", "where", "while", "dyn",
];

/// A function: free, associated, a trait's default, or a const's
/// initializer.
pub struct Node {
    pub id: String,
    pub module: String,
    pub file: PathBuf,
    /// First and last source line, 1-based and inclusive.
    pub lines: (usize, usize),
    /// String literals in the body, with their lines.
    pub literals: Vec<(usize, String)>,
    /// The full path of the type or trait the function is defined on.
    self_ty: Option<String>,
    /// The full path of the trait an impl's method implements.
    of_trait: Option<String>,
    ret: Option<Ty>,
    params: Vec<Ty>,
    sites: Vec<Site>,
    uses: Vec<Use>,
    bounds: HashMap<String, Vec<Vec<String>>>,
    env: HashMap<String, Recv>,
    /// Names the body binds or defines locally: they shadow the module's.
    locals: BTreeSet<String>,
    /// Every path the signature and body name.
    mentioned: Vec<Vec<String>>,
    reads: Reads,
}

/// How a function consumes what it deserializes: what an exception for a
/// document read is audited against.
#[derive(Default)]
struct Reads {
    /// String literals that are an error's or a log line's message.
    messages: Vec<(usize, String)>,
    /// Each deserialization, and the type it produces when the source says.
    targets: Vec<(usize, Option<Ty>)>,
    /// Where entries are walked without being named.
    iterations: Vec<usize>,
    /// Structs the function defines for itself, by name.
    local_structs: HashMap<String, Shape>,
}

/// The fields serde reads into a struct, or why that cannot be said.
#[derive(Clone)]
pub enum Shape {
    Fields(Vec<String>),
    Opaque(String),
}

/// A deserialization target: the type's name, and its shape when it is a
/// struct of this crate.
pub type Produced = (String, Option<Shape>);

/// What a function that reads a document consumes of it.
pub struct Projection {
    /// String literals that are not messages: its own, and those of the
    /// constants and argument-free leaf functions it names. The keys it
    /// can index with.
    pub keys: Vec<(usize, String)>,
    /// Each deserialization: the target type's name and shape, or `None`
    /// when the source does not say what it produces.
    pub targets: Vec<(usize, Option<Produced>)>,
    pub iterations: Vec<usize>,
    /// Calls that hand a whole document to another function of the crate.
    pub hands_on: Vec<(usize, String)>,
    pub returns_document: bool,
}

/// One edge out of a node.
pub struct Edge {
    pub line: usize,
    pub to: To,
    /// The callee was named as a value, not called.
    pub value: bool,
}

pub enum To {
    Node(usize),
    External(String),
    /// A method called by name, whatever it resolved to.
    Method(String),
}

/// A site the graph cannot follow.
pub struct Refusal {
    pub line: usize,
    pub form: String,
    pub what: String,
}

struct Site {
    line: usize,
    callee: Callee,
}

enum Callee {
    Path(Vec<String>),
    /// `<T as Trait>::name`: every associated function of that name.
    Qualified(String),
    Method(String, Recv),
    Value(Vec<String>),
    /// A macro invocation, and whether the graph parsed its arguments as
    /// the expressions a known macro takes.
    Macro {
        name: String,
        parsed: bool,
    },
}

/// A type as written.
#[derive(Clone, PartialEq, Debug)]
enum Ty {
    Path(Vec<String>, Vec<Ty>),
    Traits(Vec<Vec<String>>),
    Unknown,
}

/// How a receiver's type follows from declarations.
#[derive(Clone, PartialEq, Debug)]
enum Recv {
    SelfValue,
    Var(String),
    Declared(Ty),
    Ret(Vec<String>),
    MethodRet(Box<Recv>, String),
    Unwrapped(Box<Recv>),
    Field(Box<Recv>, String),
    Unknown,
}

/// A type resolved to full paths. An external type is `ext:` and its name.
#[derive(Clone, PartialEq, Debug)]
enum Rty {
    Named(String, Vec<Rty>),
    Traits(Vec<String>),
    Unknown,
}

#[derive(Clone)]
struct Use {
    name: String,
    target: Vec<String>,
    renamed: bool,
    glob: bool,
    line: usize,
}

#[derive(Default)]
struct Module {
    fns: BTreeSet<String>,
    foreign: BTreeSet<String>,
    types: BTreeSet<String>,
    mods: BTreeSet<String>,
    consts: BTreeSet<String>,
    uses: Vec<Use>,
}

enum Target {
    Fns(Vec<usize>),
    /// A type or trait of this crate, by full path.
    Type(String),
    External(String),
    Nothing,
    Unresolved,
}

/// An impl block, linked once every module is loaded.
struct RawImpl {
    module: String,
    self_ty: Vec<String>,
    of_trait: Option<Vec<String>>,
    generics: Vec<String>,
    fns: Vec<(usize, String, bool)>,
}

/// A type written in `module`.
type Written = (Ty, String);

#[derive(Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    externs: BTreeSet<String>,
    modules: BTreeMap<String, Module>,
    by_id: HashMap<String, Vec<usize>>,
    raw_impls: Vec<RawImpl>,
    methods: HashMap<String, Vec<usize>>,
    assoc: HashMap<(String, String), Vec<usize>>,
    assoc_by_name: HashMap<String, Vec<usize>>,
    traits: HashSet<String>,
    trait_defaults: HashMap<(String, String), Vec<usize>>,
    trait_impls: HashMap<String, Vec<usize>>,
    implements: HashMap<String, BTreeSet<String>>,
    blanket: HashMap<String, Vec<usize>>,
    fields: HashMap<(String, String), Written>,
    /// What a value of each type holds: its fields' and variants' types.
    holds: HashMap<String, Vec<Written>>,
    aliases: HashMap<String, Written>,
    types: HashSet<String>,
    /// The fields serde reads into each struct.
    shapes: HashMap<String, Shape>,
    /// Every `macro_rules!` the crate defines: its expansion can call
    /// anything, whatever its invocation's arguments say.
    macro_rules: HashSet<String>,
    sources: HashMap<PathBuf, Vec<String>>,
}

/// Where `mod x;` declared at this point is looked for.
struct Dirs {
    plain: PathBuf,
    path_attr: PathBuf,
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| match &a.meta {
        syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    })
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_cfg_test)
}

/// The dependencies `manifest` declares, by the name code uses for them.
pub fn dependencies(manifest: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(manifest).expect("the manifest reads");
    let doc: toml::Table = text.parse().expect("the manifest parses");
    let mut out = BTreeSet::new();
    let mut add = |t: Option<&toml::Value>| {
        if let Some(t) = t.and_then(toml::Value::as_table) {
            out.extend(t.keys().map(|k| k.replace('-', "_")));
        }
    };
    add(doc.get("dependencies"));
    if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
        for t in targets.values() {
            add(t.get("dependencies"));
        }
    }
    out
}

impl Graph {
    /// The production modules of the crate at `src`, following `mod`
    /// declarations from `lib.rs` as the compiler does and leaving out
    /// `#[cfg(test)]`. `externs` are the crates its paths may start with.
    pub fn load(src: &Path, externs: BTreeSet<String>) -> Result<Graph, String> {
        let mut g = Graph {
            externs,
            ..Graph::default()
        };
        g.load_file(&src.join("lib.rs"), "crate")?;
        g.link();
        Ok(g)
    }

    /// The source lines of `n`.
    pub fn lines(&self, n: usize) -> impl Iterator<Item = (usize, &str)> {
        let node = &self.nodes[n];
        let src = self
            .sources
            .get(&node.file)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let (start, end) = node.lines;
        (start..=end.min(src.len())).map(move |l| (l, src[l - 1].as_str()))
    }

    /// Whether `n` declares that it returns a path.
    pub fn returns_path(&self, n: usize) -> bool {
        matches!(&self.nodes[n].ret, Some(Ty::Path(p, _)) if p.last().is_some_and(|l| l == "PathBuf"))
    }

    /// What `n` consumes of the documents it deserializes.
    pub fn projection(&self, n: usize) -> Projection {
        let node = &self.nodes[n];
        let at = At {
            module: &node.module,
            node: Some(node),
        };
        let mut messages = node.reads.messages.clone();
        let mut keys = Vec::new();
        for lit in &node.literals {
            match messages.iter().position(|m| m == lit) {
                Some(i) => {
                    messages.swap_remove(i);
                }
                None => keys.push(lit.clone()),
            }
        }
        let (edges, _) = self.edges(n);
        let mut hands_on = Vec::new();
        for e in &edges {
            let To::Node(t) = e.to else {
                continue;
            };
            let callee = &self.nodes[t];
            if callee.ret.is_some() && callee.params.is_empty() && callee.sites.is_empty() {
                keys.extend(callee.literals.iter().map(|(_, l)| (e.line, l.clone())));
            }
            if callee.params.iter().any(holds_document) {
                hands_on.push((e.line, callee.id.clone()));
            }
        }
        let targets = node
            .reads
            .targets
            .iter()
            .map(|(line, ty)| {
                let named = ty.as_ref().and_then(|ty| {
                    let Ty::Path(segs, _) = produced(ty) else {
                        return None;
                    };
                    let name = segs.last()?.clone();
                    let shape = match segs.as_slice() {
                        [one] if node.reads.local_structs.contains_key(one) => {
                            node.reads.local_structs.get(one).cloned()
                        }
                        _ => self
                            .type_name(at.module, at.node, segs)
                            .and_then(|full| self.shapes.get(&full).cloned()),
                    };
                    Some((name, shape))
                });
                (*line, named)
            })
            .collect();
        Projection {
            keys,
            targets,
            iterations: node.reads.iterations.clone(),
            hands_on,
            returns_document: node.ret.as_ref().is_some_and(holds_document),
        }
    }

    pub fn find(&self, id: &str) -> Option<&[usize]> {
        self.by_id.get(id).map(Vec::as_slice)
    }

    fn load_file(&mut self, file: &Path, module: &str) -> Result<(), String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let parsed = syn::parse_file(&text)
            .map_err(|e| format!("{} does not parse: {e}", file.display()))?;
        self.sources.insert(
            file.to_path_buf(),
            text.lines().map(str::to_string).collect(),
        );
        let dir = file.parent().unwrap().to_path_buf();
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let plain = if matches!(stem, "mod" | "lib" | "main") {
            dir.clone()
        } else {
            dir.join(stem)
        };
        self.load_items(
            &parsed.items,
            module,
            file,
            &Dirs {
                plain,
                path_attr: dir,
            },
        )
    }

    fn module(&mut self, m: &str) -> &mut Module {
        self.modules.entry(m.to_string()).or_default()
    }

    fn load_items(
        &mut self,
        items: &[syn::Item],
        module: &str,
        file: &Path,
        dirs: &Dirs,
    ) -> Result<(), String> {
        self.module(module);
        for item in items {
            match item {
                syn::Item::Mod(m) if !test_only(&m.attrs) => {
                    let name = m.ident.to_string();
                    let child = format!("{module}::{name}");
                    self.module(module).mods.insert(name.clone());
                    match &m.content {
                        Some((_, inner)) => {
                            let d = dirs.plain.join(&name);
                            let nested = Dirs {
                                plain: d.clone(),
                                path_attr: d,
                            };
                            self.load_items(inner, &child, file, &nested)?;
                        }
                        None => {
                            let target = match path_attr(&m.attrs) {
                                Some(p) => dirs.path_attr.join(p),
                                None => {
                                    let flat = dirs.plain.join(format!("{name}.rs"));
                                    if flat.exists() {
                                        flat
                                    } else {
                                        dirs.plain.join(&name).join("mod.rs")
                                    }
                                }
                            };
                            self.load_file(&target, &child)?;
                        }
                    }
                }
                syn::Item::Fn(f) if !test_only(&f.attrs) => {
                    self.module(module).fns.insert(f.sig.ident.to_string());
                    let id = format!("{module}::{}", f.sig.ident);
                    let ctx = Ctx {
                        module,
                        file,
                        self_ty: None,
                        outer: None,
                    };
                    self.add_fn(id, &ctx, &f.sig, &f.block);
                }
                syn::Item::ForeignMod(f) => {
                    for it in &f.items {
                        if let syn::ForeignItem::Fn(f) = it {
                            self.module(module).foreign.insert(f.sig.ident.to_string());
                        }
                    }
                }
                syn::Item::Impl(i) if !test_only(&i.attrs) => self.load_impl(i, module, file),
                syn::Item::Trait(t) if !test_only(&t.attrs) => self.load_trait(t, module, file),
                syn::Item::Struct(s) if !test_only(&s.attrs) => {
                    let name = s.ident.to_string();
                    let full = format!("{module}::{name}");
                    self.module(module).types.insert(name);
                    self.shapes.insert(full.clone(), shape(s));
                    for (i, f) in s.fields.iter().enumerate() {
                        if test_only(&f.attrs) {
                            continue;
                        }
                        let field = f
                            .ident
                            .as_ref()
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| i.to_string());
                        let written = (ty_of(&f.ty), module.to_string());
                        self.fields.insert((full.clone(), field), written.clone());
                        self.holds.entry(full.clone()).or_default().push(written);
                    }
                }
                syn::Item::Enum(e) if !test_only(&e.attrs) => {
                    let name = e.ident.to_string();
                    let full = format!("{module}::{name}");
                    self.module(module).types.insert(name);
                    for v in &e.variants {
                        for f in &v.fields {
                            self.holds
                                .entry(full.clone())
                                .or_default()
                                .push((ty_of(&f.ty), module.to_string()));
                        }
                    }
                }
                syn::Item::Union(u) if !test_only(&u.attrs) => {
                    self.module(module).types.insert(u.ident.to_string());
                }
                syn::Item::Type(t) if !test_only(&t.attrs) => {
                    let name = t.ident.to_string();
                    self.aliases.insert(
                        format!("{module}::{name}"),
                        (ty_of(&t.ty), module.to_string()),
                    );
                    self.module(module).types.insert(name);
                }
                syn::Item::Const(k) if !test_only(&k.attrs) => {
                    self.add_const(module, file, &k.ident, &k.ty, &k.expr);
                }
                syn::Item::Static(s) if !test_only(&s.attrs) => {
                    self.add_const(module, file, &s.ident, &s.ty, &s.expr);
                }
                syn::Item::Macro(m) if !test_only(&m.attrs) => {
                    if let Some(name) = &m.ident {
                        self.macro_rules.insert(name.to_string());
                    }
                }
                syn::Item::Use(u) if !test_only(&u.attrs) => {
                    let mut out = Vec::new();
                    flatten_use(&u.tree, Vec::new(), u.span().start().line, &mut out);
                    self.module(module).uses.extend(out);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn load_impl(&mut self, i: &syn::ItemImpl, module: &str, file: &Path) {
        let syn::Type::Path(self_path) = strip_refs(&i.self_ty) else {
            return;
        };
        let self_segs = segments(&self_path.path);
        let ident = self_segs.last().cloned().unwrap_or_default();
        let mut raw = RawImpl {
            module: module.to_string(),
            self_ty: self_segs,
            of_trait: i.trait_.as_ref().map(|(_, p, _)| segments(p)),
            generics: i
                .generics
                .type_params()
                .map(|p| p.ident.to_string())
                .collect(),
            fns: Vec::new(),
        };
        for it in &i.items {
            let syn::ImplItem::Fn(f) = it else {
                continue;
            };
            if test_only(&f.attrs) {
                continue;
            }
            let ctx = Ctx {
                module,
                file,
                self_ty: None,
                outer: Some(&i.generics),
            };
            let id = format!("{module}::{ident}::{}", f.sig.ident);
            let n = self.add_fn(id, &ctx, &f.sig, &f.block);
            raw.fns
                .push((n, f.sig.ident.to_string(), f.sig.receiver().is_some()));
        }
        self.raw_impls.push(raw);
    }

    fn load_trait(&mut self, t: &syn::ItemTrait, module: &str, file: &Path) {
        let name = t.ident.to_string();
        let full = format!("{module}::{name}");
        self.module(module).types.insert(name);
        self.traits.insert(full.clone());
        for it in &t.items {
            let syn::TraitItem::Fn(f) = it else {
                continue;
            };
            let Some(block) = &f.default else {
                continue;
            };
            let ctx = Ctx {
                module,
                file,
                self_ty: Some(&full),
                outer: Some(&t.generics),
            };
            let id = format!("{full}::{}", f.sig.ident);
            let n = self.add_fn(id, &ctx, &f.sig, block);
            let fname = f.sig.ident.to_string();
            self.assoc
                .entry((full.clone(), fname.clone()))
                .or_default()
                .push(n);
            self.assoc_by_name.entry(fname.clone()).or_default().push(n);
            self.trait_defaults
                .entry((full.clone(), fname.clone()))
                .or_default()
                .push(n);
            self.methods.entry(fname).or_default().push(n);
        }
    }

    /// Resolve every impl's type and trait now that every module is known.
    fn link(&mut self) {
        self.types = self
            .modules
            .iter()
            .flat_map(|(m, md)| md.types.iter().map(move |t| format!("{m}::{t}")))
            .collect();
        for raw in std::mem::take(&mut self.raw_impls) {
            let blanket = raw.self_ty.len() == 1 && raw.generics.contains(&raw.self_ty[0]);
            let self_ty = match blanket {
                true => None,
                false => Some(self.type_name_or_ext(&raw.module, &raw.self_ty)),
            };
            let of_trait = raw
                .of_trait
                .as_ref()
                .map(|t| self.type_name_or_ext(&raw.module, t));
            if let (Some(s), Some(t)) = (&self_ty, &of_trait) {
                self.implements
                    .entry(s.clone())
                    .or_default()
                    .insert(t.clone());
            }
            for (n, name, receiver) in raw.fns {
                self.nodes[n].self_ty = self_ty.clone();
                self.nodes[n].of_trait = of_trait.clone();
                if let Some(s) = &self_ty {
                    self.assoc
                        .entry((s.clone(), name.clone()))
                        .or_default()
                        .push(n);
                }
                self.assoc_by_name.entry(name.clone()).or_default().push(n);
                if let Some(t) = &of_trait {
                    self.trait_impls.entry(t.clone()).or_default().push(n);
                }
                if blanket {
                    self.blanket.entry(name.clone()).or_default().push(n);
                }
                if receiver || of_trait.is_some() {
                    self.methods.entry(name).or_default().push(n);
                }
            }
        }
    }

    fn add_const(
        &mut self,
        module: &str,
        file: &Path,
        ident: &syn::Ident,
        ty: &syn::Type,
        expr: &syn::Expr,
    ) {
        let name = ident.to_string();
        self.module(module).consts.insert(name.clone());
        let mut body = Body::default();
        body.visit_type(ty);
        body.visit_expr(expr);
        let mut env = HashMap::new();
        let locals = body.settle(&mut env);
        self.push(Node {
            id: format!("{module}::{name}"),
            module: module.to_string(),
            file: file.to_path_buf(),
            lines: (ident.span().start().line, expr.span().end().line),
            literals: body.literals,
            self_ty: None,
            of_trait: None,
            ret: Some(ty_of(ty)),
            params: Vec::new(),
            sites: body.sites,
            uses: body.uses,
            bounds: HashMap::new(),
            env,
            locals,
            mentioned: body.mentioned,
            reads: body.reads,
        });
    }

    fn add_fn(&mut self, id: String, ctx: &Ctx, sig: &syn::Signature, block: &syn::Block) -> usize {
        let mut body = Body::default();
        body.visit_generics(&sig.generics);
        body.visit_return_type(&sig.output);
        let mut bounds = HashMap::new();
        for g in ctx.outer.into_iter().chain([&sig.generics]) {
            collect_bounds(g, &mut bounds);
        }
        for input in &sig.inputs {
            if let syn::FnArg::Typed(t) = input {
                body.visit_type(&t.ty);
                body.bind(&t.pat, Recv::Declared(ty_of(&t.ty)));
            }
        }
        let ret = match &sig.output {
            syn::ReturnType::Default => None,
            syn::ReturnType::Type(_, t) => Some(ty_of(t)),
        };
        body.ret = ret.clone();
        if let Some(syn::Stmt::Expr(tail, None)) = block.stmts.last() {
            body.returned = returned_call(tail);
        }
        body.visit_block(block);
        let params = sig
            .inputs
            .iter()
            .filter_map(|i| match i {
                syn::FnArg::Typed(t) => Some(ty_of(&t.ty)),
                syn::FnArg::Receiver(_) => None,
            })
            .collect();
        let mut env = HashMap::new();
        let locals = body.settle(&mut env);
        self.push(Node {
            id,
            module: ctx.module.to_string(),
            file: ctx.file.to_path_buf(),
            lines: (
                sig.fn_token.span.start().line,
                block.brace_token.span.close().end().line,
            ),
            literals: body.literals,
            self_ty: ctx.self_ty.map(str::to_string),
            of_trait: None,
            ret,
            params,
            sites: body.sites,
            uses: body.uses,
            bounds,
            env,
            locals,
            mentioned: body.mentioned,
            reads: body.reads,
        })
    }

    fn push(&mut self, node: Node) -> usize {
        let n = self.nodes.len();
        self.by_id.entry(node.id.clone()).or_default().push(n);
        self.nodes.push(node);
        n
    }
}

struct Ctx<'a> {
    module: &'a str,
    file: &'a Path,
    self_ty: Option<&'a str>,
    outer: Option<&'a syn::Generics>,
}

fn trait_paths(
    bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::Token![+]>,
) -> Vec<Vec<String>> {
    bounds
        .iter()
        .filter_map(|b| match b {
            syn::TypeParamBound::Trait(t) => Some(segments(&t.path)),
            _ => None,
        })
        .collect()
}

fn collect_bounds(g: &syn::Generics, out: &mut HashMap<String, Vec<Vec<String>>>) {
    for p in g.type_params() {
        out.entry(p.ident.to_string())
            .or_default()
            .extend(trait_paths(&p.bounds));
    }
    if let Some(w) = &g.where_clause {
        for pred in &w.predicates {
            if let syn::WherePredicate::Type(t) = pred
                && let syn::Type::Path(p) = strip_refs(&t.bounded_ty)
                && let Some(name) = p.path.get_ident()
            {
                out.entry(name.to_string())
                    .or_default()
                    .extend(trait_paths(&t.bounds));
            }
        }
    }
}

fn strip_refs(ty: &syn::Type) -> &syn::Type {
    match ty {
        syn::Type::Reference(r) => strip_refs(&r.elem),
        syn::Type::Paren(p) => strip_refs(&p.elem),
        syn::Type::Group(g) => strip_refs(&g.elem),
        other => other,
    }
}

fn ty_of(ty: &syn::Type) -> Ty {
    match strip_refs(ty) {
        syn::Type::ImplTrait(t) => Ty::Traits(trait_paths(&t.bounds)),
        syn::Type::TraitObject(t) => Ty::Traits(trait_paths(&t.bounds)),
        syn::Type::Path(p) if p.qself.is_none() => {
            let Some(last) = p.path.segments.last() else {
                return Ty::Unknown;
            };
            let args: Vec<Ty> = match &last.arguments {
                syn::PathArguments::AngleBracketed(a) => a
                    .args
                    .iter()
                    .filter_map(|a| match a {
                        syn::GenericArgument::Type(t) => Some(ty_of(t)),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            if TRANSPARENT.contains(&last.ident.to_string().as_str())
                && let Some(inner) = args.first()
            {
                return inner.clone();
            }
            Ty::Path(segments(&p.path), args)
        }
        _ => Ty::Unknown,
    }
}

fn flatten_use(tree: &syn::UseTree, prefix: Vec<String>, line: usize, out: &mut Vec<Use>) {
    let mut push = |name: String, target: Vec<String>, renamed: bool, glob: bool| {
        out.push(Use {
            name,
            target,
            renamed,
            glob,
            line,
        })
    };
    match tree {
        syn::UseTree::Path(p) => {
            let mut next = prefix;
            next.push(p.ident.to_string());
            flatten_use(&p.tree, next, line, out);
        }
        syn::UseTree::Name(n) if n.ident == "self" => {
            if let Some(last) = prefix.last().cloned() {
                push(last, prefix, false, false);
            }
        }
        syn::UseTree::Name(n) => {
            let mut target = prefix;
            target.push(n.ident.to_string());
            push(n.ident.to_string(), target, false, false);
        }
        syn::UseTree::Rename(r) => {
            let mut target = prefix;
            if r.ident != "self" {
                target.push(r.ident.to_string());
            }
            push(r.rename.to_string(), target, r.rename != "_", false);
        }
        syn::UseTree::Glob(_) => push(String::new(), prefix, false, true),
        syn::UseTree::Group(gr) => {
            for t in &gr.items {
                flatten_use(t, prefix.clone(), line, out);
            }
        }
    }
}

#[derive(Default)]
struct Body {
    sites: Vec<Site>,
    uses: Vec<Use>,
    literals: Vec<(usize, String)>,
    bindings: BTreeMap<String, Vec<Recv>>,
    /// Items the body defines for itself: functions, types, consts.
    items: BTreeSet<String>,
    mentioned: Vec<Vec<String>>,
    reads: Reads,
    ret: Option<Ty>,
    /// Where the call the function returns starts, when it returns one.
    returned: Option<(usize, usize)>,
    /// The annotated type of the `let` whose initializer is being visited.
    let_type: Option<Ty>,
}

/// Where the call `tail` returns starts: `tail` itself, or what a method
/// chain or `?` on it starts from.
fn returned_call(tail: &syn::Expr) -> Option<(usize, usize)> {
    match tail {
        syn::Expr::Call(c) => {
            let at = c.func.span().start();
            Some((at.line, at.column))
        }
        syn::Expr::MethodCall(m) => returned_call(&m.receiver),
        syn::Expr::Try(t) => returned_call(&t.expr),
        syn::Expr::Paren(p) => returned_call(&p.expr),
        _ => None,
    }
}

/// The fields serde reads into `s`. Renaming, flattening or aliasing
/// makes them something other than the field names.
fn shape(s: &syn::ItemStruct) -> Shape {
    use quote::ToTokens;
    let serde_attr = |attrs: &[syn::Attribute]| {
        attrs
            .iter()
            .filter(|a| a.path().is_ident("serde"))
            .map(|a| a.meta.to_token_stream().to_string())
            .find(|m| ["rename", "flatten", "alias"].iter().any(|w| m.contains(w)))
    };
    if let Some(m) = serde_attr(&s.attrs) {
        return Shape::Opaque(m);
    }
    let mut fields = Vec::new();
    for f in &s.fields {
        if let Some(m) = serde_attr(&f.attrs) {
            return Shape::Opaque(m);
        }
        match &f.ident {
            Some(i) => fields.push(i.to_string()),
            None => return Shape::Opaque("a tuple struct".into()),
        }
    }
    Shape::Fields(fields)
}

fn segments(path: &syn::Path) -> Vec<String> {
    let mut out: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    if path.leading_colon.is_some() {
        out.insert(0, String::new());
    }
    out
}

fn recv_of(e: &syn::Expr) -> Recv {
    match e {
        syn::Expr::Path(p) if p.qself.is_none() => match p.path.get_ident() {
            Some(i) if i == "self" => Recv::SelfValue,
            Some(i) => Recv::Var(i.to_string()),
            None => Recv::Unknown,
        },
        syn::Expr::Call(c) => match &*c.func {
            syn::Expr::Path(p) if p.qself.is_none() => Recv::Ret(segments(&p.path)),
            _ => Recv::Unknown,
        },
        syn::Expr::MethodCall(m) => {
            Recv::MethodRet(Box::new(recv_of(&m.receiver)), m.method.to_string())
        }
        syn::Expr::Try(t) => Recv::Unwrapped(Box::new(recv_of(&t.expr))),
        syn::Expr::Await(a) => recv_of(&a.base),
        syn::Expr::Paren(p) => recv_of(&p.expr),
        syn::Expr::Group(g) => recv_of(&g.expr),
        syn::Expr::Reference(r) => recv_of(&r.expr),
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => recv_of(&u.expr),
        syn::Expr::Field(f) => {
            let name = match &f.member {
                syn::Member::Named(i) => i.to_string(),
                syn::Member::Unnamed(i) => i.index.to_string(),
            };
            Recv::Field(Box::new(recv_of(&f.base)), name)
        }
        syn::Expr::Struct(s) if s.qself.is_none() => match s.path.get_ident() {
            Some(i) if i == "Self" => Recv::SelfValue,
            _ => Recv::Declared(Ty::Path(segments(&s.path), Vec::new())),
        },
        syn::Expr::Cast(c) => Recv::Declared(ty_of(&c.ty)),
        syn::Expr::Lit(_) => Recv::Declared(Ty::Path(vec!["str".into()], Vec::new())),
        _ => Recv::Unknown,
    }
}

fn expr_attrs(e: &syn::Expr) -> &[syn::Attribute] {
    match e {
        syn::Expr::Call(x) => &x.attrs,
        syn::Expr::MethodCall(x) => &x.attrs,
        syn::Expr::Macro(x) => &x.attrs,
        syn::Expr::Block(x) => &x.attrs,
        syn::Expr::If(x) => &x.attrs,
        syn::Expr::Match(x) => &x.attrs,
        syn::Expr::Assign(x) => &x.attrs,
        syn::Expr::Path(x) => &x.attrs,
        syn::Expr::Unsafe(x) => &x.attrs,
        syn::Expr::Let(x) => &x.attrs,
        _ => &[],
    }
}

impl Body {
    fn site(&mut self, span: proc_macro2::Span, callee: Callee) {
        self.sites.push(Site {
            line: span.start().line,
            callee,
        });
    }

    /// Record what `pat` binds: `found` for a plain or typed name, and
    /// unknown for every name a destructuring binds.
    fn bind(&mut self, pat: &syn::Pat, found: Recv) {
        match pat {
            syn::Pat::Ident(i) => {
                self.bindings
                    .entry(i.ident.to_string())
                    .or_default()
                    .push(found);
                if let Some((_, sub)) = &i.subpat {
                    self.bind(sub, Recv::Unknown);
                }
            }
            syn::Pat::Type(t) => {
                self.visit_type(&t.ty);
                self.bind(&t.pat, Recv::Declared(ty_of(&t.ty)));
            }
            syn::Pat::Reference(r) => self.bind(&r.pat, found),
            syn::Pat::Paren(p) => self.bind(&p.pat, found),
            other => {
                let mut names = Names::default();
                names.visit_pat(other);
                for n in names.0 {
                    self.bindings.entry(n).or_default().push(Recv::Unknown);
                }
            }
        }
    }

    /// Each binding's one type, or unknown when bindings of that name
    /// disagree; and every name the body binds or defines.
    fn settle(&mut self, env: &mut HashMap<String, Recv>) -> BTreeSet<String> {
        let mut locals = std::mem::take(&mut self.items);
        for (name, mut found) in std::mem::take(&mut self.bindings) {
            let first = found.swap_remove(0);
            let agreed = match found.iter().all(|r| *r == first) {
                true => first,
                false => Recv::Unknown,
            };
            env.insert(name.clone(), agreed);
            locals.insert(name);
        }
        locals
    }

    fn path_callee(&mut self, p: &syn::ExprPath, call: bool) {
        if let Some(q) = &p.qself {
            self.visit_type(&q.ty);
        }
        self.mentioned.push(segments(&p.path));
        let callee = match &p.qself {
            Some(_) => match p.path.segments.last() {
                Some(last) => Callee::Qualified(last.ident.to_string()),
                None => return,
            },
            None if call => Callee::Path(segments(&p.path)),
            None => Callee::Value(segments(&p.path)),
        };
        self.site(p.span(), callee);
        for s in &p.path.segments {
            self.visit_path_arguments(&s.arguments);
        }
    }

    fn macro_body(&mut self, mac: &syn::Macro) {
        let name = mac
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        let parsed = self.parse_known_macro(&name, mac);
        if !parsed {
            self.token_calls(mac.tokens.clone(), mac.span());
        }
        self.site(mac.span(), Callee::Macro { name, parsed });
    }

    /// Visit the arguments of a macro whose arguments are expressions;
    /// false when `name` is not one, or its arguments did not parse.
    fn parse_known_macro(&mut self, name: &str, mac: &syn::Macro) -> bool {
        if CONSTANT_MACROS.contains(&name) {
            return true;
        }
        if name == "matches" {
            let Ok((e, pat, guard)) = mac.parse_body_with(parse_matches) else {
                return false;
            };
            self.visit_expr(&e);
            self.bind(&pat, Recv::Unknown);
            if let Some(guard) = &guard {
                self.visit_expr(guard);
            }
            return true;
        }
        if !EXPRESSION_MACROS.contains(&name) {
            return false;
        }
        let list = mac.parse_body_with(
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
        );
        if let Ok(args) = list {
            if let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            })) = message_arg(name).and_then(|i| args.iter().nth(i))
            {
                self.reads.messages.push((s.span().start().line, s.value()));
            }
            for a in &args {
                self.visit_expr(a);
            }
            return true;
        }
        if let Ok((a, b)) = mac.parse_body_with(parse_repeat) {
            self.visit_expr(&a);
            self.visit_expr(&b);
            return true;
        }
        false
    }

    /// Every call shape in unparsed macro tokens, as sites: `a::b(` as a
    /// path call and `.m(` as a method call on an unknown receiver.
    /// Returns whether there was any call.
    fn token_calls(&mut self, tokens: TokenStream, span: proc_macro2::Span) -> bool {
        let mut found = false;
        let mut path: Vec<String> = Vec::new();
        let mut joined = false;
        let mut after_dot = false;
        let mut colons = 0;
        for tree in tokens {
            match tree {
                TokenTree::Ident(id) => {
                    let id = id.to_string();
                    if KEYWORDS.contains(&id.as_str()) {
                        path.clear();
                        after_dot = false;
                        continue;
                    }
                    if !joined && !path.is_empty() {
                        self.mentioned.push(std::mem::take(&mut path));
                    }
                    path.push(id);
                    joined = false;
                    colons = 0;
                    continue;
                }
                TokenTree::Punct(p) if p.as_char() == ':' => {
                    colons += 1;
                    joined = colons == 2;
                    continue;
                }
                TokenTree::Punct(p) if p.as_char() == '.' => {
                    path.clear();
                    after_dot = true;
                    continue;
                }
                TokenTree::Group(g) => {
                    if g.delimiter() == proc_macro2::Delimiter::Parenthesis && !path.is_empty() {
                        found = true;
                        let callee = match after_dot {
                            true => Callee::Method(path.last().unwrap().clone(), Recv::Unknown),
                            false => Callee::Path(path.clone()),
                        };
                        self.site(span, callee);
                    }
                    found |= self.token_calls(g.stream(), span);
                }
                TokenTree::Literal(l) => {
                    let s = l.to_string();
                    self.literals
                        .push((span.start().line, s.trim_matches('"').to_string()));
                }
                TokenTree::Punct(_) => {}
            }
            if !path.is_empty() {
                self.mentioned.push(std::mem::take(&mut path));
            }
            after_dot = false;
            joined = false;
            colons = 0;
        }
        if !path.is_empty() {
            self.mentioned.push(path);
        }
        found
    }
}

type Matches = (syn::Expr, syn::Pat, Option<syn::Expr>);

fn parse_matches(input: syn::parse::ParseStream) -> syn::Result<Matches> {
    let e: syn::Expr = input.parse()?;
    input.parse::<syn::Token![,]>()?;
    let pat = syn::Pat::parse_multi_with_leading_vert(input)?;
    let guard = if input.peek(syn::Token![if]) {
        input.parse::<syn::Token![if]>()?;
        Some(input.parse()?)
    } else {
        None
    };
    input.parse::<Option<syn::Token![,]>>()?;
    Ok((e, pat, guard))
}

fn parse_repeat(input: syn::parse::ParseStream) -> syn::Result<(syn::Expr, syn::Expr)> {
    let a = input.parse()?;
    input.parse::<syn::Token![;]>()?;
    Ok((a, input.parse()?))
}

#[derive(Default)]
struct Names(Vec<String>);

impl<'ast> Visit<'ast> for Names {
    fn visit_pat_ident(&mut self, i: &'ast syn::PatIdent) {
        self.0.push(i.ident.to_string());
        syn::visit::visit_pat_ident(self, i);
    }
}

impl<'ast> Visit<'ast> for Body {
    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func
            && let Some(last) = p.path.segments.last()
            && DESERIALIZERS.contains(&last.ident.to_string().as_str())
        {
            let turbofish = match &last.arguments {
                syn::PathArguments::AngleBracketed(a) => a.args.iter().find_map(|a| match a {
                    syn::GenericArgument::Type(t) => Some(ty_of(t)),
                    _ => None,
                }),
                _ => None,
            };
            let target = turbofish.or_else(|| self.let_type.clone()).or_else(|| {
                let at = p.span().start();
                (self.returned == Some((at.line, at.column)))
                    .then(|| self.ret.clone())
                    .flatten()
            });
            self.reads.targets.push((p.span().start().line, target));
        }
        match &*c.func {
            syn::Expr::Path(p) => self.path_callee(p, true),
            other => self.visit_expr(other),
        }
        for a in &c.args {
            self.visit_expr(a);
        }
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        let method = m.method.to_string();
        if ITERATING.contains(&method.as_str()) {
            self.reads.iterations.push(m.method.span().start().line);
        }
        if matches!(method.as_str(), "context" | "expect")
            && let [
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }),
            ] = m.args.iter().collect::<Vec<_>>().as_slice()
        {
            self.reads.messages.push((s.span().start().line, s.value()));
        }
        let recv = recv_of(&m.receiver);
        self.site(m.method.span(), Callee::Method(m.method.to_string(), recv));
        self.visit_expr(&m.receiver);
        if let Some(t) = &m.turbofish {
            self.visit_angle_bracketed_generic_arguments(t);
        }
        for a in &m.args {
            self.visit_expr(a);
        }
    }

    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        self.path_callee(p, false);
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        self.mentioned.push(segments(p));
        syn::visit::visit_path(self, p);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.macro_body(m);
    }

    fn visit_lit_str(&mut self, s: &'ast syn::LitStr) {
        self.literals.push((s.span().start().line, s.value()));
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        let found = match &l.init {
            Some(init) => recv_of(&init.expr),
            None => Recv::Unknown,
        };
        self.bind(&l.pat, found);
        if let Some(init) = &l.init {
            self.let_type = match &l.pat {
                syn::Pat::Type(t) => Some(ty_of(&t.ty)),
                _ => None,
            };
            self.visit_expr(&init.expr);
            self.let_type = None;
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
    }

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        for p in &c.inputs {
            self.bind(p, Recv::Unknown);
        }
        if let syn::ReturnType::Type(_, t) = &c.output {
            self.visit_type(t);
        }
        self.visit_expr(&c.body);
    }

    fn visit_expr_for_loop(&mut self, f: &'ast syn::ExprForLoop) {
        if !matches!(&*f.expr, syn::Expr::Array(_)) {
            self.reads.iterations.push(f.for_token.span.start().line);
        }
        self.bind(&f.pat, Recv::Unknown);
        self.visit_expr(&f.expr);
        self.visit_block(&f.body);
    }

    fn visit_expr_let(&mut self, l: &'ast syn::ExprLet) {
        self.bind(&l.pat, Recv::Unknown);
        self.visit_expr(&l.expr);
    }

    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        self.bind(&a.pat, Recv::Unknown);
        if let Some((_, g)) = &a.guard {
            self.visit_expr(g);
        }
        self.visit_expr(&a.body);
    }

    fn visit_pat(&mut self, p: &'ast syn::Pat) {
        self.bind(p, Recv::Unknown);
    }

    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        flatten_use(&u.tree, Vec::new(), u.span().start().line, &mut self.uses);
    }

    fn visit_item(&mut self, i: &'ast syn::Item) {
        let (attrs, name) = match i {
            syn::Item::Fn(f) => (&f.attrs, Some(&f.sig.ident)),
            syn::Item::Mod(m) => (&m.attrs, Some(&m.ident)),
            syn::Item::Impl(m) => (&m.attrs, None),
            syn::Item::Struct(s) => (&s.attrs, Some(&s.ident)),
            syn::Item::Enum(e) => (&e.attrs, Some(&e.ident)),
            syn::Item::Const(c) => (&c.attrs, Some(&c.ident)),
            syn::Item::Static(s) => (&s.attrs, Some(&s.ident)),
            syn::Item::Type(t) => (&t.attrs, Some(&t.ident)),
            syn::Item::Trait(t) => (&t.attrs, Some(&t.ident)),
            _ => return syn::visit::visit_item(self, i),
        };
        if test_only(attrs) {
            return;
        }
        if let Some(name) = name {
            self.items.insert(name.to_string());
        }
        if let syn::Item::Struct(s) = i {
            self.reads
                .local_structs
                .insert(s.ident.to_string(), shape(s));
        }
        syn::visit::visit_item(self, i);
    }

    fn visit_stmt(&mut self, s: &'ast syn::Stmt) {
        let attrs = match s {
            syn::Stmt::Expr(e, _) => expr_attrs(e),
            syn::Stmt::Macro(m) => &m.attrs,
            syn::Stmt::Local(l) => &l.attrs,
            _ => &[],
        };
        if !test_only(attrs) {
            syn::visit::visit_stmt(self, s);
        }
    }

    fn visit_field_value(&mut self, f: &'ast syn::FieldValue) {
        if !test_only(&f.attrs) {
            syn::visit::visit_field_value(self, f);
        }
    }

    fn visit_expr_struct(&mut self, s: &'ast syn::ExprStruct) {
        self.mentioned.push(segments(&s.path));
        for f in &s.fields {
            self.visit_field_value(f);
        }
        if let Some(rest) = &s.rest {
            self.visit_expr(rest);
        }
    }
}

/// Where a path is resolved: a module, and the function it is written in.
#[derive(Clone, Copy)]
struct At<'a> {
    module: &'a str,
    node: Option<&'a Node>,
}

impl Graph {
    fn parent(module: &str) -> String {
        module
            .rsplit_once("::")
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| module.to_string())
    }

    /// `segs` with a `crate`/`self`/`super` head made absolute.
    fn absolute(module: &str, segs: &[String]) -> Option<Vec<String>> {
        let mut cur = module.to_string();
        let mut i = 0;
        match segs.first().map(String::as_str) {
            Some("crate") => {
                cur = "crate".into();
                i = 1;
            }
            Some("self") => i = 1,
            Some("super") => {
                while segs.get(i).map(String::as_str) == Some("super") {
                    cur = Self::parent(&cur);
                    i += 1;
                }
            }
            _ => return None,
        }
        let mut out: Vec<String> = cur.split("::").map(str::to_string).collect();
        out.extend(segs[i..].iter().cloned());
        Some(out)
    }

    fn resolve(&self, at: At, segs: &[String], renamed: &mut Option<usize>) -> Target {
        let Some(head) = segs.first().map(String::as_str) else {
            return Target::Unresolved;
        };
        if head.is_empty() {
            return self.external(&segs[1..]);
        }
        if head == "Self" {
            let Some(ty) = at.node.and_then(|n| n.self_ty.clone()) else {
                return Target::Nothing;
            };
            return match segs.len() {
                1 => Target::Type(ty),
                2 => self.assoc_of(&ty, &segs[1]),
                _ => Target::Nothing,
            };
        }
        if let Some(abs) = Self::absolute(at.module, segs) {
            return self.walk(&abs, renamed, 0);
        }
        if let Some(n) = at.node {
            if let Some(u) = n.uses.iter().rev().find(|u| !u.glob && u.name == head) {
                return self.through_use(at.module, u, &segs[1..], renamed, 0);
            }
            if let Some(bounds) = n.bounds.get(head) {
                return match segs.len() {
                    1 => Target::Nothing,
                    _ => {
                        let name = segs.last().unwrap();
                        match self.trait_names(at, bounds) {
                            Some(traits) => Target::Fns(self.trait_fns(&traits, name)),
                            None => Target::Fns(
                                self.assoc_by_name.get(name).cloned().unwrap_or_default(),
                            ),
                        }
                    }
                };
            }
        }
        if let Some(t) = self.in_module(at.module, segs, renamed, 0) {
            return t;
        }
        if let Some(n) = at.node {
            for u in n.uses.iter().filter(|u| u.glob) {
                if let Some(t) = self.through_glob(at.module, u, segs, renamed, 0) {
                    return t;
                }
            }
            if n.locals.contains(head) {
                return Target::Nothing;
            }
        }
        if self.externs.contains(head) || BUILTIN_CRATES.contains(&head) || PRELUDE.contains(&head)
        {
            return self.external(segs);
        }
        Target::Unresolved
    }

    /// An external path; a crate extension trait's function on an external
    /// type when the crate implements one by that name.
    fn external(&self, segs: &[String]) -> Target {
        if segs.len() >= 2 {
            let ty = format!("ext:{}", segs[segs.len() - 2]);
            let name = &segs[segs.len() - 1];
            if self.trait_impls.contains_key(&ty) {
                return Target::Fns(self.trait_fns(&[ty], name));
            }
            if let Some(f) = self.assoc.get(&(ty, name.clone())) {
                return Target::Fns(f.clone());
            }
        }
        Target::External(segs.join("::"))
    }

    fn in_module(
        &self,
        module: &str,
        segs: &[String],
        renamed: &mut Option<usize>,
        depth: usize,
    ) -> Option<Target> {
        if depth > 16 {
            return Some(Target::Unresolved);
        }
        let m = self.modules.get(module)?;
        let head = &segs[0];
        if m.fns.contains(head)
            || m.mods.contains(head)
            || m.types.contains(head)
            || m.consts.contains(head)
            || m.foreign.contains(head)
        {
            let mut abs: Vec<String> = module.split("::").map(str::to_string).collect();
            abs.extend(segs.iter().cloned());
            return Some(self.walk(&abs, renamed, depth + 1));
        }
        if let Some(u) = m.uses.iter().find(|u| !u.glob && &u.name == head) {
            return Some(self.through_use(module, u, &segs[1..], renamed, depth + 1));
        }
        for u in m.uses.iter().filter(|u| u.glob) {
            if let Some(t) = self.through_glob(module, u, segs, renamed, depth + 1) {
                return Some(t);
            }
        }
        None
    }

    fn use_target(&self, module: &str, u: &Use) -> Option<Vec<String>> {
        if let Some(abs) = Self::absolute(module, &u.target) {
            return Some(abs);
        }
        let head = u.target.first()?;
        let m = self.modules.get(module)?;
        if m.mods.contains(head) || m.types.contains(head) || m.fns.contains(head) {
            let mut abs: Vec<String> = module.split("::").map(str::to_string).collect();
            abs.extend(u.target.iter().cloned());
            return Some(abs);
        }
        None
    }

    fn through_use(
        &self,
        module: &str,
        u: &Use,
        rest: &[String],
        renamed: &mut Option<usize>,
        depth: usize,
    ) -> Target {
        if u.renamed && renamed.is_none() {
            *renamed = Some(u.line);
        }
        match self.use_target(module, u) {
            Some(mut abs) => {
                abs.extend(rest.iter().cloned());
                self.walk(&abs, renamed, depth + 1)
            }
            None => {
                let mut ext = u.target.clone();
                ext.extend(rest.iter().cloned());
                self.external(&ext)
            }
        }
    }

    fn through_glob(
        &self,
        module: &str,
        u: &Use,
        segs: &[String],
        renamed: &mut Option<usize>,
        depth: usize,
    ) -> Option<Target> {
        let target = self.use_target(module, u)?.join("::");
        if target == module || !self.modules.contains_key(&target) {
            return None;
        }
        self.in_module(&target, segs, renamed, depth + 1)
    }

    fn walk(&self, abs: &[String], renamed: &mut Option<usize>, depth: usize) -> Target {
        if abs.first().map(String::as_str) != Some("crate") {
            return self.external(abs);
        }
        let mut module = "crate".to_string();
        for (i, seg) in abs.iter().enumerate().skip(1) {
            let last = i + 1 == abs.len();
            let Some(m) = self.modules.get(&module) else {
                return Target::Unresolved;
            };
            if m.mods.contains(seg) {
                module = format!("{module}::{seg}");
                if last {
                    return Target::Nothing;
                }
                continue;
            }
            if m.foreign.contains(seg) {
                return Target::External(abs.join("::"));
            }
            if m.fns.contains(seg) || m.consts.contains(seg) {
                return match (last, self.by_id.get(&format!("{module}::{seg}"))) {
                    (true, Some(n)) => Target::Fns(n.clone()),
                    _ => Target::Unresolved,
                };
            }
            if m.types.contains(seg) {
                let full = format!("{module}::{seg}");
                return match abs.len() - i {
                    1 => Target::Type(full),
                    2 => self.assoc_of(&full, &abs[i + 1]),
                    _ => Target::Nothing,
                };
            }
            return self
                .in_module(&module, &abs[i..], renamed, depth + 1)
                .unwrap_or(Target::Unresolved);
        }
        Target::Nothing
    }

    /// `ty::name`: the associated functions of that name on `ty`, else the
    /// defaults of the traits it implements; nothing for a variant or a
    /// derived function.
    fn assoc_of(&self, ty: &str, name: &str) -> Target {
        if self.traits.contains(ty) {
            return Target::Fns(self.trait_fns(&[ty.to_string()], name));
        }
        if let Some(n) = self.assoc.get(&(ty.to_string(), name.to_string())) {
            return Target::Fns(n.clone());
        }
        let defaults: Vec<usize> = self
            .implements
            .get(ty)
            .into_iter()
            .flatten()
            .filter_map(|t| self.trait_defaults.get(&(t.clone(), name.to_string())))
            .flatten()
            .copied()
            .collect();
        match defaults.is_empty() {
            true => Target::Nothing,
            false => Target::Fns(defaults),
        }
    }

    /// The full path of the type `segs` names, or `ext:` and its name for
    /// another crate's; `None` for a generic parameter or anything else
    /// that does not resolve.
    fn type_name(&self, module: &str, node: Option<&Node>, segs: &[String]) -> Option<String> {
        let at = At { module, node };
        match self.resolve(at, segs, &mut None) {
            Target::Type(t) => Some(t),
            Target::External(_) => Some(format!("ext:{}", segs.last()?)),
            _ => None,
        }
    }

    /// Like [`Self::type_name`], naming what does not resolve as external:
    /// an impl's trait is a trait whether or not the graph can find it.
    fn type_name_or_ext(&self, module: &str, segs: &[String]) -> String {
        self.type_name(module, None, segs)
            .unwrap_or_else(|| format!("ext:{}", segs.last().cloned().unwrap_or_default()))
    }

    /// The traits `paths` name; `None` when one does not resolve, since
    /// its methods could then be anything's.
    fn trait_names(&self, at: At, paths: &[Vec<String>]) -> Option<Vec<String>> {
        paths
            .iter()
            .map(|p| self.type_name(at.module, at.node, p))
            .collect()
    }

    /// `name` called on a value bounded by `traits`.
    fn trait_fns(&self, traits: &[String], name: &str) -> Vec<usize> {
        let mut out = Vec::new();
        for t in traits {
            let impls = self.trait_impls.get(t).into_iter().flatten();
            if self.traits.contains(t) {
                out.extend(impls.filter(|n| self.nodes[**n].id.ends_with(&format!("::{name}"))));
                out.extend(
                    self.trait_defaults
                        .get(&(t.clone(), name.to_string()))
                        .into_iter()
                        .flatten(),
                );
            } else {
                out.extend(impls);
            }
        }
        out.extend(self.blanket.get(name).into_iter().flatten());
        out
    }

    /// `ty` as written at `at`, resolved.
    fn rty(&self, at: At, ty: &Ty, depth: usize) -> Rty {
        if depth > 8 {
            return Rty::Unknown;
        }
        let traits = |paths: &[Vec<String>]| match self.trait_names(at, paths) {
            Some(t) if !t.is_empty() => Rty::Traits(t),
            _ => Rty::Unknown,
        };
        match ty {
            Ty::Unknown => Rty::Unknown,
            Ty::Traits(t) => traits(t),
            Ty::Path(segs, args) => {
                if let (Some(n), [one]) = (at.node, segs.as_slice())
                    && let Some(b) = n.bounds.get(one)
                {
                    return traits(b);
                }
                let Some(name) = self.type_name(at.module, at.node, segs) else {
                    return Rty::Unknown;
                };
                if let Some((alias, module)) = self.aliases.get(&name) {
                    let there = At { module, node: None };
                    return self.rty(there, alias, depth + 1);
                }
                if self.traits.contains(&name) {
                    return Rty::Traits(vec![name]);
                }
                let args = args.iter().map(|a| self.rty(at, a, depth + 1)).collect();
                Rty::Named(name, args)
            }
        }
    }

    /// The methods named `name` that a receiver typed by `recv` can reach.
    fn method(&self, s: &Scope, name: &str, recv: &Recv) -> Vec<usize> {
        let ty = self.type_of(s, recv, 0);
        self.method_on(s, name, &ty)
    }

    fn method_on(&self, s: &Scope, name: &str, ty: &Rty) -> Vec<usize> {
        match ty {
            Rty::Unknown => self.by_name(s, name),
            Rty::Traits(ts) => self.trait_fns(ts, name),
            Rty::Named(ty, _) => {
                let mut out = match self.assoc_of(ty, name) {
                    Target::Fns(f) => f,
                    _ => Vec::new(),
                };
                for t in self.implements.get(ty).into_iter().flatten() {
                    if !self.traits.contains(t) {
                        out.extend(
                            self.trait_impls
                                .get(t)
                                .into_iter()
                                .flatten()
                                .filter(|n| self.nodes[**n].self_ty.as_ref() == Some(ty)),
                        );
                    }
                }
                if out.is_empty() && self.types.contains(ty) {
                    // A crate type without that method: it derefs to, or
                    // derives from, something the graph does not see.
                    return self.by_name(s, name);
                }
                out.extend(self.blanket.get(name).into_iter().flatten());
                out
            }
        }
    }

    /// The methods named `name` on any type a value in scope can have.
    fn by_name(&self, s: &Scope, name: &str) -> Vec<usize> {
        let held = |ty: &String| s.universe.contains(ty);
        self.methods
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|c| {
                let n = &self.nodes[*c];
                let Some(ty) = &n.self_ty else {
                    return true;
                };
                held(ty)
                    || n.of_trait.as_ref().is_some_and(held)
                    || (self.traits.contains(ty)
                        && s.universe
                            .iter()
                            .any(|u| self.implements.get(u).is_some_and(|i| i.contains(ty))))
            })
            .collect()
    }

    fn type_of(&self, s: &Scope, recv: &Recv, depth: usize) -> Rty {
        let node = s.node;
        let at = At {
            module: &node.module,
            node: Some(node),
        };
        if depth > 12 {
            return Rty::Unknown;
        }
        match recv {
            Recv::Unknown => Rty::Unknown,
            Recv::SelfValue => match &node.self_ty {
                Some(t) if self.traits.contains(t) => Rty::Traits(vec![t.clone()]),
                Some(t) => Rty::Named(t.clone(), Vec::new()),
                None => Rty::Unknown,
            },
            Recv::Var(v) => match node.env.get(v) {
                Some(r) => self.type_of(s, r, depth + 1),
                None => match self.resolve(at, std::slice::from_ref(v), &mut None) {
                    Target::Fns(f) => self.returns(&f),
                    _ => Rty::Unknown,
                },
            },
            Recv::Declared(t) => self.rty(at, t, 0),
            Recv::Ret(segs) => match self.resolve(at, segs, &mut None) {
                Target::Fns(f) => self.returns(&f),
                Target::Type(t) => Rty::Named(t, Vec::new()),
                Target::Nothing if segs.len() >= 2 => {
                    match self.resolve(at, &segs[..segs.len() - 1], &mut None) {
                        Target::Type(t) => Rty::Named(t, Vec::new()),
                        _ => Rty::Unknown,
                    }
                }
                _ => Rty::Unknown,
            },
            Recv::MethodRet(inner, m) => {
                let base = self.type_of(s, inner, depth + 1);
                if SAME_TYPE.contains(&m.as_str()) {
                    return base;
                }
                if UNWRAP.contains(&m.as_str()) {
                    return unwrap(base);
                }
                if base == Rty::Unknown {
                    return Rty::Unknown;
                }
                let found = self.method_on(s, m, &base);
                match found.is_empty() {
                    true => Rty::Unknown,
                    false => self.returns(&found),
                }
            }
            Recv::Unwrapped(inner) => unwrap(self.type_of(s, inner, depth + 1)),
            Recv::Field(inner, f) => match self.type_of(s, inner, depth + 1) {
                Rty::Named(ty, _) => match self.fields.get(&(ty, f.clone())) {
                    Some((t, module)) => self.rty(At { module, node: None }, t, 0),
                    None => Rty::Unknown,
                },
                _ => Rty::Unknown,
            },
        }
    }

    /// The one type every candidate declares it returns, if they agree.
    fn returns(&self, fns: &[usize]) -> Rty {
        let mut out: Option<Rty> = None;
        for f in fns {
            let n = &self.nodes[*f];
            let at = At {
                module: &n.module,
                node: Some(n),
            };
            let r = match &n.ret {
                Some(t) => self.rty(at, t, 0),
                None => Rty::Unknown,
            };
            match &out {
                None => out = Some(r),
                Some(o) if *o == r => {}
                _ => return Rty::Unknown,
            }
        }
        out.unwrap_or(Rty::Unknown)
    }

    /// The crate's types in `names`, with every type a value of one of them
    /// holds, transitively.
    fn held(&self, names: impl IntoIterator<Item = String>) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut todo: Vec<String> = names.into_iter().collect();
        while let Some(t) = todo.pop() {
            if !(self.types.contains(&t) || self.traits.contains(&t)) || !out.insert(t.clone()) {
                continue;
            }
            for (ty, module) in self
                .holds
                .get(&t)
                .into_iter()
                .flatten()
                .chain(self.aliases.get(&t))
            {
                let at = At { module, node: None };
                rty_names(&self.rty(at, ty, 0), &mut todo);
            }
        }
        out
    }

    /// The types of this crate a path names, at any of its prefixes.
    fn named_types(&self, at: At, segs: &[String], out: &mut Vec<String>) {
        for k in 1..=segs.len() {
            if let Target::Type(t) = self.resolve(at, &segs[..k], &mut None) {
                out.push(t);
            }
        }
    }

    /// Every edge out of `n`, and what in it cannot be followed.
    ///
    /// A method on an unknown receiver reaches the methods of every type a
    /// value in the function can have. That set grows with the edges it
    /// admits, so it is computed to a fixed point.
    pub fn edges(&self, n: usize) -> (Vec<Edge>, Vec<Refusal>) {
        let node = &self.nodes[n];
        let at = At {
            module: &node.module,
            node: Some(node),
        };
        let mut named = Vec::new();
        for p in &node.mentioned {
            self.named_types(at, p, &mut named);
        }
        named.extend(node.self_ty.iter().cloned());
        named.extend(node.of_trait.iter().cloned());
        let mut scope = Scope {
            node,
            universe: self.held(named),
        };
        loop {
            let (edges, refusals) = self.edges_in(&scope);
            let mut returned = Vec::new();
            for e in &edges {
                if let To::Node(t) = e.to {
                    let callee = &self.nodes[t];
                    let there = At {
                        module: &callee.module,
                        node: Some(callee),
                    };
                    rty_names(&self.returns(&[t]), &mut returned);
                    for p in &callee.params {
                        rty_names(&self.rty(there, p, 0), &mut returned);
                    }
                }
            }
            let grown = self.held(scope.universe.iter().cloned().chain(returned));
            if grown == scope.universe {
                return (edges, refusals);
            }
            scope.universe = grown;
        }
    }

    fn edges_in(&self, s: &Scope) -> (Vec<Edge>, Vec<Refusal>) {
        let node = s.node;
        let at = At {
            module: &node.module,
            node: Some(node),
        };
        let mut edges = Vec::new();
        let mut refusals = Vec::new();
        for site in &node.sites {
            let mut renamed = None;
            let (target, value) = match &site.callee {
                Callee::Path(p) => (self.resolve(at, p, &mut renamed), false),
                Callee::Value(p) if p.len() == 1 && node.locals.contains(&p[0]) => continue,
                Callee::Value(p) => (self.resolve(at, p, &mut renamed), true),
                Callee::Qualified(name) => (
                    Target::Fns(
                        self.assoc_by_name
                            .get(name)
                            .into_iter()
                            .flatten()
                            .copied()
                            .filter(|c| {
                                let c = &self.nodes[*c];
                                c.self_ty.as_ref().is_none_or(|t| s.universe.contains(t))
                                    || c.of_trait.as_ref().is_some_and(|t| s.universe.contains(t))
                            })
                            .collect(),
                    ),
                    false,
                ),
                Callee::Method(name, recv) => {
                    edges.push(Edge {
                        line: site.line,
                        to: To::Method(name.clone()),
                        value: false,
                    });
                    (Target::Fns(self.method(s, name, recv)), false)
                }
                Callee::Macro { name, parsed } => {
                    if self.macro_rules.contains(name) {
                        refusals.push(Refusal {
                            line: site.line,
                            form: format!("{name}!"),
                            what: format!(
                                "`{name}!`, a macro this crate defines, whose expansion is not followed"
                            ),
                        });
                    } else if !parsed {
                        refusals.push(Refusal {
                            line: site.line,
                            form: format!("{name}!"),
                            what: format!("`{name}!`, whose expansion is not followed"),
                        });
                    }
                    continue;
                }
            };
            if let Some(line) = renamed {
                refusals.push(Refusal {
                    line: site.line,
                    form: "use as".into(),
                    what: format!("a path through the renaming `use` at line {line}"),
                });
            }
            match target {
                Target::External(path) => edges.push(Edge {
                    line: site.line,
                    to: To::External(path),
                    value,
                }),
                Target::Fns(fns) => {
                    for f in fns {
                        edges.push(Edge {
                            line: site.line,
                            to: To::Node(f),
                            value,
                        });
                    }
                }
                Target::Type(_) | Target::Nothing => {}
                Target::Unresolved => {
                    let shown = match &site.callee {
                        Callee::Path(p) | Callee::Value(p) => p.join("::"),
                        _ => String::new(),
                    };
                    refusals.push(Refusal {
                        line: site.line,
                        form: "unresolved".into(),
                        what: format!("`{shown}`, which does not resolve"),
                    });
                }
            }
        }
        (edges, refusals)
    }
}

/// What a deserializer returning `ty` produces: through `Result` and
/// `Option`.
fn produced(ty: &Ty) -> &Ty {
    match ty {
        Ty::Path(segs, args)
            if segs.last().is_some_and(|l| l == "Result" || l == "Option") && !args.is_empty() =>
        {
            produced(&args[0])
        }
        other => other,
    }
}

fn holds_document(ty: &Ty) -> bool {
    match ty {
        Ty::Path(segs, args) => {
            segs.last()
                .is_some_and(|l| RAW_DOCUMENTS.contains(&l.as_str()))
                || args.iter().any(holds_document)
        }
        _ => false,
    }
}

fn unwrap(base: Rty) -> Rty {
    match base {
        Rty::Named(n, args) if matches!(n.as_str(), "ext:Option" | "ext:Result") => {
            args.into_iter().next().unwrap_or(Rty::Unknown)
        }
        _ => Rty::Unknown,
    }
}

/// What one function's edges are resolved in.
struct Scope<'a> {
    node: &'a Node,
    universe: BTreeSet<String>,
}

fn rty_names(r: &Rty, out: &mut Vec<String>) {
    match r {
        Rty::Named(n, args) => {
            out.push(n.clone());
            for a in args {
                rty_names(a, out);
            }
        }
        Rty::Traits(t) => out.extend(t.iter().cloned()),
        Rty::Unknown => {}
    }
}
