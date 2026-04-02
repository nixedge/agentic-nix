/// A named symbol extracted from source code via tree-sitter.
#[allow(dead_code)]
pub struct Symbol {
    pub name: String,
    pub kind: String, // "function", "class", "struct", "enum", "trait", "type", "const", "impl"
    pub content: String,
    pub start_line: usize, // 1-indexed
    pub end_line: usize,   // 1-indexed
}

const MAX_CHUNK_BYTES: usize = 8_000;

/// Dispatch to per-language symbol extractors.
/// Returns empty vec for unsupported languages or parse failures.
pub fn extract_symbols(source: &str, language: &str) -> Vec<Symbol> {
    let bytes = source.as_bytes();
    let lines: Vec<&str> = source.lines().collect();

    let syms = match language {
        "typescript" | "javascript" => extract_ts_symbols(bytes, &lines),
        "python" => extract_py_symbols(bytes, &lines),
        "rust" => extract_rust_symbols(bytes, &lines),
        "haskell" => extract_haskell_symbols(bytes, &lines),
        "latex" => extract_latex_symbols(bytes, &lines),
        "nix" => extract_nix_symbols(bytes, &lines),
        _ => vec![],
    };

    // Drop chunks that are too large for embedding
    syms.into_iter()
        .filter(|s| s.content.len() <= MAX_CHUNK_BYTES)
        .collect()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_parser(lang: tree_sitter::Language) -> Option<tree_sitter::Parser> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).ok()?;
    Some(parser)
}

fn node_name<'a>(node: &tree_sitter::Node<'a>, source: &'a [u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(|s| s.to_string())
}

fn lines_slice(lines: &[&str], start: usize, end: usize) -> String {
    let end = end.min(lines.len().saturating_sub(1));
    lines[start..=end].join("\n")
}

/// Walk backward from `symbol_start` collecting preceding `///` doc comments
/// and `#[...]` attribute lines (Rust style).
fn rust_doc_start(lines: &[&str], symbol_start: usize) -> usize {
    let mut start = symbol_start;
    while start > 0 {
        let line = lines[start - 1].trim_start();
        if line.starts_with("///")
            || line.starts_with("#[")
            || line.starts_with("//!")
            || line == "#"
        {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// Walk backward from `symbol_start` collecting preceding Haddock comments.
fn haddock_start(lines: &[&str], symbol_start: usize) -> usize {
    let mut start = symbol_start;
    while start > 0 {
        let line = lines[start - 1].trim_start();
        if line.starts_with("--") || line.starts_with("{-") {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

// ── TypeScript / JavaScript ───────────────────────────────────────────────────

fn extract_ts_symbols(source: &[u8], lines: &[&str]) -> Vec<Symbol> {
    let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    let mut parser = match make_parser(lang) {
        Some(p) => p,
        None => return vec![],
    };
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    let mut syms = vec![];
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        visit_ts_node(&child, source, lines, &mut syms);
    }
    syms
}

fn visit_ts_node(
    node: &tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    syms: &mut Vec<Symbol>,
) {
    match node.kind() {
        // Unwrap export / ambient wrappers
        "export_statement" | "ambient_declaration" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                visit_ts_node(&child, source, lines, syms);
            }
        }
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = node_name(node, source) {
                emit_ts(node, name, "function", lines, syms);
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = node_name(node, source) {
                emit_ts(node, name, "class", lines, syms);
            }
        }
        "interface_declaration" => {
            if let Some(name) = node_name(node, source) {
                emit_ts(node, name, "interface", lines, syms);
            }
        }
        "type_alias_declaration" => {
            if let Some(name) = node_name(node, source) {
                emit_ts(node, name, "type", lines, syms);
            }
        }
        "enum_declaration" => {
            if let Some(name) = node_name(node, source) {
                emit_ts(node, name, "enum", lines, syms);
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            // e.g. `const foo = () => ...` or `const MyClass = class { ... }`
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name) = node_name(&child, source) {
                        emit_ts(node, name, "function", lines, syms);
                    }
                }
            }
        }
        _ => {}
    }
}

fn emit_ts(
    node: &tree_sitter::Node<'_>,
    name: String,
    kind: &str,
    lines: &[&str],
    syms: &mut Vec<Symbol>,
) {
    let start = node.start_position().row;
    let end = node.end_position().row;
    let content = lines_slice(lines, start, end);
    if !content.trim().is_empty() {
        syms.push(Symbol {
            name,
            kind: kind.to_string(),
            content,
            start_line: start + 1,
            end_line: end + 1,
        });
    }
}

// ── Python ────────────────────────────────────────────────────────────────────

fn extract_py_symbols(source: &[u8], lines: &[&str]) -> Vec<Symbol> {
    let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    let mut parser = match make_parser(lang) {
        Some(p) => p,
        None => return vec![],
    };
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    let mut syms = vec![];
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        visit_py_node(&child, source, lines, &mut syms);
    }
    syms
}

