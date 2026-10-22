use crate::ast::*;
use std::fmt::{self, Write};
use types_error::PgResult;

// Debug-only output owns its buffer and scope stack; neither is query-lifetime data.
struct Dump<'a, W> {
    out: W,
    datums: &'a [PlDatum],
    indent: usize,
    locals: Option<Vec<Dno>>,
}

pub(crate) fn dump_tree(function: &PlFunction) -> PgResult<()> {
    let mut output = String::new();
    let mut dump = Dump {
        out: &mut output,
        datums: &function.datums,
        indent: 0,
        locals: None,
    };
    dump.function(function, |oid| {
        let shape = syscache_seams::lookup_pg_type_typcache_shape::call(oid)?;
        Ok(shape
            .map(|s| String::from_utf8_lossy(s.typname.name_str()).into_owned())
            .unwrap_or_default())
    })?;
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(output.as_bytes());
    let _ = stdout.flush();
    Ok(())
}

impl<W: Write> Dump<'_, W> {
    fn text(&mut self, args: fmt::Arguments<'_>) {
        let _ = self.out.write_fmt(args);
    }
    fn ind(&mut self) {
        for _ in 0..self.indent {
            let _ = self.out.write_char(' ');
        }
    }
    fn expr(&mut self, expr: &PlExpr) {
        self.text(format_args!("'{}'", expr.query));
        if expr.target_param >= 0 {
            let local = self
                .locals
                .as_ref()
                .is_none_or(|v| v.contains(&expr.target_param));
            self.text(format_args!(
                " target {}{}",
                expr.target_param,
                if local { " (local)" } else { "" }
            ));
        }
    }
    fn expr_line(&mut self, prefix: &str, expr: &PlExpr) {
        self.ind();
        self.text(format_args!("{prefix}"));
        self.expr(expr);
        self.text(format_args!("\n"));
    }
    fn stmts(&mut self, stmts: &[PlStmt]) {
        self.indent += 2;
        for stmt in stmts {
            self.stmt(stmt);
        }
        self.indent -= 2;
    }
    fn end(&mut self, name: &str) {
        self.ind();
        self.text(format_args!("    {name}\n"));
    }
    fn block(&mut self, block: &PlBlock) {
        let saved = self.locals.clone();
        if block.exceptions.is_some() {
            self.locals = Some(Vec::new());
        } else if let Some(locals) = &mut self.locals {
            locals.extend_from_slice(&block.initvarnos);
        }
        let name = block.label.as_deref().unwrap_or("*unnamed*");
        self.ind();
        self.text(format_args!("BLOCK <<{name}>>\n"));
        self.stmts(&block.body);
        if let Some(exceptions) = &block.exceptions {
            for exc in &exceptions.exc_list {
                self.ind();
                self.text(format_args!("    EXCEPTION WHEN "));
                for (i, condition) in exc.conditions.iter().enumerate() {
                    self.text(format_args!(
                        "{}{}",
                        if i == 0 { "" } else { " OR " },
                        condition.condname
                    ));
                }
                self.text(format_args!(" THEN\n"));
                self.stmts(&exc.action);
            }
        }
        self.ind();
        self.text(format_args!("    END -- {name}\n"));
        self.locals = saved;
    }
    fn params(&mut self, params: &[PlExpr], dollar: bool) {
        if params.is_empty() {
            return;
        }
        self.indent += 2;
        self.end("USING");
        self.indent += 2;
        for (i, expr) in params.iter().enumerate() {
            self.ind();
            self.text(format_args!(
                "    parameter {}{}: ",
                if dollar { "$" } else { "" },
                i + 1
            ));
            self.expr(expr);
            self.text(format_args!("\n"));
        }
        self.indent -= 4;
    }
    fn into(&mut self, target: Dno, strict: bool) {
        if target < 0 {
            return;
        }
        self.indent += 2;
        self.ind();
        let _ = writeln!(
            self.out,
            "    INTO{} target = {} {}",
            if strict { " STRICT" } else { "" },
            target,
            self.datums[target as usize].refname()
        );
        self.indent -= 2;
    }
    fn stmt(&mut self, stmt: &PlStmt) {
        self.text(format_args!("{:3}:", stmt_lineno(stmt)));
        if let PlStmt::Block(block) = stmt {
            self.block(block);
            return;
        }
        self.ind();
        match stmt {
            PlStmt::Block(_) => unreachable!(),
            PlStmt::Assign { varno, expr, .. } => {
                self.text(format_args!("ASSIGN var {varno} := "));
                self.expr(expr);
                self.text(format_args!("\n"));
            }
            PlStmt::If {
                cond,
                then_body,
                elsifs,
                else_body,
                ..
            } => {
                self.text(format_args!("IF "));
                self.expr(cond);
                self.text(format_args!(" THEN\n"));
                self.stmts(then_body);
                for (cond, body) in elsifs {
                    self.ind();
                    self.text(format_args!("    ELSIF "));
                    self.expr(cond);
                    self.text(format_args!(" THEN\n"));
                    self.stmts(body);
                }
                if let Some(body) = else_body.as_ref().filter(|b| !b.is_empty()) {
                    self.end("ELSE");
                    self.stmts(body);
                }
                self.end("ENDIF");
            }
            PlStmt::Case {
                t_expr,
                t_varno,
                whens,
                have_else,
                else_stmts,
                ..
            } => {
                self.text(format_args!("CASE {t_varno} "));
                if let Some(e) = t_expr {
                    self.expr(e);
                }
                self.text(format_args!("\n"));
                self.indent += 6;
                for (expr, body) in whens {
                    self.expr_line("WHEN ", expr);
                    self.ind();
                    self.text(format_args!("THEN\n"));
                    self.indent += 2;
                    self.stmts(body);
                    self.indent -= 2;
                }
                if *have_else {
                    self.ind();
                    self.text(format_args!("ELSE\n"));
                    self.indent += 2;
                    self.stmts(else_stmts);
                    self.indent -= 2;
                }
                self.indent -= 6;
                self.end("ENDCASE");
            }
            PlStmt::Loop { body, .. } => {
                self.text(format_args!("LOOP\n"));
                self.stmts(body);
                self.end("ENDLOOP");
            }
            PlStmt::While { cond, body, .. } => {
                self.text(format_args!("WHILE "));
                self.expr(cond);
                self.text(format_args!("\n"));
                self.stmts(body);
                self.end("ENDWHILE");
            }
            PlStmt::ForI {
                var,
                lower,
                upper,
                step,
                reverse,
                body,
                ..
            } => {
                let _ = writeln!(
                    self.out,
                    "FORI {} {}",
                    self.datums[*var as usize].refname(),
                    if *reverse { "REVERSE" } else { "NORMAL" }
                );
                self.indent += 2;
                self.expr_line("    lower = ", lower);
                self.expr_line("    upper = ", upper);
                if let Some(e) = step {
                    self.expr_line("    step = ", e);
                }
                self.indent -= 2;
                self.stmts(body);
                self.end("ENDFORI");
            }
            PlStmt::ForS {
                var, query, body, ..
            }
            | PlStmt::DynForS {
                var, query, body, ..
            } => {
                let _ = write!(
                    self.out,
                    "FORS {} {}",
                    self.datums[*var as usize].refname(),
                    if matches!(stmt, PlStmt::DynForS { .. }) {
                        "EXECUTE "
                    } else {
                        ""
                    }
                );
                self.expr(query);
                self.text(format_args!("\n"));
                if let PlStmt::DynForS { params, .. } = stmt {
                    self.params(params, true);
                }
                self.stmts(body);
                self.end("ENDFORS");
            }
            PlStmt::ForC {
                var,
                curvar,
                argquery,
                body,
                ..
            } => {
                let _ = writeln!(
                    self.out,
                    "FORC {} curvar={curvar}",
                    self.datums[*var as usize].refname()
                );
                self.indent += 2;
                if let Some(e) = argquery {
                    self.expr_line("  arguments = ", e);
                }
                self.indent -= 2;
                self.stmts(body);
                self.end("ENDFORC");
            }
            PlStmt::ForEachA {
                varno,
                slice,
                expr,
                body,
                ..
            } => {
                self.text(format_args!("FOREACHA var {varno} "));
                if *slice != 0 {
                    self.text(format_args!("SLICE {slice} "));
                }
                self.text(format_args!("IN "));
                self.expr(expr);
                self.text(format_args!("\n"));
                self.stmts(body);
                self.ind();
                self.text(format_args!("    ENDFOREACHA"));
            }
            PlStmt::ExitContinue {
                is_exit,
                label,
                cond,
                ..
            } => {
                self.text(format_args!(
                    "{}",
                    if *is_exit { "EXIT" } else { "CONTINUE" }
                ));
                if let Some(label) = label {
                    self.text(format_args!(" label='{label}'"));
                }
                if let Some(e) = cond {
                    self.text(format_args!(" WHEN "));
                    self.expr(e);
                }
                self.text(format_args!("\n"));
            }
            PlStmt::Return { expr, retvarno, .. } | PlStmt::ReturnNext { expr, retvarno, .. } => {
                self.text(format_args!(
                    "RETURN {}",
                    if matches!(stmt, PlStmt::ReturnNext { .. }) {
                        "NEXT "
                    } else {
                        ""
                    }
                ));
                if *retvarno >= 0 {
                    self.text(format_args!("variable {retvarno}"));
                } else if let Some(e) = expr {
                    self.expr(e);
                } else {
                    self.text(format_args!("NULL"));
                }
                self.text(format_args!("\n"));
            }
            PlStmt::ReturnQuery {
                query,
                dynquery,
                params,
                ..
            } => {
                if let Some(e) = query {
                    self.text(format_args!("RETURN QUERY "));
                    self.expr(e);
                    self.text(format_args!("\n"));
                } else if let Some(e) = dynquery {
                    self.text(format_args!("RETURN QUERY EXECUTE "));
                    self.expr(e);
                    self.text(format_args!("\n"));
                    self.params(params, true);
                }
            }
            PlStmt::Raise {
                elog_level,
                condname,
                message,
                params,
                options,
                ..
            } => {
                self.text(format_args!("RAISE level={elog_level}"));
                if let Some(c) = condname {
                    self.text(format_args!(" condname='{c}'"));
                }
                if let Some(m) = message {
                    self.text(format_args!(" message='{m}'"));
                }
                self.text(format_args!("\n"));
                self.indent += 2;
                for (i, e) in params.iter().enumerate() {
                    self.ind();
                    self.text(format_args!("    parameter {i}: "));
                    self.expr(e);
                    self.text(format_args!("\n"));
                }
                if !options.is_empty() {
                    self.end("USING");
                    self.indent += 2;
                    for opt in options {
                        let name = [
                            "ERRCODE",
                            "MESSAGE",
                            "DETAIL",
                            "HINT",
                            "COLUMN",
                            "CONSTRAINT",
                            "DATATYPE",
                            "TABLE",
                            "SCHEMA",
                        ][opt.opt_type as usize];
                        self.ind();
                        self.text(format_args!("    {name} = "));
                        self.expr(&opt.expr);
                        self.text(format_args!("\n"));
                    }
                    self.indent -= 2;
                }
                self.indent -= 2;
            }
            PlStmt::Assert { cond, message, .. } => {
                self.text(format_args!("ASSERT "));
                self.expr(cond);
                self.text(format_args!("\n"));
                self.indent += 2;
                if let Some(e) = message {
                    self.expr_line("    MESSAGE = ", e);
                }
                self.indent -= 2;
            }
            PlStmt::ExecSql {
                sqlstmt,
                target,
                strict,
                ..
            } => {
                self.text(format_args!("EXECSQL "));
                self.expr(sqlstmt);
                self.text(format_args!("\n"));
                self.into(*target, *strict);
            }
            PlStmt::DynExecute {
                query,
                target,
                strict,
                params,
                ..
            } => {
                self.text(format_args!("EXECUTE "));
                self.expr(query);
                self.text(format_args!("\n"));
                self.into(*target, *strict);
                self.params(params, false);
            }
            PlStmt::Perform { expr, .. } | PlStmt::Call { expr, .. } => {
                let name = match stmt {
                    PlStmt::Perform { .. } => "PERFORM",
                    PlStmt::Call { is_call: true, .. } => "CALL",
                    _ => "DO",
                };
                self.text(format_args!("{name} expr = "));
                self.expr(expr);
                self.text(format_args!("\n"));
            }
            PlStmt::Commit { chain, .. } | PlStmt::Rollback { chain, .. } => {
                self.text(format_args!(
                    "{}{}\n",
                    if matches!(stmt, PlStmt::Commit { .. }) {
                        "COMMIT"
                    } else {
                        "ROLLBACK"
                    },
                    if *chain { " AND CHAIN" } else { "" }
                ));
            }
            PlStmt::GetDiag {
                is_stacked, items, ..
            } => {
                self.text(format_args!(
                    "GET {} DIAGNOSTICS ",
                    if *is_stacked { "STACKED" } else { "CURRENT" }
                ));
                for (i, item) in items.iter().enumerate() {
                    let name = [
                        "ROW_COUNT",
                        "PG_ROUTINE_OID",
                        "PG_CONTEXT",
                        "PG_EXCEPTION_CONTEXT",
                        "PG_EXCEPTION_DETAIL",
                        "PG_EXCEPTION_HINT",
                        "RETURNED_SQLSTATE",
                        "COLUMN_NAME",
                        "CONSTRAINT_NAME",
                        "PG_DATATYPE_NAME",
                        "MESSAGE_TEXT",
                        "TABLE_NAME",
                        "SCHEMA_NAME",
                    ][item.kind as usize];
                    self.text(format_args!(
                        "{}{{var {}}} = {name}",
                        if i == 0 { "" } else { ", " },
                        item.target
                    ));
                }
                self.text(format_args!("\n"));
            }
            PlStmt::Open {
                curvar,
                argquery,
                query,
                dynquery,
                params,
                ..
            } => {
                self.text(format_args!("OPEN curvar={curvar}\n"));
                self.indent += 2;
                for (name, expr) in [
                    ("arguments", argquery),
                    ("query", query),
                    ("execute", dynquery),
                ] {
                    if let Some(e) = expr {
                        self.ind();
                        self.text(format_args!("  {name} = '"));
                        self.expr(e);
                        self.text(format_args!("'\n"));
                    }
                }
                if dynquery.is_some() {
                    self.params(params, true);
                }
                self.indent -= 2;
            }
            PlStmt::Fetch {
                target,
                curvar,
                direction,
                how_many,
                expr,
                is_move,
                ..
            } => {
                self.text(format_args!(
                    "{} curvar={curvar}\n",
                    if *is_move { "MOVE" } else { "FETCH" }
                ));
                self.indent += 2;
                self.ind();
                self.text(format_args!(
                    "    {} ",
                    ["FORWARD", "BACKWARD", "ABSOLUTE", "RELATIVE"][*direction as usize]
                ));
                if let Some(e) = expr {
                    self.expr(e);
                } else {
                    self.text(format_args!("{how_many}"));
                }
                self.text(format_args!("\n"));
                if !*is_move && *target >= 0 {
                    self.ind();
                    let _ = writeln!(
                        self.out,
                        "    target = {target} {}",
                        self.datums[*target as usize].refname()
                    );
                }
                self.indent -= 2;
            }
            PlStmt::Close { curvar, .. } => self.text(format_args!("CLOSE curvar={curvar}\n")),
        }
    }
    fn properties(&mut self, constant: bool, notnull: bool, default: &Option<PlExpr>) {
        if constant {
            self.text(format_args!("                                  CONSTANT\n"));
        }
        if notnull {
            self.text(format_args!("                                  NOT NULL\n"));
        }
        if let Some(e) = default {
            self.text(format_args!("                                  DEFAULT "));
            self.expr(e);
            self.text(format_args!("\n"));
        }
    }
    fn function(
        &mut self,
        function: &PlFunction,
        mut typename: impl FnMut(types_core::Oid) -> PgResult<String>,
    ) -> PgResult<()> {
        self.text(format_args!("\nExecution tree of successfully compiled PL/pgSQL function {}:\n\nFunction's data area:\n", function.fn_signature));
        for (i, datum) in function.datums.iter().enumerate() {
            self.text(format_args!("    entry {i}: "));
            match datum {
                PlDatum::Var(v) => {
                    self.text(format_args!(
                        "VAR {}{padding} type {} (typoid {}) atttypmod {}\n",
                        v.refname,
                        typename(v.datatype.typoid)?,
                        v.datatype.typoid,
                        v.datatype.atttypmod,
                        padding = &"                "[..16usize.saturating_sub(v.refname.len())]
                    ));
                    self.properties(v.isconst, v.notnull, &v.default_val);
                    if let Some(e) = &v.cursor_explicit_expr {
                        if v.cursor_explicit_argrow >= 0 {
                            self.text(format_args!(
                                "                                  CURSOR argument row {}\n",
                                v.cursor_explicit_argrow
                            ));
                        }
                        self.text(format_args!("                                  CURSOR IS "));
                        self.expr(e);
                        self.text(format_args!("\n"));
                    }
                    if v.promise != PROMISE_NONE {
                        self.text(format_args!(
                            "                                  PROMISE {}\n",
                            v.promise
                        ));
                    }
                }
                PlDatum::Row(r) => {
                    self.text(format_args!(
                        "ROW {}{padding} fields",
                        r.refname,
                        padding = &"                "[..16usize.saturating_sub(r.refname.len())]
                    ));
                    for (name, varno) in r.fieldnames.iter().zip(&r.varnos) {
                        self.text(format_args!(" {name}=var {varno}"));
                    }
                    self.text(format_args!("\n"));
                }
                PlDatum::Rec(r) => {
                    self.text(format_args!(
                        "REC {}{padding} typoid {}\n",
                        r.refname,
                        r.rectypeid,
                        padding = &"                "[..16usize.saturating_sub(r.refname.len())]
                    ));
                    self.properties(r.isconst, r.notnull, &r.default_val);
                }
                PlDatum::RecField(r) => self.text(format_args!(
                    "RECFIELD {}{padding} of REC {}\n",
                    r.fieldname,
                    r.recparentno,
                    padding = &"                "[..16usize.saturating_sub(r.fieldname.len())]
                )),
            }
        }
        self.text(format_args!(
            "\nFunction's statements:\n{:3}:",
            function.action.lineno
        ));
        if has_exceptions(&function.action) {
            self.locals = Some(
                function
                    .fn_argvarnos
                    .iter()
                    .zip(
                        function
                            .fn_arg_is_input
                            .iter()
                            .chain(std::iter::repeat(&true)),
                    )
                    .filter_map(|(&dno, &is_input)| is_input.then_some(dno))
                    .take(function.fn_nargs as usize)
                    .collect(),
            );
        }
        self.block(&function.action);
        self.text(format_args!(
            "\nEnd of execution tree of function {}\n\n",
            function.fn_signature
        ));
        Ok(())
    }
}

