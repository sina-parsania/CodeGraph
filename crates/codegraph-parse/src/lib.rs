//! Grammar-driven parser: one generic tree-sitter walk parameterized by a
//! per-language `LangSpec` (label map, call-node kinds, callee fields, name
//! extraction mode). Adding a language = one grammar dep + one `LangSpec`.

use codegraph_core::{InheritKind, Metadata, Node, NodeLabel, QualifiedName, RawCall, RawInherit};
use tree_sitter::{Language, Node as TsNode, Parser};

pub struct ParsedFile {
    pub nodes: Vec<Node>,
    pub calls: Vec<RawCall>,
    pub inherits: Vec<RawInherit>,
}

impl ParsedFile {
    fn empty() -> Self {
        ParsedFile { nodes: Vec::new(), calls: Vec::new(), inherits: Vec::new() }
    }
}

#[derive(Clone, Copy)]
enum NameMode {
    Field,
    CDeclarator,
}

type InheritFn = fn(TsNode, &[u8]) -> Vec<RawInherit>;

struct LangSpec {
    name: &'static str,
    language: fn() -> Language,
    label_for: fn(&str) -> Option<NodeLabel>,
    call_kinds: &'static [&'static str],
    callee_fields: &'static [&'static str],
    name_mode: NameMode,
    inherit_fn: Option<InheritFn>,
}

pub fn parse_file(project: &str, rel_path: &str, source: &str) -> ParsedFile {
    let ext = rel_path.rsplit('.').next().unwrap_or("");
    match spec_for_ext(ext) {
        Some(spec) => parse_with(spec, project, rel_path, source),
        None => ParsedFile::empty(),
    }
}

fn spec_for_ext(ext: &str) -> Option<&'static LangSpec> {
    let s = match ext {
        "rs" => &RUST,
        "py" | "pyi" => &PYTHON,
        "js" | "jsx" | "mjs" | "cjs" => &JS,
        "ts" | "mts" | "cts" => &TS,
        "tsx" => &TSX,
        "go" => &GO,
        "swift" => &SWIFT,
        "java" => &JAVA,
        "c" | "h" => &C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => &CPP,
        "rb" => &RUBY,
        "cs" => &CSHARP,
        "sh" | "bash" => &BASH,
        _ => return None,
    };
    Some(s)
}

// Back-compat single-language entry points (used by tests).
pub fn parse_rust(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&RUST, p, r, s) }
pub fn parse_python(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&PYTHON, p, r, s) }
pub fn parse_js(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&JS, p, r, s) }
pub fn parse_ts(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&TS, p, r, s) }
pub fn parse_tsx(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&TSX, p, r, s) }
pub fn parse_go(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&GO, p, r, s) }
pub fn parse_swift(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&SWIFT, p, r, s) }
pub fn parse_java(p: &str, r: &str, s: &str) -> ParsedFile { parse_with(&JAVA, p, r, s) }

static RUST: LangSpec = LangSpec { name: "rust", language: || tree_sitter_rust::LANGUAGE.into(), label_for: rust_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(rust_inherits) };
static PYTHON: LangSpec = LangSpec { name: "python", language: || tree_sitter_python::LANGUAGE.into(), label_for: python_label, call_kinds: &["call"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(py_inherits) };
static JS: LangSpec = LangSpec { name: "javascript", language: || tree_sitter_javascript::LANGUAGE.into(), label_for: js_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None };
static TS: LangSpec = LangSpec { name: "typescript", language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), label_for: ts_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(ts_inherits) };
static TSX: LangSpec = LangSpec { name: "typescript", language: || tree_sitter_typescript::LANGUAGE_TSX.into(), label_for: ts_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: Some(ts_inherits) };
static GO: LangSpec = LangSpec { name: "go", language: || tree_sitter_go::LANGUAGE.into(), label_for: go_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None };
static SWIFT: LangSpec = LangSpec { name: "swift", language: || tree_sitter_swift::LANGUAGE.into(), label_for: swift_label, call_kinds: &["call_expression"], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: None };
static JAVA: LangSpec = LangSpec { name: "java", language: || tree_sitter_java::LANGUAGE.into(), label_for: java_label, call_kinds: &["method_invocation"], callee_fields: &["name"], name_mode: NameMode::Field, inherit_fn: Some(java_inherits) };
static C: LangSpec = LangSpec { name: "c", language: || tree_sitter_c::LANGUAGE.into(), label_for: c_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::CDeclarator, inherit_fn: None };
static CPP: LangSpec = LangSpec { name: "cpp", language: || tree_sitter_cpp::LANGUAGE.into(), label_for: cpp_label, call_kinds: &["call_expression"], callee_fields: &["function"], name_mode: NameMode::CDeclarator, inherit_fn: None };
static RUBY: LangSpec = LangSpec { name: "ruby", language: || tree_sitter_ruby::LANGUAGE.into(), label_for: ruby_label, call_kinds: &["call"], callee_fields: &["method"], name_mode: NameMode::Field, inherit_fn: None };
static CSHARP: LangSpec = LangSpec { name: "csharp", language: || tree_sitter_c_sharp::LANGUAGE.into(), label_for: csharp_label, call_kinds: &["invocation_expression"], callee_fields: &["function"], name_mode: NameMode::Field, inherit_fn: None };
static BASH: LangSpec = LangSpec { name: "bash", language: || tree_sitter_bash::LANGUAGE.into(), label_for: bash_label, call_kinds: &[], callee_fields: &[], name_mode: NameMode::Field, inherit_fn: None };