fn visit_py_node(
    node: &tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    syms: &mut Vec<Symbol>,
) {
    match node.kind() {
        "function_definition" => {
            if let Some(name) = node_name(node, source) {
                let start = node.start_position().row;
                let end = node.end_position().row;
                syms.push(Symbol {
                    name,
                    kind: "function".into(),
                    content: lines_slice(lines, start, end),
                    start_line: start + 1,
                    end_line: end + 1,
                });
            }
        }
        "class_definition" => {
            if let Some(name) = node_name(node, source) {
                let start = node.start_position().row;
                let end = node.end_position().row;
                syms.push(Symbol {
                    name,
                    kind: "class".into(),
                    content: lines_slice(lines, start, end),
                    start_line: start + 1,
                    end_line: end + 1,
                });
            }
        }
        "decorated_definition" => {
            // Unwrap: decorator + inner function/class
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if matches!(child.kind(), "function_definition" | "class_definition") {
                    if let Some(name) = node_name(&child, source) {
                        let start = node.start_position().row; // include decorators
                        let end = node.end_position().row;
                        let kind = if child.kind() == "class_definition" {
                            "class"
                        } else {
                            "function"
                        };
                        syms.push(Symbol {
                            name,
                            kind: kind.into(),
                            content: lines_slice(lines, start, end),
                            start_line: start + 1,
                            end_line: end + 1,
                        });
                    }
                    break;
                }
            }
        }
        _ => {}
    }
}

// ── Rust ──────────────────────────────────────────────────────────────────────

fn extract_rust_symbols(source: &[u8], lines: &[&str]) -> Vec<Symbol> {
    let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = match make_parser(lang) {
        Some(p) => p,
        None => return vec![],
    };
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    let mut syms = vec![];
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        visit_rust_node(&child, source, lines, &mut syms, None);
    }
    syms
}

fn visit_rust_node(
    node: &tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    syms: &mut Vec<Symbol>,
    impl_type: Option<&str>,
) {
    match node.kind() {
        "function_item" => {
            if let Some(raw_name) = node_name(node, source) {
                let name = match impl_type {
                    Some(t) => format!("{t}::{raw_name}"),
                    None => raw_name,
                };
                let start = node.start_position().row;
                let end = node.end_position().row;
                let doc_start = rust_doc_start(lines, start);
                syms.push(Symbol {
                    name,
                    kind: "function".into(),
                    content: lines_slice(lines, doc_start, end),
                    start_line: doc_start + 1,
                    end_line: end + 1,
                });
            }
        }
        "struct_item" => {
            if let Some(name) = node_name(node, source) {
                let start = node.start_position().row;
                let end = node.end_position().row;
                let doc_start = rust_doc_start(lines, start);
                syms.push(Symbol {
                    name,
                    kind: "struct".into(),
                    content: lines_slice(lines, doc_start, end),
                    start_line: doc_start + 1,
                    end_line: end + 1,
                });
            }
        }
        "enum_item" => {
            if let Some(name) = node_name(node, source) {
                let start = node.start_position().row;
                let end = node.end_position().row;
                let doc_start = rust_doc_start(lines, start);
                syms.push(Symbol {
                    name,
                    kind: "enum".into(),
                    content: lines_slice(lines, doc_start, end),
                    start_line: doc_start + 1,
                    end_line: end + 1,
                });
            }
        }
        "trait_item" => {
            if let Some(name) = node_name(node, source) {
                let start = node.start_position().row;
                let end = node.end_position().row;
                let doc_start = rust_doc_start(lines, start);
                syms.push(Symbol {
                    name,
                    kind: "trait".into(),
                    content: lines_slice(lines, doc_start, end),
                    start_line: doc_start + 1,
                    end_line: end + 1,
                });
            }
        }
        "type_item" => {
            if let Some(name) = node_name(node, source) {
                let start = node.start_position().row;
                let end = node.end_position().row;
                let doc_start = rust_doc_start(lines, start);
                syms.push(Symbol {
                    name,
                    kind: "type".into(),
                    content: lines_slice(lines, doc_start, end),
                    start_line: doc_start + 1,
                    end_line: end + 1,
                });
            }
        }
        "const_item" => {
            if let Some(name) = node_name(node, source) {
                let start = node.start_position().row;
                let end = node.end_position().row;
                let doc_start = rust_doc_start(lines, start);
                syms.push(Symbol {
                    name,
                    kind: "const".into(),
                    content: lines_slice(lines, doc_start, end),
                    start_line: doc_start + 1,
                    end_line: end + 1,
                });
            }
        }
        "impl_item" => {
            // Extract type name for method qualification
            let type_name = node
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source).ok())
                .map(|s| s.split('<').next().unwrap_or(s).trim().to_string()) // strip generics
                .unwrap_or_else(|| "Unknown".to_string());

            // Recurse into the impl body to extract methods
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    visit_rust_node(&child, source, lines, syms, Some(&type_name));
                }
            }
        }
        // Unwrap visibility / attribute wrappers
        "mod_item" => {
            // Only recurse into inline mods (those with a body)
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    visit_rust_node(&child, source, lines, syms, impl_type);
                }
            }
        }
        _ => {}
    }
}

