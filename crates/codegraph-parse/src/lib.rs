//! Grammar-driven parser: one generic tree-sitter walk + a per-language
//! kind→label map. Rust/Python/JS/TS/Go today; adding a language is a `LANGUAGE`
//! + a `*_label` fn + a `parse_file` arm.

use codegraph_core::{Metadata, Node, NodeLabel, QualifiedName, RawCall};
use tree_sitter::{Node as TsNode, Parser};

pub struct ParsedFile {
    pub nodes: Vec<Node>,
    pub calls: Vec<RawCall>,
}

impl ParsedFile {
    fn empty() -> Self {
        ParsedFile { nodes: Vec::new(), calls: Vec::new() }
    }
}

/// Dispatch by file extension. Unknown extensions yield an empty result.
pub fn parse_file(project: &str, rel_path: &str, source: &str) -> ParsedFile {
    match rel_path.rsplit('.').next().unwrap_or("") {
        "rs" => parse_rust(project, rel_path, source),
        "py" | "pyi" => parse_python(project, rel_path, source),
        "js" | "jsx" | "mjs" | "cjs" => parse_js(project, rel_path, source),
        "ts" | "mts" | "cts" => parse_ts(project, rel_path, source),
        "tsx" => parse_tsx(project, rel_path, source),
        "go" => parse_go(project, rel_path, source),
        _ => ParsedFile::empty(),
    }
}

pub fn parse_rust(p: &str, r: &str, s: &str) -> ParsedFile {
    parse_with(tree_sitter_rust::LANGUAGE.into(), "rust", rust_label, p, r, s)
}
pub fn parse_python(p: &str, r: &str, s: &str) -> ParsedFile {
    parse_with(tree_sitter_python::LANGUAGE.into(), "python", python_label, p, r, s)
}
pub fn parse_js(p: &str, r: &str, s: &str) -> ParsedFile {
    parse_with(tree_sitter_javascript::LANGUAGE.into(), "javascript", js_label, p, r, s)
}
pub fn parse_ts(p: &str, r: &str, s: &str) -> ParsedFile {
    parse_with(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), "typescript", ts_label, p, r, s)
}
pub fn parse_tsx(p: &str, r: &str, s: &str) -> ParsedFile {
    parse_with(tree_sitter_typescript::LANGUAGE_TSX.into(), "typescript", ts_label, p, r, s)
}
pub fn parse_go(p: &str, r: &str, s: &str) -> ParsedFile {
    parse_with(tree_sitter_go::LANGUAGE.into(), "go", go_label, p, r, s)
}

fn rust_label(kind: &str) -> Option<NodeLabel> {
    match kind {
        "function_item" => Some(NodeLabel::Function),
        "struct_item" | "union_item" => Some(NodeLabel::Class),
        "enum_item" => Some(NodeLabel::Enum),
        "trait_item" => Some(NodeLabel::Interface),
        "type_item" => Some(NodeLabel::Type),
        "mod_item" => Some(NodeLabel::Module),
        _ => None,
    }
}
fn python_label(kind: &str) -> Option<NodeLabel> {
    match kind {
        "function_definition" => Some(NodeLabel::Function),
        "class_definition" => Some(NodeLabel::Class),
        _ => None,
    }
}
fn js_label(kind: &str) -> Option<NodeLabel> {
    match kind {
        "function_declaration" | "generator_function_declaration" => Some(NodeLabel::Function),
        "class_declaration" => Some(NodeLabel::Class),
        "method_definition" => Some(NodeLabel::Method),
        _ => None,
    }
}
fn ts_label(kind: &str) -> Option<NodeLabel> {
    match kind {
        "interface_declaration" => Some(NodeLabel::Interface),
        "type_alias_declaration" => Some(NodeLabel::Type),
        "enum_declaration" => Some(NodeLabel::Enum),
        "abstract_class_declaration" => Some(NodeLabel::Class),
        other => js_label(other),
    }
}
fn go_label(kind: &str) -> Option<NodeLabel> {
    match kind {
        "function_declaration" => Some(NodeLabel::Function),
        "method_declaration" => Some(NodeLabel::Method),
        "type_spec" => Some(NodeLabel::Class),
        _ => None,
    }
}

type LabelFn = fn(&str) -> Option<NodeLabel>;

fn parse_with(language: tree_sitter::Language, lang: &str, label_for: LabelFn, project: &str, rel_path: &str, source: &str) -> ParsedFile {
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
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
        language: lang.to_string(),
        metadata: Metadata::new(),
        community: None,
        pagerank: 0.0,
        betweenness: 0.0,
    }];
    let mut calls = Vec::new();
    let ctx = Ctx { project, segs: &file_segs, rel_path, file_id: &file_id, lang, label_for };
    collect(tree.root_node(), bytes, &ctx, None, &mut nodes, &mut calls);
    ParsedFile { nodes, calls }
}

struct Ctx<'a> {
    project: &'a str,
    segs: &'a [&'a str],
    rel_path: &'a str,
    file_id: &'a str,
    lang: &'a str,
    label_for: LabelFn,
}

