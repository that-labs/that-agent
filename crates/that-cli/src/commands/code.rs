use crate::ToolContext;
use that_tools::output;

pub fn handle_code(
    ctx: &ToolContext,
    command: &that_tools::cli::CodeCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::CodeCommands;
    match command {
        CodeCommands::Read {
            path,
            context,
            symbols,
            line,
            end_line,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let ctx_lines = context.or(Some(ctx.config.output.code_read_context_lines));
            let result = that_tools::tools::code::code_read(
                path,
                ctx_lines,
                *symbols,
                ctx.max_tokens,
                *line,
                *end_line,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::Grep {
            pattern,
            path,
            context,
            limit,
            ignore_case,
            regex,
            include,
            exclude,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            // Auto-correct swapped arguments
            let (pattern, path) = if !path.exists() && std::path::Path::new(pattern).exists() {
                (
                    path.to_string_lossy().into_owned(),
                    std::path::PathBuf::from(pattern),
                )
            } else {
                (pattern.clone(), path.clone())
            };
            let result = that_tools::tools::code::code_grep_filtered_with_options(
                &path,
                &pattern,
                *context,
                ctx.max_tokens,
                *limit,
                *ignore_case,
                *regex,
                include,
                exclude,
                that_tools::tools::code::GrepRuntimeOptions {
                    workers: Some(ctx.config.code.grep_workers),
                    mmap_min_bytes: Some(ctx.config.code.mmap_min_bytes),
                },
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::Tree {
            path,
            depth,
            compact,
            ranked,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let tree_depth = depth.or(Some(ctx.config.output.code_tree_max_depth));
            let result = that_tools::tools::code::code_tree(
                path,
                tree_depth,
                ctx.max_tokens,
                *compact,
                *ranked,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::Symbols {
            path,
            kind,
            name,
            references,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            handle_symbols(ctx, path, kind.as_deref(), name.as_deref(), *references)
        }
        CodeCommands::Index { path, status } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            handle_index(ctx, path, *status)
        }
        CodeCommands::Edit {
            path,
            patch,
            search,
            replace,
            target_fn,
            new_body,
            whole_file,
            all,
            dry_run,
        } => {
            ctx.check_policy("code_edit").map_err(|e| e.to_string())?;
            handle_edit(
                ctx,
                path,
                *patch,
                search.clone(),
                replace.clone(),
                target_fn.clone(),
                new_body.clone(),
                *whole_file,
                *all,
                *dry_run,
            )
        }
        CodeCommands::Summary { path } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let result = that_tools::tools::code::summary::code_summary(path, ctx.max_tokens)
                .map_err(|e| e.to_string())?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::AstGrep {
            pattern,
            path,
            language,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let result = that_tools::tools::code::astgrep::structural_search(
                path,
                pattern,
                language.as_deref(),
                ctx.max_tokens,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}

fn handle_symbols(
    ctx: &ToolContext,
    path: &std::path::Path,
    kind_filter: Option<&str>,
    name_filter: Option<&str>,
    references: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::tools::code::parse;

    let symbols = if path.is_file() {
        let parsed = parse::parse_file(path)?;
        parsed.symbols
    } else {
        let mut all_symbols = Vec::new();
        let walker = ignore::WalkBuilder::new(path)
            .hidden(true)
            .git_ignore(true)
            .build();

        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if parse::Language::from_path(entry.path()).is_some() {
                if let Ok(parsed) = parse::parse_file(entry.path()) {
                    for mut sym in parsed.symbols {
                        let rel = entry
                            .path()
                            .strip_prefix(path)
                            .unwrap_or(entry.path())
                            .to_string_lossy();
                        sym.name = format!("{}:{}", rel, sym.name);
                        all_symbols.push(sym);
                    }
                }
            }
        }
        all_symbols
    };

    let filtered: Vec<_> = symbols
        .into_iter()
        .filter(|s| {
            if let Some(k) = kind_filter {
                let kind_str = format!("{:?}", s.kind).to_lowercase();
                if !kind_str.contains(&k.to_lowercase()) {
                    return false;
                }
            }
            if let Some(n) = name_filter {
                if !s.name.to_lowercase().contains(&n.to_lowercase()) {
                    return false;
                }
            }
            true
        })
        .collect();

    if references {
        let start = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let root = that_tools::index::find_tools_root(start).unwrap_or_else(|| start.to_path_buf());
        let db_path = that_tools::index::index_db_path(&root);
        if !db_path.exists() && ctx.config.code.auto_index {
            if let Ok(idx) = that_tools::index::SymbolIndex::open(&db_path) {
                let _ = idx.build(&root);
            }
        }
        if db_path.exists() {
            if let Ok(idx) = that_tools::index::SymbolIndex::open(&db_path) {
                #[derive(serde::Serialize)]
                struct SymbolWithRefs {
                    name: String,
                    kind: String,
                    line_start: usize,
                    line_end: usize,
                    references: Vec<that_tools::index::IndexedReference>,
                }
                let enriched: Vec<SymbolWithRefs> = filtered
                    .iter()
                    .map(|s| {
                        let bare_name = s.name.rsplit(':').next().unwrap_or(&s.name);
                        let refs = idx.query_references(bare_name).unwrap_or_default();
                        SymbolWithRefs {
                            name: s.name.clone(),
                            kind: format!("{:?}", s.kind).to_lowercase(),
                            line_start: s.line_start,
                            line_end: s.line_end,
                            references: refs,
                        }
                    })
                    .collect();
                let result = output::emit_json(&enriched, ctx.max_tokens);
                println!("{}", ctx.format_output(&result));
                return Ok(());
            }
        }
    }

    let result = output::emit_json(&filtered, ctx.max_tokens);
    println!("{}", ctx.format_output(&result));
    Ok(())
}

fn handle_index(
    ctx: &ToolContext,
    path: &std::path::Path,
    status: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let db_path = that_tools::index::index_db_path(root);

    if status {
        if !db_path.exists() {
            let status = that_tools::index::IndexStatus {
                path: db_path.to_string_lossy().to_string(),
                total_files: 0,
                total_symbols: 0,
                total_refs: 0,
                stale_files: 0,
                schema_version: "none".to_string(),
            };
            let result = output::emit_json(&status, ctx.max_tokens);
            println!("{}", ctx.format_output(&result));
            return Ok(());
        }
        let idx = that_tools::index::SymbolIndex::open(&db_path)?;
        let status = idx.status(root)?;
        let result = output::emit_json(&status, ctx.max_tokens);
        println!("{}", ctx.format_output(&result));
    } else {
        let idx = that_tools::index::SymbolIndex::open(&db_path)?;
        let build_result = idx.build(root)?;
        let result = output::emit_json(&build_result, ctx.max_tokens);
        println!("{}", ctx.format_output(&result));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_edit(
    ctx: &ToolContext,
    path: &std::path::Path,
    patch: bool,
    search: Option<String>,
    replace: Option<String>,
    target_fn: Option<String>,
    new_body: Option<String>,
    whole_file: bool,
    all: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::tools::code::edit;
    use that_tools::tools::code::git;

    let format = if patch {
        edit::EditFormat::UnifiedDiff
    } else if let (Some(s), Some(r)) = (&search, &replace) {
        edit::EditFormat::SearchReplace {
            search: s.replace("\\n", "\n").replace("\\t", "\t"),
            replace: r.replace("\\n", "\n").replace("\\t", "\t"),
            all,
        }
    } else if let (Some(name), Some(body)) = (&target_fn, &new_body) {
        edit::EditFormat::AstNode {
            symbol_name: name.clone(),
            new_body: body.replace("\\n", "\n").replace("\\t", "\t"),
        }
    } else if whole_file {
        edit::EditFormat::WholeFile
    } else {
        return Err(
            "specify one of: --patch, --search/--replace, --fn/--new-body, --whole-file".into(),
        );
    };

    let checkpoint = if ctx.config.code.git_safety && !dry_run {
        match git::create_checkpoint(path, ctx.config.code.git_safety_branch) {
            Ok(cp) => Some(cp),
            Err(e) => {
                tracing::debug!("git checkpoint skipped: {}", e);
                None
            }
        }
    } else {
        None
    };

    let result = edit::code_edit(path, &format, dry_run, ctx.max_tokens);

    match result {
        Ok(output) => {
            // Edit succeeded — stash is intentionally left (no drop API), safety branch already
            // created. The user can clean it up with `git stash drop` / `git branch -D`.
            println!("{}", ctx.format_output(&output));
            Ok(())
        }
        Err(e) => {
            // Edit failed — restore the pre-edit state
            if let Some(cp) = &checkpoint {
                if let Err(re) = git::restore_checkpoint(cp) {
                    tracing::warn!("checkpoint restore failed after edit error: {}", re);
                }
            }
            Err(e.to_string().into())
        }
    }
}