fn rust_label(k: &str) -> Option<NodeLabel> {
    match k { "function_item" => Some(NodeLabel::Function), "struct_item" | "union_item" => Some(NodeLabel::Class), "enum_item" => Some(NodeLabel::Enum), "trait_item" => Some(NodeLabel::Interface), "type_item" => Some(NodeLabel::Type), "mod_item" => Some(NodeLabel::Module), _ => None }
}
fn python_label(k: &str) -> Option<NodeLabel> {
    match k { "function_definition" => Some(NodeLabel::Function), "class_definition" => Some(NodeLabel::Class), _ => None }
}
fn js_label(k: &str) -> Option<NodeLabel> {
    match k { "function_declaration" | "generator_function_declaration" => Some(NodeLabel::Function), "class_declaration" => Some(NodeLabel::Class), "method_definition" => Some(NodeLabel::Method), _ => None }
}
fn ts_label(k: &str) -> Option<NodeLabel> {
    match k { "interface_declaration" => Some(NodeLabel::Interface), "type_alias_declaration" => Some(NodeLabel::Type), "enum_declaration" => Some(NodeLabel::Enum), "abstract_class_declaration" => Some(NodeLabel::Class), other => js_label(other) }
}
fn go_label(k: &str) -> Option<NodeLabel> {
    match k { "function_declaration" => Some(NodeLabel::Function), "method_declaration" => Some(NodeLabel::Method), "type_spec" => Some(NodeLabel::Class), _ => None }
}
fn swift_label(k: &str) -> Option<NodeLabel> {
    // tree-sitter-swift parses class/struct/enum/actor all as `class_declaration`.
    match k { "function_declaration" => Some(NodeLabel::Function), "class_declaration" => Some(NodeLabel::Class), "protocol_declaration" => Some(NodeLabel::Interface), "typealias_declaration" => Some(NodeLabel::Type), _ => None }
}
fn java_label(k: &str) -> Option<NodeLabel> {
    match k { "method_declaration" | "constructor_declaration" => Some(NodeLabel::Method), "class_declaration" | "record_declaration" => Some(NodeLabel::Class), "interface_declaration" | "annotation_type_declaration" => Some(NodeLabel::Interface), "enum_declaration" => Some(NodeLabel::Enum), _ => None }
}
fn c_label(k: &str) -> Option<NodeLabel> {
    match k { "function_definition" => Some(NodeLabel::Function), "struct_specifier" | "union_specifier" => Some(NodeLabel::Class), "enum_specifier" => Some(NodeLabel::Enum), "type_definition" => Some(NodeLabel::Type), _ => None }
}
fn cpp_label(k: &str) -> Option<NodeLabel> {
    match k { "class_specifier" => Some(NodeLabel::Class), "namespace_definition" => Some(NodeLabel::Module), other => c_label(other) }
}
fn ruby_label(k: &str) -> Option<NodeLabel> {
    match k { "method" | "singleton_method" => Some(NodeLabel::Method), "class" => Some(NodeLabel::Class), "module" => Some(NodeLabel::Module), _ => None }
}
fn csharp_label(k: &str) -> Option<NodeLabel> {
    match k { "method_declaration" | "constructor_declaration" => Some(NodeLabel::Method), "class_declaration" | "record_declaration" | "struct_declaration" => Some(NodeLabel::Class), "interface_declaration" => Some(NodeLabel::Interface), "enum_declaration" => Some(NodeLabel::Enum), _ => None }
}
fn bash_label(k: &str) -> Option<NodeLabel> {
    match k { "function_definition" => Some(NodeLabel::Function), _ => None }
}