// ── Haskell ───────────────────────────────────────────────────────────────────

/// Lightweight declaration info for the grouping pass.
struct HsDecl {
    kind: String,
    name: Option<String>,
    start_row: usize,
    end_row: usize,
}

fn extract_haskell_symbols(source: &[u8], lines: &[&str]) -> Vec<Symbol> {
    let lang: tree_sitter::Language = tree_sitter_haskell::LANGUAGE.into();
    let mut parser = match make_parser(lang) {
        Some(p) => p,
        None => return vec![],
    };
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    let root = tree.root_node();

    // Collect declarations (two-pass to avoid lifetime issues)
    let decls = collect_hs_decls(&root, source);
    group_haskell_decls(&decls, lines)
}

fn collect_hs_decls(root: &tree_sitter::Node<'_>, source: &[u8]) -> Vec<HsDecl> {
    let mut decls = vec![];

    // Some grammar versions wrap children in a `declarations` field; try both.
    let container = root.child_by_field_name("declarations");
    let iter_node = container.as_ref().unwrap_or(root);

    collect_hs_decls_from(iter_node, source, &mut decls);
    decls
}

fn collect_hs_decls_from(node: &tree_sitter::Node<'_>, source: &[u8], decls: &mut Vec<HsDecl>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "cpp" {
            // CPP conditional blocks (#if/#else/#endif) wrap declarations as children;
            // recurse so instances/functions inside them are not silently dropped.
            collect_hs_decls_from(&child, source, decls);
        } else {
            let name = hs_node_name(&child, source);
            decls.push(HsDecl {
                kind: child.kind().to_string(),
                name,
                start_row: child.start_position().row,
                end_row: child.end_position().row,
            });
        }
    }
}

/// Extract name from a Haskell declaration node.
fn hs_node_name(node: &tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    // Try the `name` field first (works for many declaration types)
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(s) = n.utf8_text(source) {
            return Some(s.to_string());
        }
    }
    // Fall back: first named child that is a variable or constructor
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable" | "constructor" | "operator" => {
                if let Ok(s) = child.utf8_text(source) {
                    return Some(s.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

// ── LaTeX ─────────────────────────────────────────────────────────────────────

fn extract_latex_symbols(source: &[u8], lines: &[&str]) -> Vec<Symbol> {
    let lang: tree_sitter::Language = codebook_tree_sitter_latex::LANGUAGE.into();
    let mut parser = match make_parser(lang) {
        Some(p) => p,
        None => return vec![],
    };
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    let mut syms = vec![];
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        visit_latex_node(&child, source, lines, &mut syms);
    }
    syms
}

fn visit_latex_node(
    node: &tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    syms: &mut Vec<Symbol>,
) {
    match node.kind() {
        kind @ ("chapter" | "section" | "subsection" | "subsubsection" | "paragraph") => {
            let start = node.start_position().row;
            let end = node.end_position().row;
            let title = latex_section_title(node, source)
                .unwrap_or_else(|| format!("{}@L{}", kind, start + 1));
            syms.push(Symbol {
                name: title,
                kind: kind.to_string(),
                content: lines_slice(lines, start, end),
                start_line: start + 1,
                end_line: end + 1,
            });
            // Recurse to extract nested sections, figures, and math
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                visit_latex_node(&child, source, lines, syms);
            }
        }
        "generic_environment" => {
            let env = latex_env_name(node, source).unwrap_or_default();
            let start = node.start_position().row;
            let end = node.end_position().row;
            let content = lines_slice(lines, start, end);
            match env.trim_end_matches('*') {
                "figure" | "table" => {
                    let name = latex_caption_text(node, source)
                        .or_else(|| latex_label_text(node, source))
                        .unwrap_or_else(|| format!("{}@L{}", env, start + 1));
                    syms.push(Symbol {
                        name,
                        kind: "figure".to_string(),
                        content,
                        start_line: start + 1,
                        end_line: end + 1,
                    });
                }
                "theorem" | "lemma" | "definition" | "corollary" | "proposition" | "proof"
                | "remark" | "example" | "conjecture" | "axiom" | "claim" | "observation" => {
                    let name = latex_label_text(node, source)
                        .unwrap_or_else(|| format!("{}@L{}", env, start + 1));
                    syms.push(Symbol {
                        name,
                        kind: env.trim_end_matches('*').to_string(),
                        content,
                        start_line: start + 1,
                        end_line: end + 1,
                    });
                }
                _ => {
                    // Recurse into unknown environments for nested figures/math
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        visit_latex_node(&child, source, lines, syms);
                    }
                }
            }
        }
        "displayed_equation" | "math_environment" => {
            let start = node.start_position().row;
            let end = node.end_position().row;
            if end > start {
                syms.push(Symbol {
                    name: format!("equation@L{}", start + 1),
                    kind: "equation".to_string(),
                    content: lines_slice(lines, start, end),
                    start_line: start + 1,
                    end_line: end + 1,
                });
            }
        }
        _ => {}
    }
}

