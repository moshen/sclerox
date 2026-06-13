use std::path::Path;
use tree_sitter::{Language, Node, Parser};

/// A parsed symbol extracted from source code.
#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub kind: String,
    pub name: String,
    pub signature: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
}

/// A chunk of source code for embedding.
#[derive(Debug, Clone)]
pub struct CodeChunk {
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Detect language from file extension.
/// Languages with a tree-sitter grammar get full symbol extraction.
/// Others fall back to line-based chunking (still indexed and searchable).
pub fn detect_language(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        // Tree-sitter backed (full symbol extraction)
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "cs" => Some("csharp"),
        // Line-based fallback (indexed but no symbol extraction)
        "java" => Some("java"),
        "cpp" | "cc" | "cxx" | "c" | "h" | "hpp" | "hxx" => Some("cpp"),
        "rb" => Some("ruby"),
        "swift" => Some("swift"),
        "kt" | "kts" => Some("kotlin"),
        "scala" => Some("scala"),
        "php" => Some("php"),
        "sh" | "bash" | "zsh" => Some("shell"),
        "sql" => Some("sql"),
        "md" | "mdx" => Some("markdown"),
        _ => None,
    }
}

fn get_tree_sitter_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "csharp" => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        // Everything else falls back to line-based chunking in parse_file
        _ => None,
    }
}

/// Parse a source file, extracting top-level symbols and code chunks.
/// Falls back to line-based chunking if no tree-sitter grammar is available.
pub fn parse_file(
    source: &str,
    language: &str,
    chunk_size_lines: usize,
) -> (Vec<ParsedSymbol>, Vec<CodeChunk>) {
    if let Some(ts_lang) = get_tree_sitter_language(language) {
        parse_with_tree_sitter(source, language, &ts_lang, chunk_size_lines)
    } else {
        (vec![], chunk_by_lines(source, chunk_size_lines))
    }
}

fn parse_with_tree_sitter(
    source: &str,
    language: &str,
    ts_lang: &Language,
    chunk_size_lines: usize,
) -> (Vec<ParsedSymbol>, Vec<CodeChunk>) {
    let mut parser = Parser::new();
    if parser.set_language(ts_lang).is_err() {
        return (vec![], chunk_by_lines(source, chunk_size_lines));
    }

    let tree = match parser.parse(source.as_bytes(), None) {
        Some(t) => t,
        None => return (vec![], chunk_by_lines(source, chunk_size_lines)),
    };

    let root = tree.root_node();
    let mut symbols = Vec::new();
    let mut chunks = Vec::new();

    collect_symbols(&root, source, language, &mut symbols, &mut chunks);

    // If no symbols found, fall back to line-based chunks
    if chunks.is_empty() {
        chunks = chunk_by_lines(source, chunk_size_lines);
    }

    (symbols, chunks)
}

fn collect_symbols(
    node: &Node,
    source: &str,
    language: &str,
    symbols: &mut Vec<ParsedSymbol>,
    chunks: &mut Vec<CodeChunk>,
) {
    let kind = node.kind();
    let is_symbol = match language {
        "rust" => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "impl_item"
                | "trait_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
        ),
        "python" => matches!(
            kind,
            "function_definition" | "class_definition" | "decorated_definition"
        ),
        "typescript" | "javascript" => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "method_definition"
                | "arrow_function"
                | "export_statement"
                | "lexical_declaration"
        ),
        "go" => matches!(
            kind,
            "function_declaration"
                | "method_declaration"
                | "type_declaration"
                | "var_declaration"
                | "const_declaration"
        ),
        "csharp" => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "struct_declaration"
                | "namespace_declaration"
                | "property_declaration"
        ),
        _ => false,
    };

    if is_symbol {
        let start = node.start_position();
        let end = node.end_position();
        let name = extract_name(node, source, language);
        let text = &source[node.start_byte()..node.end_byte()];
        let signature = extract_signature(text, language);

        symbols.push(ParsedSymbol {
            kind: kind.to_string(),
            name: name.unwrap_or_else(|| "<anonymous>".to_string()),
            signature,
            start_line: start.row as u32 + 1,
            end_line: end.row as u32 + 1,
        });

        // Use each top-level symbol as a chunk
        chunks.push(CodeChunk {
            text: text.to_string(),
            start_line: start.row as u32 + 1,
            end_line: end.row as u32 + 1,
        });
        return; // Don't recurse into the symbol's children for chunking
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(&child, source, language, symbols, chunks);
    }
}

fn extract_name(node: &Node, source: &str, language: &str) -> Option<String> {
    let name_kinds: &[&str] = match language {
        "rust" => &["identifier"],
        "python" => &["identifier"],
        "typescript" | "javascript" => &["identifier", "property_identifier"],
        "go" => &["identifier"],
        _ => &["identifier"],
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if name_kinds.contains(&child.kind()) {
            return Some(source[child.start_byte()..child.end_byte()].to_string());
        }
    }
    None
}