fn rust_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "impl_item" {
        return Vec::new();
    }
    let Some(typ) = node.child_by_field_name("type").and_then(|t| trailing_ident(t, src)) else {
        return Vec::new();
    };
    if let Some(tr) = node.child_by_field_name("trait").and_then(|t| trailing_ident(t, src)) {
        return vec![RawInherit { impl_name: typ, super_name: tr, kind: InheritKind::Implements }];
    }
    Vec::new()
}

fn py_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "class_definition" {
        return Vec::new();
    }
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let Some(args) = node.child_by_field_name("superclasses") else { return Vec::new() };
    let mut out = Vec::new();
    let mut c = args.walk();
    for ch in args.named_children(&mut c) {
        if let Some(sup) = trailing_ident(ch, src) {
            out.push(RawInherit { impl_name: cls.clone(), super_name: sup, kind: InheritKind::Extends });
        }
    }
    out
}

fn ts_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    if node.kind() != "class_declaration" {
        return Vec::new();
    }
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let mut out = Vec::new();
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        if ch.kind() != "class_heritage" {
            continue;
        }
        let mut h = ch.walk();
        for clause in ch.children(&mut h) {
            let kind = match clause.kind() {
                "extends_clause" => InheritKind::Extends,
                "implements_clause" => InheritKind::Implements,
                _ => continue,
            };
            for sup in type_idents(clause, src) {
                out.push(RawInherit { impl_name: cls.clone(), super_name: sup, kind });
            }
        }
    }
    out
}

fn java_inherits(node: TsNode, src: &[u8]) -> Vec<RawInherit> {
    let Some(cls) = field_text(node, "name", src) else { return Vec::new() };
    let mut out = Vec::new();
    match node.kind() {
        "class_declaration" => {
            if let Some(sc) = node.child_by_field_name("superclass") {
                for s in type_idents(sc, src) {
                    out.push(RawInherit { impl_name: cls.clone(), super_name: s, kind: InheritKind::Extends });
                }
            }
            if let Some(ifs) = node.child_by_field_name("interfaces") {
                for s in type_idents(ifs, src) {
                    out.push(RawInherit { impl_name: cls.clone(), super_name: s, kind: InheritKind::Implements });
                }
            }
        }
        "interface_declaration" => {
            if let Some(ifs) = node.child_by_field_name("interfaces") {
                for s in type_idents(ifs, src) {
                    out.push(RawInherit { impl_name: cls.clone(), super_name: s, kind: InheritKind::Extends });
                }
            }
        }
        _ => {}
    }
    out
}

fn type_idents(node: TsNode, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "type_identifier" | "identifier") {
            if let Ok(s) = std::str::from_utf8(&src[n.byte_range()]) {
                out.push(s.to_string());
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    out
}

fn parse_with(spec: &LangSpec, project: &str, rel_path: &str, source: &str) -> ParsedFile {
    let mut parser = Parser::new();
    if parser.set_language(&(spec.language)()).is_err() {
        return ParsedFile::empty();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return ParsedFile::empty(),
    };
    let bytes = source.as_bytes();
    let mut dir: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    let filename = dir.pop().unwrap_or("file");
    let mut file_segs = dir.clone();
    file_segs.push(filename);
    let file_id = QualifiedName::build(project, &dir, filename);

    let mut nodes = vec![Node {
        id: file_id.clone(),
        label: NodeLabel::File,
        name: filename.to_string(),
        file_path: rel_path.to_string(),
        line_start: 1,
        line_end: source.lines().count().max(1) as u32,
        language: spec.name.to_string(),
        metadata: Metadata::new(),
        community: None,
        pagerank: 0.0,
        betweenness: 0.0,
    }];
    let mut calls = Vec::new();
    let mut inherits = Vec::new();
    let ctx = Ctx { spec, project, segs: &file_segs, rel_path, file_id: &file_id };
    collect(tree.root_node(), bytes, &ctx, None, &mut nodes, &mut calls, &mut inherits);
    ParsedFile { nodes, calls, inherits }
}

struct Ctx<'a> {
    spec: &'a LangSpec,
    project: &'a str,
    segs: &'a [&'a str],
    rel_path: &'a str,
    file_id: &'a str,
}