fn latex_section_title(node: &tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let text_node = node.child_by_field_name("text")?;
    let raw = text_node.utf8_text(source).ok()?;
    Some(
        raw.trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .trim()
            .to_string(),
    )
}

fn latex_env_name(node: &tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let begin = node.child_by_field_name("begin")?;
    let name_node = begin.child_by_field_name("name")?;
    let raw = name_node.utf8_text(source).ok()?;
    Some(
        raw.trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .trim()
            .to_string(),
    )
}

fn latex_label_text(node: &tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "label_definition" {
            if let Some(name_node) = child.child_by_field_name("name") {
                if let Ok(raw) = name_node.utf8_text(source) {
                    return Some(
                        raw.trim()
                            .trim_start_matches('{')
                            .trim_end_matches('}')
                            .trim()
                            .to_string(),
                    );
                }
            }
        }
    }
    None
}

fn latex_caption_text(node: &tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "caption" {
            if let Some(long_node) = child.child_by_field_name("long") {
                if let Ok(raw) = long_node.utf8_text(source) {
                    return Some(
                        raw.trim()
                            .trim_start_matches('{')
                            .trim_end_matches('}')
                            .trim()
                            .to_string(),
                    );
                }
            }
        }
    }
    None
}

// ── Nix ───────────────────────────────────────────────────────────────────────

/// Extract top-level attribute bindings from a Nix expression.
///
/// Nix files are typically one of:
///   - A function:   `{ pkgs, ... }: { foo = ...; bar = ...; }`
///   - An attr set:  `{ foo = ...; bar = ...; }`
///   - A `let` expression followed by either of the above
///
/// We walk into function bodies and let-in expressions to find the innermost
/// attr set, then emit each `binding` as a symbol.
fn extract_nix_symbols(source: &[u8], lines: &[&str]) -> Vec<Symbol> {
    let lang: tree_sitter::Language = tree_sitter_nix::LANGUAGE.into();
    let mut parser = match make_parser(lang) {
        Some(p) => p,
        None => return vec![],
    };
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return vec![],
    };

    let mut syms = vec![];
    let root = tree.root_node();
    // source_expression has a single child
    if let Some(expr) = root.named_child(0) {
        collect_nix_bindings(&expr, source, lines, &mut syms, 0);
    }
    syms
}