fn extract_signature(text: &str, language: &str) -> Option<String> {
    let sig = match language {
        "rust" => {
            // Take everything up to the first `{` or the whole thing if no brace
            text.lines()
                .next()
                .map(|l| l.trim_end_matches('{').trim().to_string())
        }
        "python" | "go" | "typescript" | "javascript" => text.lines().next().map(|l| l.to_string()),
        _ => None,
    };
    sig.filter(|s| !s.is_empty())
}

fn chunk_by_lines(source: &str, chunk_size: usize) -> Vec<CodeChunk> {
    let lines: Vec<&str> = source.lines().collect();
    let overlap = chunk_size / 4;
    let mut chunks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let end = (i + chunk_size).min(lines.len());
        let text = lines[i..end].join("\n");
        chunks.push(CodeChunk {
            text,
            start_line: i as u32 + 1,
            end_line: end as u32,
        });
        if end == lines.len() {
            break;
        }
        i += chunk_size - overlap;
    }
    chunks
}

/// Split a chunk that exceeds `max_chars` into smaller overlapping pieces.
/// Uses line boundaries to avoid splitting mid-statement.
/// `overlap_lines` controls how many lines are repeated between consecutive sub-chunks.
pub fn split_large_chunk(
    chunk: CodeChunk,
    max_chars: usize,
    overlap_lines: usize,
) -> Vec<CodeChunk> {
    if chunk.text.len() <= max_chars {
        return vec![chunk];
    }

    let lines: Vec<&str> = chunk.text.lines().collect();
    let mut result = Vec::new();
    let mut start = 0usize;

    while start < lines.len() {
        let mut char_count = 0usize;
        let mut end = start;

        // Accumulate lines until we hit the char budget
        while end < lines.len() {
            let line_chars = lines[end].len() + 1; // +1 for '\n'
            if char_count + line_chars > max_chars && end > start {
                break;
            }
            char_count += line_chars;
            end += 1;
        }

        if end == start {
            end += 1; // always include at least one line
        }

        result.push(CodeChunk {
            text: lines[start..end].join("\n"),
            start_line: chunk.start_line + start as u32,
            end_line: chunk.start_line + end as u32 - 1,
        });

        if end >= lines.len() {
            break;
        }

        // Always advance by at least 1 line. Without this guard, when a single
        // line is longer than max_chars the overlap calculation can send start
        // back to its previous position, creating an infinite loop that
        // allocates memory until the OOM killer terminates the process.
        let next = end.saturating_sub(overlap_lines);
        start = next.max(start + 1);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language(Path::new("main.rs")), Some("rust"));
        assert_eq!(detect_language(Path::new("app.py")), Some("python"));
        assert_eq!(detect_language(Path::new("index.ts")), Some("typescript"));
        assert_eq!(detect_language(Path::new("main.go")), Some("go"));
        assert_eq!(detect_language(Path::new("Program.cs")), Some("csharp"));
        assert_eq!(detect_language(Path::new("Main.java")), Some("java"));
        assert_eq!(detect_language(Path::new("readme.md")), Some("markdown"));
        assert_eq!(detect_language(Path::new("makefile")), None);
    }

    #[test]
    fn test_parse_rust_functions() {
        let source = r#"
fn hello(name: &str) -> String {
    format!("Hello, {}!", name)
}

struct Config {
    debug: bool,
    port: u16,
}

fn main() {
    println!("ok");
}
"#;
        let (symbols, _chunks) = parse_file(source, "rust", 50);
        assert!(!symbols.is_empty(), "should extract symbols");
        let fn_names: Vec<&str> = symbols
            .iter()
            .filter(|s| s.kind == "function_item")
            .map(|s| s.name.as_str())
            .collect();
        assert!(fn_names.contains(&"hello"), "missing fn hello");
        assert!(fn_names.contains(&"main"), "missing fn main");
    }

    #[test]
    fn test_parse_python_functions() {
        let source = r#"
def greet(name: str) -> str:
    return f"Hello, {name}"

class MyClass:
    def method(self):
        pass
"#;
        let (symbols, _chunks) = parse_file(source, "python", 50);
        assert!(!symbols.is_empty());
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"));
    }

    #[test]
    fn test_chunk_by_lines_fallback() {
        let source = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_, chunks) = parse_file(&source, "unknown_lang", 50);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let line_count = chunk.text.lines().count();
            assert!(line_count <= 55, "chunk too big: {line_count} lines");
        }
    }

    #[test]
    fn test_line_range_is_set() {
        let source = r#"
fn foo() {}

fn bar() {}
"#;
        let (symbols, _) = parse_file(source, "rust", 50);
        for sym in &symbols {
            assert!(sym.start_line > 0);
            assert!(sym.end_line >= sym.start_line);
        }
    }

    #[test]
    fn test_split_large_chunk_small_is_passthrough() {
        let chunk = CodeChunk {
            text: "fn small() { 42 }".to_string(),
            start_line: 1,
            end_line: 1,
        };
        let result = split_large_chunk(chunk, 800, 3);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text, "fn small() { 42 }");
    }

    #[test]
    fn test_split_large_chunk_splits_oversized() {
        // Build a fake large function: 50 lines of ~30 chars each = ~1500 chars
        let source = (1..=50u32)
            .map(|i| format!("    let variable_{i} = {i} * {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(source.len() > 800, "test source too short");

        let chunk = CodeChunk {
            text: source.clone(),
            start_line: 10,
            end_line: 59,
        };
        let result = split_large_chunk(chunk, 800, 3);

        // Should have produced multiple sub-chunks
        assert!(
            result.len() > 1,
            "expected multiple chunks, got {}",
            result.len()
        );

        // Each sub-chunk must fit within the char budget
        for c in &result {
            assert!(
                c.text.len() <= 850,
                "sub-chunk too large: {} chars",
                c.text.len()
            );
        }

        // All content is covered: first and last lines appear somewhere
        let all_text = result
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("variable_1"), "first line missing");
        assert!(all_text.contains("variable_50"), "last line missing");

        // Line numbers are correctly offset from the original start_line
        assert_eq!(result[0].start_line, 10);
        assert!(result.last().unwrap().end_line <= 59);
    }

    #[test]
    fn test_split_large_chunk_overlap_repeats_lines() {
        // 30 lines × 40 chars = 1200 chars; split at 400 chars with 3-line overlap
        let source = (1..=30u32)
            .map(|i| format!("line-content-{i:02}-padding-here"))
            .collect::<Vec<_>>()
            .join("\n");

        let chunk = CodeChunk {
            text: source,
            start_line: 1,
            end_line: 30,
        };
        let chunks = split_large_chunk(chunk, 400, 3);

        // Overlapping lines should appear in consecutive chunks
        if chunks.len() >= 2 {
            let last_lines_of_first: Vec<&str> = chunks[0].text.lines().rev().take(3).collect();
            let first_lines_of_second: Vec<&str> = chunks[1].text.lines().take(3).collect();
            // At least one overlap line should appear in both
            let overlap_count = last_lines_of_first
                .iter()
                .filter(|l| first_lines_of_second.contains(l))
                .count();
            assert!(overlap_count > 0, "no overlap between consecutive chunks");
        }
    }

    #[test]
    fn test_repo_indexer_uses_correct_chunk_size() {
        // Verify that the constants we ship stay within the model's context window.
        // AllMiniLML6V2: 256 tokens max ≈ 1024 chars at 4 chars/token.
        let max_safe_chars = 1024usize;

        // Fallback line-based chunks: CHUNK_SIZE_LINES lines × avg 60 chars/line
        let max_fallback_chars = super::super::CHUNK_SIZE_LINES * 60;
        assert!(
            max_fallback_chars <= max_safe_chars,
            "CHUNK_SIZE_LINES ({}) produces chunks up to {max_fallback_chars} chars, \
             exceeding model limit of {max_safe_chars}",
            super::super::CHUNK_SIZE_LINES,
        );

        // MAX_EMBED_CHARS is the hard ceiling applied after tree-sitter extraction
        assert!(
            super::super::MAX_EMBED_CHARS <= max_safe_chars,
            "MAX_EMBED_CHARS ({}) exceeds model limit",
            super::super::MAX_EMBED_CHARS,
        );
    }

    #[test]
    fn test_split_large_chunk_no_infinite_loop_on_long_lines() {
        // Regression test for OOM bug: when a single line is longer than
        // max_chars, the overlap calculation previously sent start back to 0,
        // creating an infinite loop that allocated 64GB before being killed.
        let long_line = "x".repeat(2000); // 2000 chars > max_chars=800
        let source = (0..10)
            .map(|_| long_line.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let chunk = CodeChunk {
            text: source.clone(),
            start_line: 1,
            end_line: 10,
        };

        // Must terminate and produce a bounded number of chunks
        let chunks = split_large_chunk(chunk, 800, 3);
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        assert!(
            chunks.len() <= 20,
            "should not loop indefinitely: {} chunks",
            chunks.len()
        );

        // Every chunk start must be strictly after the previous chunk start
        for window in chunks.windows(2) {
            assert!(
                window[1].start_line > window[0].start_line,
                "start_line must strictly increase: {} -> {}",
                window[0].start_line,
                window[1].start_line
            );
        }
    }
}