fn collect(node: TsNode, src: &[u8], ctx: &Ctx, current_fn: Option<&str>, nodes: &mut Vec<Node>, calls: &mut Vec<RawCall>) {
    let mut my_fn_id: Option<String> = None;

    if let Some(label) = (ctx.label_for)(node.kind()) {
        if let Some(name) = text_of_field(node, "name", src) {
            if !name.is_empty() {
                let id = QualifiedName::build(ctx.project, ctx.segs, name);
                if matches!(label, NodeLabel::Function | NodeLabel::Method) {
                    my_fn_id = Some(id.clone());
                }
                nodes.push(Node {
                    id,
                    label,
                    name: name.to_string(),
                    file_path: ctx.rel_path.to_string(),
                    line_start: node.start_position().row as u32 + 1,
                    line_end: node.end_position().row as u32 + 1,
                    language: ctx.lang.to_string(),
                    metadata: Metadata::new(),
                    community: None,
                    pagerank: 0.0,
                    betweenness: 0.0,
                });
            }
        }
    }

    if matches!(node.kind(), "call_expression" | "call") {
        if let Some(callee) = node.child_by_field_name("function").and_then(|f| trailing_ident(f, src)) {
            calls.push(RawCall {
                caller_id: current_fn.unwrap_or(ctx.file_id).to_string(),
                callee_name: callee,
                line: node.start_position().row as u32 + 1,
            });
        }
    }

    let next_fn = my_fn_id.as_deref().or(current_fn);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, src, ctx, next_fn, nodes, calls);
    }
}

fn text_of_field<'a>(node: TsNode, field: &str, src: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(&src[node.child_by_field_name(field)?.byte_range()]).ok()
}

fn trailing_ident(node: TsNode, src: &[u8]) -> Option<String> {
    let k = node.kind();
    if k == "identifier" || k.ends_with("_identifier") {
        return std::str::from_utf8(&src[node.byte_range()]).ok().map(|s| s.to_string());
    }
    for field in ["name", "field", "attribute", "property"] {
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

    fn names(pf: &ParsedFile) -> Vec<(&str, NodeLabel)> {
        pf.nodes.iter().map(|n| (n.name.as_str(), n.label)).collect()
    }

    #[test]
    fn rust_defs_and_calls() {
        let pf = parse_rust("p", "src/lib.rs", "fn helper() {}\nfn main() { helper(); }\nstruct S;\nenum E {}\ntrait T {}\n");
        assert!(names(&pf).contains(&("helper", NodeLabel::Function)));
        assert!(names(&pf).contains(&("S", NodeLabel::Class)));
        assert!(names(&pf).contains(&("T", NodeLabel::Interface)));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper" && c.caller_id.ends_with("main")));
    }

    #[test]
    fn python_defs_and_calls() {
        let pf = parse_python("p", "m.py", "def foo():\n    pass\nclass Bar:\n    def m(self):\n        foo()\n");
        assert!(names(&pf).contains(&("foo", NodeLabel::Function)));
        assert!(names(&pf).contains(&("Bar", NodeLabel::Class)));
        assert!(names(&pf).contains(&("m", NodeLabel::Function)));
        assert!(pf.calls.iter().any(|c| c.callee_name == "foo"));
    }

    #[test]
    fn javascript_defs() {
        let pf = parse_js("p", "a.js", "function foo(){}\nclass Bar { m(){ foo(); } }\n");
        assert!(names(&pf).contains(&("foo", NodeLabel::Function)));
        assert!(names(&pf).contains(&("Bar", NodeLabel::Class)));
        assert!(names(&pf).contains(&("m", NodeLabel::Method)));
        assert!(pf.calls.iter().any(|c| c.callee_name == "foo"));
    }

    #[test]
    fn typescript_interface_and_type() {
        let pf = parse_ts("p", "a.ts", "interface I { x: number }\ntype Alias = string;\nfunction f(): I { return {x:1}; }\n");
        assert!(names(&pf).contains(&("I", NodeLabel::Interface)));
        assert!(names(&pf).contains(&("Alias", NodeLabel::Type)));
        assert!(names(&pf).contains(&("f", NodeLabel::Function)));
    }

    #[test]
    fn go_defs_and_calls() {
        let pf = parse_go("p", "main.go", "package main\nfunc helper() {}\nfunc main() { helper() }\ntype S struct{}\n");
        assert!(names(&pf).contains(&("helper", NodeLabel::Function)));
        assert!(names(&pf).contains(&("main", NodeLabel::Function)));
        assert!(names(&pf).contains(&("S", NodeLabel::Class)));
        assert!(pf.calls.iter().any(|c| c.callee_name == "helper" && c.caller_id.ends_with("main")));
    }

    #[test]
    fn dispatch_by_extension() {
        assert!(!parse_file("p", "x.py", "def a():\n    pass\n").nodes.is_empty());
        assert!(!parse_file("p", "x.go", "package m\nfunc a(){}\n").nodes.is_empty());
        assert!(parse_file("p", "x.unknown", "whatever").nodes.is_empty());
    }
}