fn collect(node: TsNode, src: &[u8], ctx: &Ctx, current_fn: Option<&str>, nodes: &mut Vec<Node>, calls: &mut Vec<RawCall>, inherits: &mut Vec<RawInherit>) {
    let mut my_fn_id: Option<String> = None;

    if let Some(label) = (ctx.spec.label_for)(node.kind()) {
        if let Some(name) = name_of(node, src, ctx.spec.name_mode) {
            if !name.is_empty() {
                let id = QualifiedName::build(ctx.project, ctx.segs, &name);
                if matches!(label, NodeLabel::Function | NodeLabel::Method) {
                    my_fn_id = Some(id.clone());
                }
                nodes.push(Node {
                    id,
                    label,
                    name,
                    file_path: ctx.rel_path.to_string(),
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                    language: ctx.spec.name.to_string(),
                    metadata: Metadata::new(),
                    community: None,
                    pagerank: 0.0,
                    betweenness: 0.0,
                });
            }
        }
    }

    if ctx.spec.call_kinds.contains(&node.kind()) {
        if let Some(callee) = callee_name(node, src, ctx.spec.callee_fields) {
            calls.push(RawCall {
                caller_id: current_fn.unwrap_or(ctx.file_id).to_string(),
                callee_name: callee,
                line: node.start_position().row as u32 + 1,
            });
        }
    }

    if let Some(inf) = ctx.spec.inherit_fn {
        inherits.extend(inf(node, src));
    }

    let next_fn = my_fn_id.as_deref().or(current_fn);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, src, ctx, next_fn, nodes, calls, inherits);
    }
}

fn name_of(node: TsNode, src: &[u8], mode: NameMode) -> Option<String> {
    match mode {
        NameMode::Field => field_text(node, "name", src),
        NameMode::CDeclarator => {
            if let Some(s) = field_text(node, "name", src) {
                return Some(s);
            }
            // C/C++ function: descend the declarator chain to the identifier.
            let mut d = node.child_by_field_name("declarator")?;
            loop {
                match d.kind() {
                    "identifier" | "field_identifier" | "type_identifier" | "operator_name" => {
                        return std::str::from_utf8(&src[d.byte_range()]).ok().map(|s| s.to_string());
                    }
                    _ => d = d.child_by_field_name("declarator")?,
                }
            }
        }
    }
}

fn field_text(node: TsNode, field: &str, src: &[u8]) -> Option<String> {
    std::str::from_utf8(&src[node.child_by_field_name(field)?.byte_range()]).ok().map(|s| s.to_string())
}

fn callee_name(call: TsNode, src: &[u8], fields: &[&str]) -> Option<String> {
    for f in fields {
        if let Some(c) = call.child_by_field_name(f) {
            if let Some(s) = trailing_ident(c, src) {
                return Some(s);
            }
        }
    }
    let mut cursor = call.walk();
    for child in call.named_children(&mut cursor) {
        if let Some(s) = trailing_ident(child, src) {
            return Some(s);
        }
    }
    None
}