fn has_exceptions(block: &PlBlock) -> bool {
    fn statements(stmts: &[PlStmt]) -> bool {
        stmts.iter().any(|s| match s {
            PlStmt::Block(b) => has_exceptions(b),
            PlStmt::If {
                then_body,
                elsifs,
                else_body,
                ..
            } => {
                statements(then_body)
                    || elsifs.iter().any(|(_, b)| statements(b))
                    || else_body.as_ref().is_some_and(|b| statements(b))
            }
            PlStmt::Case {
                whens, else_stmts, ..
            } => whens.iter().any(|(_, b)| statements(b)) || statements(else_stmts),
            PlStmt::Loop { body, .. }
            | PlStmt::While { body, .. }
            | PlStmt::ForI { body, .. }
            | PlStmt::ForS { body, .. }
            | PlStmt::ForC { body, .. }
            | PlStmt::DynForS { body, .. }
            | PlStmt::ForEachA { body, .. } => statements(body),
            _ => false,
        })
    }
    block.exceptions.is_some() || statements(&block.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(query: &str, target_param: Dno) -> PlExpr {
        PlExpr {
            query: query.into(),
            parse_mode: parser_seams::RawParseMode::RAW_PARSE_PLPGSQL_EXPR,
            ns: -1,
            expr_id: 0,
            target_param,
            target_is_local: target_param >= 0,
        }
    }

    fn var(dno: Dno, name: &str, oid: u32, default_val: Option<PlExpr>) -> PlDatum {
        PlDatum::Var(PlVar {
            dno,
            refname: name.into(),
            lineno: 1,
            datatype: PlType {
                typoid: oid,
                ttype: TypeKind::Scalar,
                typlen: 8,
                typbyval: true,
                typtype: b'b' as i8,
                collation: 0,
                typisarray: false,
                atttypmod: -1,
                typinput: 0,
                typioparam: 0,
                rec_ident: None,
            },
            isconst: false,
            notnull: false,
            default_val,
            promise: PROMISE_NONE,
            cursor_explicit_expr: None,
            cursor_explicit_argrow: -1,
            cursor_options: 0,
        })
    }

    fn function(name: &str, datums: Vec<PlDatum>, body: Vec<PlStmt>) -> PlFunction {
        PlFunction {
            fn_signature: name.into(),
            fn_oid: 0,
            fn_xmin: 0,
            fn_tid: (0, 0),
            fn_input_collation: 0,
            fn_rettype: 20,
            fn_rettyplen: 8,
            fn_retbyval: true,
            fn_retistuple: false,
            fn_retisdomain: false,
            fn_retset: false,
            fn_readonly: false,
            fn_prokind: b'f' as i8,
            fn_is_trigger: FnTrigger::NotTrigger,
            fn_nargs: 1,
            fn_argvarnos: vec![0],
            fn_arg_is_input: vec![true],
            new_varno: -1,
            old_varno: -1,
            found_varno: 1,
            out_param_varno: -1,
            datums,
            ns: vec![],
            action: PlBlock {
                lineno: 1,
                label: None,
                body,
                initvarnos: vec![],
                exceptions: None,
            },
            resolve_option: 0,
            print_strict_params: false,
            nstatements: 0,
            expr_ids: vec![],
        }
    }

    #[test]
    fn execution_trees_match_c_stdout() {
        let named = function(
            "p158_dump(bigint)",
            vec![var(0, "a", 20, None), var(1, "found", 16, None)],
            vec![PlStmt::Return {
                lineno: 1,
                expr: Some(expr("1", -1)),
                retvarno: -1,
            }],
        );
        let looping = function(
            "p158_loop(bigint)",
            vec![
                var(0, "a", 20, None),
                var(1, "found", 16, None),
                var(2, "s", 20, Some(expr("0", 2))),
                var(3, "i", 23, None),
            ],
            vec![
                PlStmt::ForI {
                    lineno: 1,
                    label: None,
                    var: 3,
                    lower: expr("1", -1),
                    upper: expr("a", -1),
                    step: None,
                    reverse: false,
                    body: vec![PlStmt::Assign {
                        lineno: 1,
                        varno: 2,
                        expr: expr("s := s + i", 2),
                    }],
                },
                PlStmt::Return {
                    lineno: 1,
                    expr: None,
                    retvarno: 2,
                },
            ],
        );
        let inline = function(
            "inline_code_block",
            vec![var(0, "found", 16, None)],
            vec![
                PlStmt::Perform {
                    lineno: 1,
                    expr: expr("SELECT 1", -1),
                },
                PlStmt::Return {
                    lineno: 0,
                    expr: None,
                    retvarno: -1,
                },
            ],
        );
        let mut output = String::new();
        for function in [named, looping, inline] {
            let mut dump = Dump {
                out: &mut output,
                datums: &function.datums,
                indent: 0,
                locals: None,
            };
            dump.function(&function, |oid| {
                Ok(match oid {
                    16 => "bool",
                    20 => "int8",
                    23 => "int4",
                    _ => unreachable!(),
                }
                .into())
            })
            .unwrap();
        }
        assert_eq!(output, include_str!("../tests/sql/option_dump.stdout"));
    }

    #[test]
    fn datum_name_padding_counts_bytes() {
        let function = function(
            "padding()",
            vec![var(0, "é", 20, None), var(1, "éééééééééé", 20, None)],
            vec![],
        );
        let mut output = String::new();
        let mut dump = Dump {
            out: &mut output,
            datums: &function.datums,
            indent: 0,
            locals: None,
        };
        dump.function(&function, |_| Ok("int8".into())).unwrap();
        assert!(output.contains("VAR é               type int8"));
        assert!(output.contains("VAR éééééééééé type int8"));
    }

    #[test]
    fn exception_scope_controls_assignment_target_annotation() {
        let mut function = function(
            "scope()",
            vec![var(0, "a", 20, Some(expr("0", 0)))],
            vec![PlStmt::Assign {
                lineno: 1,
                varno: 0,
                expr: expr("a := 1", 0),
            }],
        );
        function.action.initvarnos.push(0);
        function.action.exceptions = Some(ExceptionBlock {
            sqlstate_varno: 1,
            sqlerrm_varno: 2,
            exc_list: vec![],
        });
        let mut output = String::new();
        let mut dump = Dump {
            out: &mut output,
            datums: &function.datums,
            indent: 0,
            locals: None,
        };
        dump.function(&function, |_| Ok("int8".into())).unwrap();
        assert!(output.contains("DEFAULT '0' target 0 (local)\n"));
        assert!(output.contains("ASSIGN var 0 := 'a := 1' target 0\n"));
    }
}