/// Recurse into functions / let-in / with to reach binding sets, then emit bindings.
fn collect_nix_bindings(
    node: &tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    syms: &mut Vec<Symbol>,
    depth: usize,
) {
    // Avoid recursing too deep into nested attr sets
    if depth > 3 {
        return;
    }
    match node.kind() {
        // `arg: body` or `{ args }: body` — descend into body
        "function" => {
            if let Some(body) = node.child_by_field_name("body") {
                collect_nix_bindings(&body, source, lines, syms, depth);
            }
        }
        // `let bindings in body` — emit let bindings + descend into body
        "let_expression" => {
            // emit the let bindings themselves at this depth
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "binding" {
                    emit_nix_binding(&child, source, lines, syms);
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                collect_nix_bindings(&body, source, lines, syms, depth);
            }
        }
        // `with expr; body`
        "with_expression" => {
            if let Some(body) = node.child_by_field_name("body") {
                collect_nix_bindings(&body, source, lines, syms, depth);
            }
        }
        // `{ ... }` or `rec { ... }` — emit each binding.
        // Some grammar versions wrap bindings in a `binding_set` node; handle both.
        "attrset_expression" | "rec_attrset_expression" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "binding" {
                    emit_nix_binding(&child, source, lines, syms);
                } else if child.kind() == "binding_set" {
                    let mut c2 = child.walk();
                    for binding in child.named_children(&mut c2) {
                        if binding.kind() == "binding" {
                            emit_nix_binding(&binding, source, lines, syms);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn emit_nix_binding(
    node: &tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    syms: &mut Vec<Symbol>,
) {
    // The attrpath is the binding's name: `foo`, `foo.bar`, `"foo"`, etc.
    let name = node
        .child_by_field_name("attrpath")
        .and_then(|n| n.utf8_text(source).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let start = node.start_position().row;
    let end = node.end_position().row;
    let content = lines_slice(lines, start, end);
    if content.trim().is_empty() {
        return;
    }

    syms.push(Symbol {
        name,
        kind: "binding".to_string(),
        content,
        start_line: start + 1,
        end_line: end + 1,
    });
}

fn group_haskell_decls(decls: &[HsDecl], lines: &[&str]) -> Vec<Symbol> {
    let mut syms = vec![];
    let mut i = 0;

    while i < decls.len() {
        let d = &decls[i];

        match d.kind.as_str() {
            "signature" => {
                let sig_name = match &d.name {
                    Some(n) => n.clone(),
                    None => {
                        i += 1;
                        continue;
                    }
                };
                let mut group_end = d.end_row;
                let mut j = i + 1;
                // Consume consecutive function equations or value bindings with the same name.
                // `function` covers `f x = ...` style; `bind` covers `f = expr` style.
                while j < decls.len() {
                    let next = &decls[j];
                    if (next.kind == "function" || next.kind == "bind")
                        && next.name.as_deref() == Some(sig_name.as_str())
                    {
                        group_end = next.end_row;
                        j += 1;
                    } else {
                        break;
                    }
                }
                let doc_start = haddock_start(lines, d.start_row);
                let content = lines_slice(lines, doc_start, group_end);
                if !content.trim().is_empty() {
                    syms.push(Symbol {
                        name: sig_name,
                        kind: "function".into(),
                        content,
                        start_line: doc_start + 1,
                        end_line: group_end + 1,
                    });
                }
                i = j;
            }
            "function" | "bind" => {
                // Orphan function/binding (no preceding signature)
                let fn_name = match &d.name {
                    Some(n) => n.clone(),
                    None => {
                        i += 1;
                        continue;
                    }
                };
                let mut group_end = d.end_row;
                let mut j = i + 1;
                while j < decls.len() {
                    let next = &decls[j];
                    if (next.kind == "function" || next.kind == "bind")
                        && next.name.as_deref() == Some(fn_name.as_str())
                    {
                        group_end = next.end_row;
                        j += 1;
                    } else {
                        break;
                    }
                }
                let doc_start = haddock_start(lines, d.start_row);
                let content = lines_slice(lines, doc_start, group_end);
                if !content.trim().is_empty() {
                    syms.push(Symbol {
                        name: fn_name,
                        kind: "function".into(),
                        content,
                        start_line: doc_start + 1,
                        end_line: group_end + 1,
                    });
                }
                i = j;
            }
            // tree-sitter-haskell spells the node kind "type_synomym" (sic).
            "type_synonym" | "type_synomym" | "type_alias" => {
                if let Some(name) = &d.name {
                    let doc_start = haddock_start(lines, d.start_row);
                    syms.push(Symbol {
                        name: name.clone(),
                        kind: "type".into(),
                        content: lines_slice(lines, doc_start, d.end_row),
                        start_line: doc_start + 1,
                        end_line: d.end_row + 1,
                    });
                }
                i += 1;
            }
            "data_type" | "newtype" | "newtype_type" => {
                if let Some(name) = &d.name {
                    let doc_start = haddock_start(lines, d.start_row);
                    syms.push(Symbol {
                        name: name.clone(),
                        kind: "struct".into(),
                        content: lines_slice(lines, doc_start, d.end_row),
                        start_line: doc_start + 1,
                        end_line: d.end_row + 1,
                    });
                }
                i += 1;
            }
            "class_declaration" | "class" => {
                let name = d
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("class@L{}", d.start_row + 1));
                let doc_start = haddock_start(lines, d.start_row);
                syms.push(Symbol {
                    name,
                    kind: "class".into(),
                    content: lines_slice(lines, doc_start, d.end_row),
                    start_line: doc_start + 1,
                    end_line: d.end_row + 1,
                });
                i += 1;
            }
            "instance_declaration" | "instance" => {
                // Instances don't have a simple name; use whatever we can get
                let name = d
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("instance@L{}", d.start_row + 1));
                let doc_start = haddock_start(lines, d.start_row);
                syms.push(Symbol {
                    name,
                    kind: "impl".into(),
                    content: lines_slice(lines, doc_start, d.end_row),
                    start_line: doc_start + 1,
                    end_line: d.end_row + 1,
                });
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    syms
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ────────────────────────────────────────────────────────────────

    fn sym(lang: &str, src: &str) -> Vec<Symbol> {
        extract_symbols(src, lang)
    }
    fn hs(src: &str) -> Vec<Symbol> {
        sym("haskell", src)
    }
    fn rs(src: &str) -> Vec<Symbol> {
        sym("rust", src)
    }
    fn py(src: &str) -> Vec<Symbol> {
        sym("python", src)
    }
    fn ts(src: &str) -> Vec<Symbol> {
        sym("typescript", src)
    }
    fn nix(src: &str) -> Vec<Symbol> {
        sym("nix", src)
    }

    fn names(syms: &[Symbol]) -> Vec<&str> {
        syms.iter().map(|s| s.name.as_str()).collect()
    }
    fn find<'a>(syms: &'a [Symbol], kind: &str, name_pat: &str) -> Option<&'a Symbol> {
        syms.iter()
            .find(|s| s.kind == kind && s.name.contains(name_pat))
    }

    // ── unsupported language falls back to empty (no crash) ───────────────────

    #[test]
    fn unknown_language_returns_empty() {
        assert!(sym("cobol", "some source").is_empty());
        assert!(sym("", "some source").is_empty());
    }

    #[test]
    fn empty_source_returns_empty_for_all_languages() {
        for lang in &["haskell", "rust", "python", "typescript", "nix"] {
            assert!(
                sym(lang, "").is_empty(),
                "language {lang} should return empty for empty source"
            );
        }
    }

    // ── helpers: rust_doc_start / haddock_start ────────────────────────────────

    #[test]
    fn rust_doc_start_includes_doc_comments() {
        let lines = vec!["", "/// Does a thing", "#[inline]", "fn foo() {}"];
        assert_eq!(rust_doc_start(&lines, 3), 1); // starts at the doc comment
    }

    #[test]
    fn rust_doc_start_stops_at_non_doc_line() {
        let lines = vec!["let x = 1;", "/// comment", "fn foo() {}"];
        assert_eq!(rust_doc_start(&lines, 2), 1);
    }

    #[test]
    fn haddock_start_includes_haddock_comments() {
        let lines = vec!["", "-- | docs", "-- more", "instance Foo Bar where"];
        assert_eq!(haddock_start(&lines, 3), 1);
    }

    #[test]
    fn haddock_start_stops_at_non_comment() {
        let lines = vec!["x = 1", "-- | docs", "foo = bar"];
        assert_eq!(haddock_start(&lines, 2), 1);
    }

    // ══ Haskell ════════════════════════════════════════════════════════════════

    #[test]
    fn hs_extracts_top_level_function() {
        let src = "module Foo where\n\nfoo :: Int -> Int\nfoo x = x + 1\n";
        let syms = hs(src);
        assert!(
            names(&syms).contains(&"foo"),
            "expected 'foo'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn hs_extracts_instance_declaration() {
        let src = "module Foo where\n\ninstance Show () where\n    show _ = \"()\"\n";
        let syms = hs(src);
        let inst = syms
            .iter()
            .find(|s| s.kind == "impl" && s.content.contains("Show ()"));
        assert!(
            inst.is_some(),
            "expected impl chunk for 'instance Show ()'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn hs_extracts_class_declaration() {
        let src = "module Foo where\n\nclass MyClass a where\n    method :: a -> String\n";
        let syms = hs(src);
        let cls = syms.iter().find(|s| s.kind == "class");
        assert!(
            cls.is_some(),
            "expected a class chunk; got {:?}",
            names(&syms)
        );
        assert!(cls.unwrap().name.contains("MyClass"));
    }

    #[test]
    fn hs_extracts_data_type() {
        let src = "module Foo where\n\ndata Color = Red | Green | Blue\n";
        let syms = hs(src);
        assert!(
            find(&syms, "struct", "Color").is_some(),
            "expected struct 'Color'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn hs_extracts_newtype() {
        let src = "module Foo where\n\nnewtype Wrapper a = Wrapper { unwrap :: a }\n";
        let syms = hs(src);
        assert!(
            find(&syms, "struct", "Wrapper").is_some(),
            "expected struct 'Wrapper'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn hs_extracts_type_alias() {
        let src = "module Foo where\n\ntype Name = String\n";
        let syms = hs(src);
        assert!(
            find(&syms, "type", "Name").is_some(),
            "expected type 'Name'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn hs_signature_and_body_are_grouped() {
        let src = "module Foo where\n\nbar :: Int -> Int\nbar x = x * 2\n";
        let syms = hs(src);
        let bars: Vec<_> = syms.iter().filter(|s| s.name == "bar").collect();
        assert_eq!(
            bars.len(),
            1,
            "signature and body should merge into one symbol"
        );
        assert!(bars[0].content.contains("bar :: Int -> Int"));
        assert!(bars[0].content.contains("bar x = x * 2"));
    }

    #[test]
    fn hs_multiequation_function_grouped() {
        let src = "module Foo where\n\nf :: Bool -> Int\nf True  = 1\nf False = 0\n";
        let syms = hs(src);
        let fs: Vec<_> = syms.iter().filter(|s| s.name == "f").collect();
        assert_eq!(fs.len(), 1, "multi-equation function should be one symbol");
        assert!(fs[0].content.contains("f True"));
        assert!(fs[0].content.contains("f False"));
    }

    #[test]
    fn hs_haddock_comment_included_in_content() {
        let src = "module Foo where\n\n-- | Adds one.\nadd1 :: Int -> Int\nadd1 x = x + 1\n";
        let syms = hs(src);
        let s = syms
            .iter()
            .find(|s| s.name == "add1")
            .expect("expected 'add1'");
        assert!(
            s.content.contains("-- | Adds one."),
            "haddock comment should be in content"
        );
    }

    #[test]
    fn hs_line_numbers_are_1_indexed() {
        let src = "module Foo where\n\nfoo :: Int\nfoo = 42\n";
        let syms = hs(src);
        let foo = syms
            .iter()
            .find(|s| s.name == "foo")
            .expect("expected 'foo'");
        assert!(foo.start_line >= 1);
        assert!(foo.end_line >= foo.start_line);
    }

    #[test]
    fn hs_instance_without_name_gets_fallback() {
        // Instances always get a name (possibly auto-generated) and are never silently dropped.
        let src = "module Foo where\n\ninstance Show () where\n    show _ = \"()\"\n";
        let syms = hs(src);
        assert!(
            syms.iter().any(|s| s.kind == "impl"),
            "expected at least one impl chunk"
        );
    }

    // ── CPP ───────────────────────────────────────────────────────────────────

    #[test]
    fn hs_cpp_wrapped_instance_is_not_dropped() {
        let src = r#"module Foo where

#if MIN_VERSION_base(4,10,0)
instance Show Foo where
    show _ = "new"
#else
instance Show Foo where
    show _ = "old"
#endif
"#;
        let syms = hs(src);
        let inst = syms
            .iter()
            .find(|s| s.kind == "impl" && s.content.contains("Show Foo"));
        assert!(
            inst.is_some(),
            "instance inside #if block was silently dropped; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn hs_cpp_nested_blocks_are_handled() {
        let src = r#"module Foo where

#if COND_A
#if COND_B
instance Show A where
    show _ = "A"
#endif
#endif
"#;
        let syms = hs(src);
        let inst = syms
            .iter()
            .find(|s| s.kind == "impl" && s.content.contains("Show A"));
        assert!(
            inst.is_some(),
            "instance inside nested #if blocks was dropped; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn hs_cpp_function_outside_block_still_extracted() {
        // Declarations outside CPP blocks must not be affected by the recursion change.
        let src = r#"module Foo where

plain :: Int
plain = 1

#if SOME_FLAG
flagged :: Int
flagged = 2
#endif
"#;
        let syms = hs(src);
        assert!(
            names(&syms).contains(&"plain"),
            "non-CPP function missing; got {:?}",
            names(&syms)
        );
    }

    // ══ Rust ══════════════════════════════════════════════════════════════════

    #[test]
    fn rs_extracts_function() {
        let src = "fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let syms = rs(src);
        assert!(
            find(&syms, "function", "add").is_some(),
            "expected fn 'add'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn rs_extracts_struct() {
        let src = "struct Point { x: f64, y: f64 }\n";
        let syms = rs(src);
        assert!(
            find(&syms, "struct", "Point").is_some(),
            "expected struct 'Point'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn rs_extracts_enum() {
        let src = "enum Direction { North, South, East, West }\n";
        let syms = rs(src);
        assert!(
            find(&syms, "enum", "Direction").is_some(),
            "expected enum 'Direction'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn rs_extracts_trait() {
        let src = "trait Animal { fn speak(&self) -> &str; }\n";
        let syms = rs(src);
        assert!(
            find(&syms, "trait", "Animal").is_some(),
            "expected trait 'Animal'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn rs_extracts_type_alias() {
        let src = "type Meters = f64;\n";
        let syms = rs(src);
        assert!(
            find(&syms, "type", "Meters").is_some(),
            "expected type 'Meters'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn rs_extracts_const() {
        let src = "const MAX: usize = 100;\n";
        let syms = rs(src);
        assert!(
            find(&syms, "const", "MAX").is_some(),
            "expected const 'MAX'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn rs_impl_methods_are_qualified() {
        let src = "struct Foo;\nimpl Foo {\n    fn bar(&self) {}\n}\n";
        let syms = rs(src);
        assert!(
            find(&syms, "function", "Foo::bar").is_some(),
            "expected 'Foo::bar'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn rs_doc_comment_included_in_content() {
        let src = "/// Computes something.\nfn compute() -> i32 { 42 }\n";
        let syms = rs(src);
        let s = syms
            .iter()
            .find(|s| s.name == "compute")
            .expect("expected 'compute'");
        assert!(s.content.contains("/// Computes something."));
    }

    #[test]
    fn rs_inline_mod_items_extracted() {
        let src = "mod inner {\n    pub fn helper() {}\n}\n";
        let syms = rs(src);
        assert!(
            find(&syms, "function", "helper").is_some(),
            "fn inside inline mod should be extracted; got {:?}",
            names(&syms)
        );
    }

    // ══ Python ════════════════════════════════════════════════════════════════

    #[test]
    fn py_extracts_function() {
        let src = "def greet(name):\n    return 'Hello ' + name\n";
        let syms = py(src);
        assert!(
            find(&syms, "function", "greet").is_some(),
            "expected fn 'greet'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn py_extracts_class() {
        let src = "class Animal:\n    def speak(self):\n        pass\n";
        let syms = py(src);
        assert!(
            find(&syms, "class", "Animal").is_some(),
            "expected class 'Animal'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn py_decorated_function_extracted() {
        let src = "@staticmethod\ndef util():\n    pass\n";
        let syms = py(src);
        assert!(
            find(&syms, "function", "util").is_some(),
            "decorated function should be extracted; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn py_decorated_class_extracted() {
        let src = "@dataclass\nclass Point:\n    x: float\n    y: float\n";
        let syms = py(src);
        assert!(
            find(&syms, "class", "Point").is_some(),
            "decorated class should be extracted; got {:?}",
            names(&syms)
        );
    }

    // ══ TypeScript ════════════════════════════════════════════════════════════

    #[test]
    fn ts_extracts_function() {
        let src = "function greet(name: string): string { return 'hi ' + name; }\n";
        let syms = ts(src);
        assert!(
            find(&syms, "function", "greet").is_some(),
            "expected fn 'greet'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn ts_extracts_class() {
        let src = "class Dog {\n  bark() { console.log('woof'); }\n}\n";
        let syms = ts(src);
        assert!(
            find(&syms, "class", "Dog").is_some(),
            "expected class 'Dog'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn ts_extracts_interface() {
        let src = "interface Shape { area(): number; }\n";
        let syms = ts(src);
        assert!(
            find(&syms, "interface", "Shape").is_some(),
            "expected interface 'Shape'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn ts_extracts_type_alias() {
        let src = "type Id = string | number;\n";
        let syms = ts(src);
        assert!(
            find(&syms, "type", "Id").is_some(),
            "expected type 'Id'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn ts_extracts_enum() {
        let src = "enum Color { Red, Green, Blue }\n";
        let syms = ts(src);
        assert!(
            find(&syms, "enum", "Color").is_some(),
            "expected enum 'Color'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn ts_exported_function_extracted() {
        let src = "export function foo(): void {}\n";
        let syms = ts(src);
        assert!(
            find(&syms, "function", "foo").is_some(),
            "exported fn should be extracted; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn ts_const_arrow_extracted() {
        let src = "const handler = (x: number) => x * 2;\n";
        let syms = ts(src);
        assert!(
            find(&syms, "function", "handler").is_some(),
            "const arrow fn should be extracted; got {:?}",
            names(&syms)
        );
    }

    // ══ Nix ═══════════════════════════════════════════════════════════════════

    #[test]
    fn nix_extracts_top_level_attr() {
        let src = "{ foo = 42; bar = \"hello\"; }\n";
        let syms = nix(src);
        // Nix bindings are emitted with kind "binding".
        assert!(
            find(&syms, "binding", "foo").is_some(),
            "expected binding 'foo'; got {:?}",
            names(&syms)
        );
        assert!(
            find(&syms, "binding", "bar").is_some(),
            "expected binding 'bar'; got {:?}",
            names(&syms)
        );
    }

    #[test]
    fn nix_extracts_function_binding() {
        let src = "{ add = x: y: x + y; }\n";
        let syms = nix(src);
        assert!(
            find(&syms, "binding", "add").is_some(),
            "expected binding 'add'; got {:?}",
            names(&syms)
        );
    }
}