fn trailing_ident(node: TsNode, src: &[u8]) -> Option<String> {
    let k = node.kind();
    if k == "identifier" || k == "simple_identifier" || k.ends_with("_identifier") {
        return std::str::from_utf8(&src[node.byte_range()]).ok().map(|s| s.to_string());
    }
    for field in ["name", "field", "attribute", "property", "method"] {
        if let Some(c) = node.child_by_field_name(field) {
            if let Some(s) = trailing_ident(c, src) {
                return Some(s);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pf: &ParsedFile) -> Vec<(String, NodeLabel)> {
        pf.nodes.iter().map(|n| (n.name.clone(), n.label)).collect()
    }
    fn has(pf: &ParsedFile, name: &str, label: NodeLabel) -> bool {
        names(pf).iter().any(|(n, l)| n == name && *l == label)
    }

    #[test]
    fn rust_python_js_ts_go_still_work() {
        assert!(has(&parse_rust("p", "a.rs", "fn helper(){}\nfn main(){ helper(); }\nstruct S;\ntrait T{}\n"), "helper", NodeLabel::Function));
        assert!(has(&parse_python("p", "a.py", "def foo():\n  pass\nclass Bar:\n  pass\n"), "Bar", NodeLabel::Class));
        assert!(has(&parse_ts("p", "a.ts", "interface I{}\nfunction f(){}\n"), "I", NodeLabel::Interface));
        assert!(has(&parse_go("p", "a.go", "package m\nfunc run(){}\ntype S struct{}\n"), "run", NodeLabel::Function));
    }

    #[test]
    fn swift_defs_and_calls() {
        let pf = parse_swift("p", "A.swift", "func helper() {}\nfunc run() { helper() }\nclass C {}\nprotocol P {}\nenum E { case a }\nstruct S {}\n");
        assert!(has(&pf, "helper", NodeLabel::Function));
        assert!(has(&pf, "run", NodeLabel::Function));
        assert!(has(&pf, "C", NodeLabel::Class));
        assert!(has(&pf, "P", NodeLabel::Interface));
        assert!(has(&pf, "E", NodeLabel::Class));
        assert!(has(&pf, "S", NodeLabel::Class));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper" && c.caller_id.ends_with("run")));
    }

    #[test]
    fn java_defs_and_calls() {
        let pf = parse_java("p", "A.java", "class A {\n  void run() { helper(); }\n  void helper() {}\n}\ninterface I {}\nenum E { X }\n");
        assert!(has(&pf, "A", NodeLabel::Class));
        assert!(has(&pf, "run", NodeLabel::Method));
        assert!(has(&pf, "I", NodeLabel::Interface));
        assert!(has(&pf, "E", NodeLabel::Enum));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper"));
    }

    #[test]
    fn c_function_name_from_declarator() {
        let pf = parse_file("p", "a.c", "int helper(int x) { return x; }\nint main() { return helper(2); }\nstruct Point { int x; };\n");
        assert!(has(&pf, "helper", NodeLabel::Function));
        assert!(has(&pf, "main", NodeLabel::Function));
        assert!(has(&pf, "Point", NodeLabel::Class));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper"));
    }

    #[test]
    fn ruby_csharp_bash() {
        assert!(has(&parse_file("p", "a.rb", "def foo\nend\nclass Bar\nend\n"), "Bar", NodeLabel::Class));
        assert!(has(&parse_file("p", "a.cs", "class C { void M() {} }"), "M", NodeLabel::Method));
        assert!(has(&parse_file("p", "a.sh", "greet() { echo hi; }\n"), "greet", NodeLabel::Function));
    }


    #[test]
    fn inheritance_extraction() {
        let py = parse_python("p", "a.py", "class Animal:\n    pass\nclass Dog(Animal):\n    pass\n");
        assert!(py.inherits.iter().any(|i| i.impl_name == "Dog" && i.super_name == "Animal" && i.kind == InheritKind::Extends));
        let ts = parse_ts("p", "a.ts", "interface Service {}\nclass Impl implements Service {}\nclass Base {}\nclass Sub extends Base {}\n");
        assert!(ts.inherits.iter().any(|i| i.impl_name == "Impl" && i.super_name == "Service" && i.kind == InheritKind::Implements));
        assert!(ts.inherits.iter().any(|i| i.impl_name == "Sub" && i.super_name == "Base" && i.kind == InheritKind::Extends));
        let java = parse_java("p", "A.java", "interface I {}\nclass A implements I {}\nclass B extends A {}\n");
        assert!(java.inherits.iter().any(|i| i.impl_name == "A" && i.super_name == "I" && i.kind == InheritKind::Implements));
        assert!(java.inherits.iter().any(|i| i.impl_name == "B" && i.super_name == "A" && i.kind == InheritKind::Extends));
        let rust = parse_rust("p", "a.rs", "struct Foo;\ntrait Show {}\nimpl Show for Foo {}\n");
        assert!(rust.inherits.iter().any(|i| i.impl_name == "Foo" && i.super_name == "Show" && i.kind == InheritKind::Implements));
    }

    #[test]
    fn unknown_extension_is_empty() {
        assert!(parse_file("p", "a.unknown", "stuff").nodes.is_empty());
    }
}
