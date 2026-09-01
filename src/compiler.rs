//! BT AST to bytecode compiler.
//!
//! The compiler turns parser AST nodes into register bytecode. Unsupported syntax is never ignored silently;
//! it produces a compilation error with a source location.

use crate::bytecode::{
    Chunk, FunctionChunk, FunctionParam, Instruction, Register, SourceSpan, SymbolId,
};
use crate::lexer::TokenKind;
use crate::parser::{Expr, MatchArm, PosExpr, Statement};
use crate::path;
use crate::value::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Bytecode compilation error.
#[derive(Debug, Clone)]
pub struct CompileError {
    /// The file where the error is located.
    pub file: String,
    /// The line number where the error occurred.
    pub line: usize,
    /// The column number where the error is located.
    pub column: usize,
    /// Error message.
    pub message: String,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}: Compilation error: {}",
            self.file, self.line, self.column, self.message
        )
    }
}

impl std::error::Error for CompileError {}

/// AST bytecode compiler.
pub struct Compiler {
    /// The bytecode block currently being built.
    chunk: Chunk,
    /// Next available virtual register.
    next_register: Register,
    /// The directory where the current compiled file is located, used to resolve relative paths such as `include('demo.bt')`.
    base_dir: PathBuf,
    /// Include stack used to reject recursive compilation of the same file.
    include_stack: Vec<PathBuf>,
    /// The jump backfill context of the current nested loop.
    loop_stack: Vec<LoopContext>,
    /// The nesting depth of try blocks currently being compiled.
    try_depth: usize,
    /// Constants declared at the top level of the current compilation unit.
    ///
    /// The function compiler inherits this set, which is used to prohibit function local constants from shadowing global constants.
    global_constants: HashSet<String>,
    /// The constant name that has been defined in the current bytecode block.
    ///
    /// The top-level and each function block maintain their own collections, so local constants with the same name in different functions do not conflict with each other.
    defined_constants: HashSet<String>,
    /// Whether the current bytecode block belongs to function scope.
    is_function_scope: bool,
}

/// The semantic classification of the identifier in its binding position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingNameKind {
    /// Ordinary variable variable.
    Variable,
    /// Constants whose first letter is capitalized and followed by English letters, numbers or underscores.
    Constant,
}

/// A parsed assignment target that can be reused.
///
/// Attribute targets pin object and key registers during compilation, then reuse
/// them for reads and writes. This ensures object expressions and dynamic indexes
/// containing function calls are evaluated only once.
enum AssignmentTarget {
    /// Ordinary variables, local variables, parameters or closure capture variables.
    Binding {
        /// Binding name, used for constant rules and error messages.
        name: String,
        /// The symbol number in the current chunk.
        symbol: SymbolId,
    },
    /// Object field, array subscript, or class instance field.
    Property {
        /// The evaluated direct host object register.
        object: Register,
        /// The evaluated attribute name or index register.
        key: Register,
    },
}

/// `break` / `continue` backfill information within a single loop body.
///
/// BT supports labeled loops. The compiler records unresolved jump locations and
/// patches them to the correct continue or break target once the loop is complete.
#[derive(Debug, Clone)]
struct LoopContext {
    /// Loop label; an empty string means unlabeled.
    label: String,
    /// `continue` The index of the instruction that should be jumped back to.
    continue_target: u32,
    /// `break` jump instruction location to be backfilled.
    breaks: Vec<usize>,
    /// `continue` jump instruction location to be backfilled.
    continues: Vec<usize>,
}

impl Compiler {
    /// Creates a new compiler.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            chunk: Chunk::new(),
            next_register: 0,
            base_dir: PathBuf::from("."),
            include_stack: Vec::new(),
            loop_stack: Vec::new(),
            try_depth: 0,
            global_constants: HashSet::new(),
            defined_constants: HashSet::new(),
            is_function_scope: false,
        }
    }

    /// Creates a compiler with a base directory.
    pub fn with_base_dir(base_dir: impl Into<PathBuf>) -> Self {
        let base_dir = base_dir.into();
        let mut chunk = Chunk::new();
        chunk.source_dir = path::path_text(&path::normalize_path(&base_dir));
        Self {
            chunk,
            next_register: 0,
            base_dir,
            include_stack: Vec::new(),
            loop_stack: Vec::new(),
            try_depth: 0,
            global_constants: HashSet::new(),
            defined_constants: HashSet::new(),
            is_function_scope: false,
        }
    }

    /// Creates a compiler with source files and base directories.
    ///
    /// `source_file` writes Chunk metainformation, and runtime function calls, include and template fragments will be restored accordingly.
    /// The current source code directory; `base_dir` is still reserved for logical use that requires directory context during compilation.
    pub fn with_source_file(source_file: impl Into<String>, base_dir: impl Into<PathBuf>) -> Self {
        let mut compiler = Self::with_base_dir(base_dir);
        compiler.chunk.source_file = source_file.into().replace('\\', "/");
        if compiler.chunk.source_dir.is_empty() {
            let source_dir = Path::new(&compiler.chunk.source_file)
                .parent()
                .unwrap_or_else(|| Path::new("."));
            compiler.chunk.source_dir = path::path_text(&path::normalize_path(source_dir));
        }
        compiler
    }

    /// Compiles a statement list into executable bytecode.
    pub fn compile(mut self, statements: &[Statement]) -> Result<Chunk, CompileError> {
        self.prepare_global_constants(statements)?;
        for statement in statements {
            self.compile_statement(statement)?;
        }
        self.chunk.emit(Instruction::Halt);
        self.chunk.register_count = self.next_register;
        Ok(self.chunk)
    }

    /// Compiles a statement list and returns the value of its last statement.
    ///
    /// `include()` What is needed is not "what the script outputs", but "the value of the referenced file after execution".
    /// `compile_block_value` preserves explicit `return`. Otherwise the last
    /// statement becomes the result, and an empty file returns `Empty`.
    pub fn compile_returning_value(
        mut self,
        statements: &[Statement],
    ) -> Result<Chunk, CompileError> {
        self.prepare_global_constants(statements)?;
        let src = self.compile_block_value(statements)?;
        self.chunk.emit(Instruction::Return { src });
        self.chunk.emit(Instruction::Halt);
        self.chunk.register_count = self.next_register;
        Ok(self.chunk)
    }

    /// Pre-collects the top-level constant names of the current compilation unit.
    ///
    /// BT constants are isolated by function scope, but function local constants cannot obscure global constants; therefore, do this before top-level compilation
    /// A lightweight scan lets later function compilation detect global conflicts.
    /// It does not enter function bodies, so same-named local constants in separate
    /// functions are never mistaken for conflicts.
    fn prepare_global_constants(&mut self, statements: &[Statement]) -> Result<(), CompileError> {
        if self.is_function_scope {
            return Ok(());
        }
        let mut constants = HashSet::new();
        Self::collect_scope_constants(&self.chunk.source_file, statements, &mut constants)?;
        self.global_constants = constants;
        Ok(())
    }

    /// Collects constant bindings in the current scope.
    fn collect_scope_constants(
        source_file: &str,
        statements: &[Statement],
        constants: &mut HashSet<String>,
    ) -> Result<(), CompileError> {
        for statement in statements {
            match statement {
                Statement::Let(name, value) => {
                    Self::validate_mutable_binding_name_at(
                        source_file,
                        name,
                        Self::span_for_statement(source_file, statement),
                        "let variable",
                    )?;
                    Self::collect_expr_constants(source_file, value, constants)?;
                }
                Statement::Fn(name, _, _) => {
                    Self::collect_binding_constant(
                        source_file,
                        name,
                        Self::span_for_statement(source_file, statement),
                        constants,
                    )?;
                }
                Statement::Class(name, members) => {
                    Self::collect_binding_constant(
                        source_file,
                        name,
                        Self::span_for_statement(source_file, statement),
                        constants,
                    )?;
                    for (_, statement) in members.values() {
                        match statement {
                            Statement::Expr(expr) => {
                                Self::collect_expr_constants(source_file, expr, constants)?;
                            }
                            Statement::Fn(_, _, _) | Statement::Empty => {}
                            other => Self::collect_scope_constants(
                                source_file,
                                std::slice::from_ref(other),
                                constants,
                            )?,
                        }
                    }
                }
                Statement::Declare(names, span) => {
                    for name in names {
                        Self::validate_mutable_binding_name_at(
                            source_file,
                            name,
                            Self::span_from_expr(span),
                            "variable declaration",
                        )?;
                    }
                }
                Statement::Assign(target, value) => {
                    Self::collect_target_constants(source_file, target, constants)?;
                    Self::collect_expr_constants(source_file, value, constants)?;
                }
                Statement::Expr(expr)
                | Statement::Print(expr)
                | Statement::Println(expr)
                | Statement::Return(expr) => {
                    Self::collect_expr_constants(source_file, expr, constants)?;
                }
                Statement::Use(object, _) => {
                    Self::collect_expr_constants(source_file, object, constants)?;
                }
                Statement::If(condition, true_body, else_body) => {
                    Self::collect_expr_constants(source_file, condition, constants)?;
                    Self::collect_scope_constants(source_file, true_body, constants)?;
                    if let Some(else_body) = else_body {
                        Self::collect_scope_constants(source_file, else_body, constants)?;
                    }
                }
                Statement::Try(_, try_body, _, catch_body) => {
                    Self::collect_scope_constants(source_file, try_body, constants)?;
                    Self::collect_scope_constants(source_file, catch_body, constants)?;
                }
                Statement::Throw(expr) => {
                    Self::collect_expr_constants(source_file, expr, constants)?;
                }
                Statement::For(_, key, value, iter, step, body) => {
                    if !Self::is_discard_binding(key) {
                        Self::validate_mutable_binding_name_at(
                            source_file,
                            key,
                            Self::span_for_statement(source_file, statement),
                            "for key variable",
                        )?;
                    }
                    if !Self::is_discard_binding(value) {
                        Self::validate_mutable_binding_name_at(
                            source_file,
                            value,
                            Self::span_for_statement(source_file, statement),
                            "for value variable",
                        )?;
                    }
                    Self::collect_expr_constants(source_file, iter, constants)?;
                    if let Some(step) = step {
                        Self::collect_expr_constants(source_file, step, constants)?;
                    }
                    Self::collect_scope_constants(source_file, body, constants)?;
                }
                Statement::ForCount(_, count, step, body) => {
                    Self::collect_expr_constants(source_file, count, constants)?;
                    if let Some(step) = step {
                        Self::collect_expr_constants(source_file, step, constants)?;
                    }
                    Self::collect_scope_constants(source_file, body, constants)?;
                }
                Statement::ForRange(_, key, value, start, end, step, body) => {
                    if !Self::is_discard_binding(key) {
                        Self::validate_mutable_binding_name_at(
                            source_file,
                            key,
                            Self::span_for_statement(source_file, statement),
                            "for interval key variable",
                        )?;
                    }
                    if !Self::is_discard_binding(value) {
                        Self::validate_mutable_binding_name_at(
                            source_file,
                            value,
                            Self::span_for_statement(source_file, statement),
                            "for interval variable",
                        )?;
                    }
                    if let Some(start) = start {
                        Self::collect_expr_constants(source_file, start, constants)?;
                    }
                    if let Some(end) = end {
                        Self::collect_expr_constants(source_file, end, constants)?;
                    }
                    if let Some(step) = step {
                        Self::collect_expr_constants(source_file, step, constants)?;
                    }
                    Self::collect_scope_constants(source_file, body, constants)?;
                }
                Statement::ForDestructure(_, names, iter, body) => {
                    for name in names {
                        Self::validate_mutable_binding_name_at(
                            source_file,
                            name,
                            Self::span_for_statement(source_file, statement),
                            "for destructuring variable",
                        )?;
                    }
                    Self::collect_expr_constants(source_file, iter, constants)?;
                    Self::collect_scope_constants(source_file, body, constants)?;
                }
                Statement::While(_, condition, body) => {
                    Self::collect_expr_constants(source_file, condition, constants)?;
                    Self::collect_scope_constants(source_file, body, constants)?;
                }
                Statement::Loop(_, body) => {
                    Self::collect_scope_constants(source_file, body, constants)?;
                }
                Statement::Empty | Statement::Break(_) | Statement::Continue(_) => {}
            }
        }
        Ok(())
    }

    /// Collects the constant bindings for the current scope from the expression tree.
    fn collect_expr_constants(
        source_file: &str,
        expr: &PosExpr,
        constants: &mut HashSet<String>,
    ) -> Result<(), CompileError> {
        match &expr.expr {
            Expr::Assign(target, value) => {
                Self::collect_target_constants(source_file, target, constants)?;
                Self::collect_expr_constants(source_file, value, constants)?;
            }
            Expr::Binary(left, _, right)
            | Expr::PlusAssign(left, right)
            | Expr::MinusAssign(left, right)
            | Expr::MultiplyAssign(left, right)
            | Expr::DivideAssign(left, right)
            | Expr::ModuloAssign(left, right)
            | Expr::ShiftLeftAssign(left, right)
            | Expr::ShiftRightAssign(left, right)
            | Expr::BitAndAssign(left, right)
            | Expr::BitXorAssign(left, right)
            | Expr::BitOrAssign(left, right) => {
                Self::collect_expr_constants(source_file, left, constants)?;
                Self::collect_expr_constants(source_file, right, constants)?;
            }
            Expr::Not(inner)
            | Expr::BitNot(inner)
            | Expr::PostfixIncrement(inner)
            | Expr::PostfixDecrement(inner)
            | Expr::PrefixIncrement(inner)
            | Expr::PrefixDecrement(inner) => {
                Self::collect_expr_constants(source_file, inner, constants)?;
            }
            Expr::Vec(items) | Expr::Array(items) | Expr::ObjectProperty(items) => {
                for item in items {
                    Self::collect_expr_constants(source_file, item, constants)?;
                }
            }
            Expr::Object(entries) => {
                for (_, value) in entries {
                    Self::collect_expr_constants(source_file, value, constants)?;
                }
            }
            Expr::Call(callee, args) | Expr::New(_, callee, args) => {
                Self::collect_expr_constants(source_file, callee, constants)?;
                for arg in args {
                    Self::collect_expr_constants(source_file, arg, constants)?;
                }
            }
            Expr::Statement(statements) => {
                Self::collect_scope_constants(source_file, statements, constants)?;
            }
            Expr::Match(target, arms) => {
                Self::collect_expr_constants(source_file, target, constants)?;
                for arm in arms {
                    if let Some(pattern) = &arm.pattern {
                        Self::collect_expr_constants(source_file, pattern, constants)?;
                    }
                    Self::collect_expr_constants(source_file, &arm.value, constants)?;
                }
            }
            Expr::Fn(_, _, _) => {}
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Strs(_)
            | Expr::Variable(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Empty
            | Expr::Destructure(_)
            | Expr::Regex(_, _) => {}
        }
        Ok(())
    }

    /// Collects constant bindings from assignment targets.
    fn collect_target_constants(
        source_file: &str,
        target: &PosExpr,
        constants: &mut HashSet<String>,
    ) -> Result<(), CompileError> {
        match &target.expr {
            Expr::Variable(name) => Self::collect_binding_constant(
                source_file,
                name,
                Self::span_from_expr(target),
                constants,
            ),
            Expr::Destructure(names) => {
                for name in names {
                    Self::collect_binding_constant(
                        source_file,
                        name,
                        Self::span_from_expr(target),
                        constants,
                    )?;
                }
                Ok(())
            }
            Expr::ObjectProperty(_) => Ok(()),
            _ => Ok(()),
        }
    }

    /// If the name conforms to the constant rules, the current scope constant collection is written.
    fn collect_binding_constant(
        source_file: &str,
        name: &str,
        span: SourceSpan,
        constants: &mut HashSet<String>,
    ) -> Result<(), CompileError> {
        if Self::binding_name_kind_at(source_file, name, span)? == BindingNameKind::Constant {
            constants.insert(name.to_string());
        }
        Ok(())
    }

    /// Verifies and classifies a binding name.
    fn binding_name_kind(
        &self,
        name: &str,
        span: SourceSpan,
    ) -> Result<BindingNameKind, CompileError> {
        Self::binding_name_kind_at(&self.chunk.source_file, name, span)
    }

    /// Verifies and classifies a binding name based on its source code location.
    fn binding_name_kind_at(
        source_file: &str,
        name: &str,
        span: SourceSpan,
    ) -> Result<BindingNameKind, CompileError> {
        let Some(first) = name.as_bytes().first().copied() else {
            return Err(Self::compile_error(
                source_file,
                span,
                "Identifier cannot be empty".to_string(),
            ));
        };
        if first.is_ascii_uppercase() {
            if name
                .as_bytes()
                .get(1..)
                .unwrap_or_default()
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                return Ok(BindingNameKind::Constant);
            }
            return Err(Self::compile_error(
                source_file,
                span,
                format!(
                    "The identifier `{}` will be regarded as a constant when the first letter is capitalized. The constant name must match [A-Z][A-Za-z0-9_]*",
                    name
                ),
            ));
        }
        Ok(BindingNameKind::Variable)
    }

    /// Validates an ordinary mutable binding name.
    fn validate_mutable_binding_name(
        &self,
        name: &str,
        span: SourceSpan,
        context: &str,
    ) -> Result<(), CompileError> {
        Self::validate_mutable_binding_name_at(&self.chunk.source_file, name, span, context)
    }

    /// Verify common variable variable names based on source code location.
    fn validate_mutable_binding_name_at(
        source_file: &str,
        name: &str,
        span: SourceSpan,
        context: &str,
    ) -> Result<(), CompileError> {
        match Self::binding_name_kind_at(source_file, name, span.clone())? {
            BindingNameKind::Variable => Ok(()),
            BindingNameKind::Constant => Err(Self::compile_error(
                source_file,
                span,
                format!(
                    "{} `{}` cannot start with an uppercase letter; names matching [A-Z][A-Za-z0-9_]* will be treated as constants",
                    context, name
                ),
            )),
        }
    }

    /// Defines a constant name in the current scope.
    fn define_constant(&mut self, name: &str, span: SourceSpan) -> Result<(), CompileError> {
        if self.is_function_scope && self.global_constants.contains(name) {
            return Err(Self::compile_error(
                &self.chunk.source_file,
                span,
                format!("Constant `{}` is already defined in global scope and cannot be shadowed inside a function", name),
            ));
        }
        if !self.defined_constants.insert(name.to_string()) {
            return Err(Self::compile_error(
                &self.chunk.source_file,
                span,
                format!("Constant `{}` is already defined", name),
            ));
        }
        Ok(())
    }

    /// Compiled variable or constant binding writing.
    fn compile_binding_store(
        &mut self,
        name: &str,
        src: Register,
        span: SourceSpan,
        force_local: bool,
    ) -> Result<Instruction, CompileError> {
        let kind = self.binding_name_kind(name, span.clone())?;
        let symbol = self.chunk.symbols.intern(name);
        match kind {
            BindingNameKind::Variable => {
                if force_local {
                    self.chunk.mark_local(symbol);
                }
                Ok(Instruction::StoreGlobal { symbol, src })
            }
            BindingNameKind::Constant => {
                self.define_constant(name, span)?;
                if self.is_function_scope || force_local {
                    self.chunk.mark_local(symbol);
                }
                Ok(Instruction::StoreConst { symbol, src })
            }
        }
    }

    /// Compiled `let` or empty declarations such as ordinary mutable variable writes.
    ///
    /// Constants are not allowed to be created via `let` or uninitialized declarations; this path only validates variable names and generates ordinary variable writes.
    fn compile_mutable_binding_store(
        &mut self,
        name: &str,
        src: Register,
        span: SourceSpan,
        force_local: bool,
        context: &str,
    ) -> Result<Instruction, CompileError> {
        self.validate_mutable_binding_name(name, span, context)?;
        let symbol = self.chunk.symbols.intern(name);
        if force_local {
            self.chunk.mark_local(symbol);
        }
        Ok(Instruction::StoreGlobal { symbol, src })
    }

    /// Construction compilation error.
    fn compile_error(source_file: &str, span: SourceSpan, message: String) -> CompileError {
        CompileError {
            file: if span.file.is_empty() {
                source_file.to_string()
            } else {
                span.file
            },
            line: span.line,
            column: span.column,
            message,
        }
    }

    /// Reads a statement's source location, falling back to the start of the file.
    fn span_for_statement(source_file: &str, statement: &Statement) -> SourceSpan {
        Self::span_from_statement(statement).unwrap_or_else(|| SourceSpan {
            file: source_file.to_string(),
            line: 1,
            column: 1,
        })
    }

    /// Compiles a single statement.
    fn compile_statement(&mut self, statement: &Statement) -> Result<(), CompileError> {
        match statement {
            Statement::Empty => Ok(()),
            Statement::Expr(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(Instruction::Pop { src }, expr);
                Ok(())
            }
            Statement::Print(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(
                    Instruction::Print {
                        src,
                        newline: false,
                    },
                    expr,
                );
                Ok(())
            }
            Statement::Println(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(Instruction::Print { src, newline: true }, expr);
                Ok(())
            }
            Statement::Assign(target, value) => {
                self.compile_assign_expr(target, value)?;
                Ok(())
            }
            Statement::Let(name, value) => {
                let src = self.compile_expr(value)?;
                let instruction = self.compile_mutable_binding_store(
                    name,
                    src,
                    Self::span_from_expr(value),
                    true,
                    "let variable",
                )?;
                self.emit_expr(instruction, value);
                Ok(())
            }
            Statement::Declare(names, span) => {
                let src = self.emit_constant(Value::Empty)?;
                for name in names {
                    let instruction = self.compile_mutable_binding_store(
                        name,
                        src,
                        Self::span_from_expr(span),
                        true,
                        "variable declaration",
                    )?;
                    self.emit_expr(instruction, span);
                }
                Ok(())
            }
            Statement::Fn(name, params, body) => {
                let dst = self.compile_function_value(name, params, body)?;
                let instruction = self.compile_binding_store(
                    name,
                    dst,
                    Self::span_for_statement(&self.chunk.source_file, statement),
                    true,
                )?;
                self.emit_statement(instruction, statement);
                Ok(())
            }
            Statement::Class(name, members) => {
                let dst = self.compile_class_value(name, members)?;
                let instruction = self.compile_binding_store(
                    name,
                    dst,
                    Self::span_for_statement(&self.chunk.source_file, statement),
                    true,
                )?;
                self.emit_statement(instruction, statement);
                Ok(())
            }
            Statement::If(condition, true_body, else_body) => {
                self.compile_if(condition, true_body, else_body.as_deref())
            }
            Statement::Try(span, try_body, error, catch_body) => {
                self.compile_try(span, try_body, error, catch_body)
            }
            Statement::For(label, key, value, iter, step, body) => {
                self.compile_for(label, key, value, iter, step.as_ref(), body)
            }
            Statement::ForCount(label, count, step, body) => {
                self.compile_for_count(label, count, step.as_ref(), body)
            }
            Statement::ForRange(label, key, value, start, end, step, body) => self
                .compile_for_range(
                    label,
                    key,
                    value,
                    start.as_ref(),
                    end.as_ref(),
                    step.as_ref(),
                    body,
                ),
            Statement::ForDestructure(label, names, iter, body) => {
                self.compile_for_destructure(label, names, iter, body)
            }
            Statement::While(label, condition, body) => {
                let loop_start = self.chunk.code.len() as u32;
                let condition_register = self.compile_expr(condition)?;
                let jump_to_end = self.emit_jump_if_false(condition_register, condition);
                self.push_loop(label, loop_start);
                self.compile_block(body)?;
                self.finish_loop(loop_start, None);
                self.emit_statement(Instruction::Jump { target: loop_start }, statement);
                self.patch_jump(jump_to_end);
                let loop_end = self.chunk.code.len() as u32;
                self.patch_finished_loop(loop_end);
                Ok(())
            }
            Statement::Loop(label, body) => {
                let loop_start = self.chunk.code.len() as u32;
                self.push_loop(label, loop_start);
                self.compile_block(body)?;
                self.finish_loop(loop_start, None);
                self.emit_statement(Instruction::Jump { target: loop_start }, statement);
                let loop_end = self.chunk.code.len() as u32;
                self.patch_finished_loop(loop_end);
                Ok(())
            }
            Statement::Return(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(Instruction::Return { src }, expr);
                Ok(())
            }
            Statement::Throw(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(Instruction::Throw { src }, expr);
                Ok(())
            }
            Statement::Break(label) => self.compile_loop_jump(label, true),
            Statement::Continue(label) => self.compile_loop_jump(label, false),
            Statement::Use(object, imports) => self.compile_use_statement(object, imports),
        }
    }

    /// Compiles the expression and returns the register holding the result.
    fn compile_expr(&mut self, expr: &PosExpr) -> Result<Register, CompileError> {
        match &expr.expr {
            Expr::Int(value) => self.emit_constant(Value::Int(*value)),
            Expr::Float(value) => self.emit_constant(Value::Float(*value)),
            Expr::Str(value) => self.emit_constant(Value::Str(value.clone())),
            Expr::Strs(value) => {
                let src = self.emit_constant(Value::Str(value.clone()))?;
                let dst = self.alloc_register();
                self.emit_expr(Instruction::ExpandTemplate { dst, src }, expr);
                Ok(dst)
            }
            Expr::Bool(value) => self.emit_constant(Value::Bool(*value)),
            Expr::Null => self.emit_constant(Value::Null),
            Expr::Empty => self.emit_constant(Value::Empty),
            Expr::Variable(name) => {
                self.binding_name_kind(name, Self::span_from_expr(expr))?;
                let dst = self.alloc_register();
                let symbol = self.chunk.symbols.intern(name);
                self.emit_expr(Instruction::LoadGlobal { dst, symbol }, expr);
                Ok(dst)
            }
            Expr::Binary(left, op, right) => {
                if matches!(
                    op.kind,
                    TokenKind::And | TokenKind::Or | TokenKind::Coalesce
                ) {
                    return self.compile_short_circuit_expr(left, &op.kind, right, expr);
                }
                let lhs = self.compile_expr(left)?;
                let rhs = self.compile_expr(right)?;
                let dst = self.alloc_register();
                self.emit_expr(
                    Instruction::Binary {
                        op: op.kind.clone(),
                        dst,
                        lhs,
                        rhs,
                    },
                    expr,
                );
                Ok(dst)
            }
            Expr::Not(inner) => {
                let src = self.compile_expr(inner)?;
                let dst = self.alloc_register();
                self.emit_expr(Instruction::Not { dst, src }, expr);
                Ok(dst)
            }
            Expr::BitNot(inner) => {
                let src = self.compile_expr(inner)?;
                let dst = self.alloc_register();
                self.emit_expr(Instruction::BitNot { dst, src }, expr);
                Ok(dst)
            }
            Expr::Array(items) => {
                let mut registers = Vec::with_capacity(items.len());
                for item in items {
                    registers.push(self.compile_expr(item)?);
                }
                let dst = self.alloc_register();
                self.emit_expr(
                    Instruction::MakeArray {
                        dst,
                        items: registers,
                    },
                    expr,
                );
                Ok(dst)
            }
            Expr::Object(entries) => {
                let mut compiled = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let symbol = self.chunk.symbols.intern(key);
                    let value = self.compile_expr(value)?;
                    compiled.push((symbol, value));
                }
                let dst = self.alloc_register();
                self.emit_expr(
                    Instruction::MakeObject {
                        dst,
                        entries: compiled,
                    },
                    expr,
                );
                Ok(dst)
            }
            Expr::ObjectProperty(items) => self.compile_property_chain(items, expr),
            Expr::Call(callee, args) => {
                let callee = self.compile_expr(callee)?;
                let mut compiled_args = Vec::with_capacity(args.len());
                for arg in args {
                    compiled_args.push(self.compile_expr(arg)?);
                }
                let dst = self.alloc_register();
                self.emit_expr(
                    Instruction::Call {
                        dst,
                        callee,
                        args: compiled_args,
                    },
                    expr,
                );
                Ok(dst)
            }
            Expr::Fn(name, params, body) => self.compile_function_value(name, params, body),
            Expr::New(name, class_expr, args) => {
                let callee = self.compile_expr(class_expr)?;
                let method_name = self.emit_constant(Value::Str(name.clone()))?;
                let method = self.alloc_register();
                self.emit_expr(
                    Instruction::GetProperty {
                        dst: method,
                        object: callee,
                        key: method_name,
                    },
                    expr,
                );
                let mut compiled_args = Vec::with_capacity(args.len());
                for arg in args {
                    compiled_args.push(self.compile_expr(arg)?);
                }
                let dst = self.alloc_register();
                self.emit_expr(
                    Instruction::Call {
                        dst,
                        callee: method,
                        args: compiled_args,
                    },
                    expr,
                );
                Ok(dst)
            }
            Expr::Statement(statements) => self.compile_block_value(statements),
            Expr::Match(target, arms) => self.compile_match_value(target, arms, expr),
            Expr::Vec(items) => {
                let mut last = self.emit_constant(Value::Empty)?;
                for item in items {
                    last = self.compile_expr(item)?;
                }
                Ok(last)
            }
            Expr::Destructure(_) => Err(CompileError {
                file: expr.file.clone(),
                line: expr.line,
                column: expr.column,
                message: "Destructuring bindings can only appear on the left side of an assignment"
                    .to_string(),
            }),
            Expr::Regex(pattern, flags) => {
                let (pattern, flags) = Self::split_regex_literal(pattern, flags);
                let regex = regex::RegexBuilder::new(&pattern)
                    .case_insensitive(flags.contains('i'))
                    .multi_line(flags.contains('m'))
                    .dot_matches_new_line(flags.contains('s'))
                    .build()
                    .map_err(|err| CompileError {
                        file: expr.file.clone(),
                        line: expr.line,
                        column: expr.column,
                        message: format!("Regular expression compilation failed: {}", err),
                    })?;
                self.emit_constant(Value::Regex(std::rc::Rc::new(regex), pattern, flags))
            }
            Expr::PostfixIncrement(inner) => self.compile_increment(inner, 1, false, expr),
            Expr::PostfixDecrement(inner) => self.compile_increment(inner, -1, false, expr),
            Expr::PrefixIncrement(inner) => self.compile_increment(inner, 1, true, expr),
            Expr::PrefixDecrement(inner) => self.compile_increment(inner, -1, true, expr),
            Expr::Assign(left, right) => self.compile_assign_expr(left, right),
            Expr::PlusAssign(left, right)
            | Expr::MinusAssign(left, right)
            | Expr::MultiplyAssign(left, right)
            | Expr::DivideAssign(left, right)
            | Expr::ModuloAssign(left, right)
            | Expr::ShiftLeftAssign(left, right)
            | Expr::ShiftRightAssign(left, right)
            | Expr::BitAndAssign(left, right)
            | Expr::BitXorAssign(left, right)
            | Expr::BitOrAssign(left, right) => {
                self.compile_compound_assign(&expr.expr, left, right, expr)
            }
        }
    }

    /// Writing constant load instructions.
    fn emit_constant(&mut self, value: Value) -> Result<Register, CompileError> {
        let dst = self.alloc_register();
        let constant = self.chunk.add_constant(value);
        self.chunk.emit(Instruction::LoadConst { dst, constant });
        Ok(dst)
    }

    /// Emits an instruction at an expression's source location.
    fn emit_expr(&mut self, instruction: Instruction, expr: &PosExpr) {
        self.chunk.emit_at(instruction, Self::span_from_expr(expr));
    }

    /// Emits an instruction at a statement's source location.
    fn emit_statement(&mut self, instruction: Instruction, statement: &Statement) {
        if let Some(span) = Self::span_from_statement(statement) {
            self.chunk.emit_at(instruction, span);
        } else {
            self.chunk.emit(instruction);
        }
    }

    /// Extracts the source code location from the expression.
    fn span_from_expr(expr: &PosExpr) -> SourceSpan {
        SourceSpan {
            file: expr.file.clone(),
            line: expr.line,
            column: expr.column,
        }
    }

    /// Extracts the source code location from the statement.
    fn span_from_statement(statement: &Statement) -> Option<SourceSpan> {
        let expr = match statement {
            Statement::Expr(expr)
            | Statement::Print(expr)
            | Statement::Println(expr)
            | Statement::Assign(expr, _)
            | Statement::Let(_, expr)
            | Statement::Return(expr)
            | Statement::Throw(expr)
            | Statement::If(expr, _, _)
            | Statement::Try(expr, _, _, _)
            | Statement::For(_, _, _, expr, _, _)
            | Statement::ForCount(_, expr, _, _)
            | Statement::ForDestructure(_, _, expr, _)
            | Statement::While(_, expr, _) => Some(expr),
            Statement::ForRange(_, _, _, start, end, step, _) => {
                start.as_ref().or(end.as_ref()).or(step.as_ref())
            }
            _ => None,
        }?;
        Some(Self::span_from_expr(expr))
    }

    /// Allocates a new virtual register.
    fn alloc_register(&mut self) -> Register {
        let register = self.next_register;
        self.next_register = self
            .next_register
            .checked_add(1)
            .expect("virtual register count exceeds the u16 limit");
        register
    }

    /// Compile continuous statement blocks.
    fn compile_block(&mut self, statements: &[Statement]) -> Result<(), CompileError> {
        for statement in statements {
            self.compile_statement(statement)?;
        }
        Ok(())
    }

    /// Compiles a block of statements and returns the value of the last statement.
    ///
    /// BT code blocks and function bodies have expression semantics: without an explicit `return`, the last statement supplies the block's value.
    /// An empty block returns `empty`, giving functions, if branches, and arrow-function bodies the same behavior.
    fn compile_block_value(&mut self, statements: &[Statement]) -> Result<Register, CompileError> {
        let Some((last, prefix)) = statements.split_last() else {
            return self.emit_constant(Value::Empty);
        };
        for statement in prefix {
            self.compile_statement(statement)?;
        }
        self.compile_statement_value(last)
    }

    /// Compiles a single statement and returns its expression value.
    ///
    /// Used for implicit return of the last statement. Statements retain their side
    /// effects while exposing natural results: assignments return their right-hand
    /// value, and function or class declarations return the declared value.
    fn compile_statement_value(&mut self, statement: &Statement) -> Result<Register, CompileError> {
        match statement {
            Statement::Empty => self.emit_constant(Value::Empty),
            Statement::Use(object, imports) => {
                self.compile_use_statement(object, imports)?;
                self.emit_constant(Value::Empty)
            }
            Statement::Expr(expr) => self.compile_expr(expr),
            Statement::Print(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(
                    Instruction::Print {
                        src,
                        newline: false,
                    },
                    expr,
                );
                Ok(src)
            }
            Statement::Println(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(Instruction::Print { src, newline: true }, expr);
                Ok(src)
            }
            Statement::Assign(target, value) => self.compile_assign_expr(target, value),
            Statement::Let(name, value) => {
                let src = self.compile_expr(value)?;
                let instruction = self.compile_mutable_binding_store(
                    name,
                    src,
                    Self::span_from_expr(value),
                    true,
                    "let variable",
                )?;
                self.emit_expr(instruction, value);
                Ok(src)
            }
            Statement::Declare(names, span) => {
                let src = self.emit_constant(Value::Empty)?;
                for name in names {
                    let instruction = self.compile_mutable_binding_store(
                        name,
                        src,
                        Self::span_from_expr(span),
                        true,
                        "variable declaration",
                    )?;
                    self.emit_expr(instruction, span);
                }
                Ok(src)
            }
            Statement::Fn(name, params, body) => {
                let dst = self.compile_function_value(name, params, body)?;
                let instruction = self.compile_binding_store(
                    name,
                    dst,
                    Self::span_for_statement(&self.chunk.source_file, statement),
                    true,
                )?;
                self.emit_statement(instruction, statement);
                Ok(dst)
            }
            Statement::Class(name, members) => {
                let dst = self.compile_class_value(name, members)?;
                let instruction = self.compile_binding_store(
                    name,
                    dst,
                    Self::span_for_statement(&self.chunk.source_file, statement),
                    true,
                )?;
                self.emit_statement(instruction, statement);
                Ok(dst)
            }
            Statement::If(condition, true_body, else_body) => {
                self.compile_if_value(condition, true_body, else_body.as_deref())
            }
            Statement::Try(span, try_body, error, catch_body) => {
                self.compile_try_value(span, try_body, error, catch_body)
            }
            Statement::Return(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(Instruction::Return { src }, expr);
                Ok(src)
            }
            Statement::Throw(expr) => {
                let src = self.compile_expr(expr)?;
                self.emit_expr(Instruction::Throw { src }, expr);
                self.emit_constant(Value::Empty)
            }
            Statement::For(_, _, _, _, _, _)
            | Statement::ForCount(_, _, _, _)
            | Statement::ForRange(_, _, _, _, _, _, _)
            | Statement::ForDestructure(_, _, _, _)
            | Statement::While(_, _, _)
            | Statement::Loop(_, _)
            | Statement::Break(_)
            | Statement::Continue(_) => {
                self.compile_statement(statement)?;
                self.emit_constant(Value::Empty)
            }
        }
    }

    /// Compiles `use obj` and `use obj{a,b}` field imports.
    ///
    /// `use` performs scope destructuring rather than module loading. Named fields
    /// become variables in the current scope; without a field list, runtime object
    /// fields are imported, which suits framework-injected Web request contexts.
    fn compile_use_statement(
        &mut self,
        object: &PosExpr,
        imports: &Option<Vec<String>>,
    ) -> Result<(), CompileError> {
        let object_expr = object;
        let object = self.compile_expr(object_expr)?;
        let fields = imports
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .map(|name| {
                        self.validate_mutable_binding_name(
                            name,
                            Self::span_from_expr(object_expr),
                            "use imported variable",
                        )?;
                        Ok(self.chunk.symbols.intern(name))
                    })
                    .collect::<Result<Vec<_>, CompileError>>()
            })
            .transpose()?
            .unwrap_or_default();
        self.emit_expr(Instruction::UseFields { object, fields }, object_expr);
        Ok(())
    }

    /// Compiles an `if` statement.
    fn compile_if(
        &mut self,
        condition: &PosExpr,
        true_body: &[Statement],
        else_body: Option<&[Statement]>,
    ) -> Result<(), CompileError> {
        let condition_register = self.compile_expr(condition)?;
        let jump_to_else = self.emit_jump_if_false(condition_register, condition);
        self.compile_block(true_body)?;
        let jump_to_end = self.emit_jump();
        self.patch_jump(jump_to_else);
        if let Some(else_body) = else_body {
            self.compile_block(else_body)?;
        }
        self.patch_jump(jump_to_end);
        Ok(())
    }

    /// Compiles a conditional expression and returns the value of the branch taken.
    /// Each branch produces a register value that is copied into a shared target, so
    /// `if` can participate in assignments, arguments, and implicit returns.
    fn compile_if_value(
        &mut self,
        condition: &PosExpr,
        true_body: &[Statement],
        else_body: Option<&[Statement]>,
    ) -> Result<Register, CompileError> {
        let dst = self.alloc_register();
        let condition_register = self.compile_expr(condition)?;
        let jump_to_else = self.emit_jump_if_false(condition_register, condition);
        let true_value = self.compile_block_value(true_body)?;
        self.emit_expr(
            Instruction::Move {
                dst,
                src: true_value,
            },
            condition,
        );
        let jump_to_end = self.emit_jump();
        self.patch_jump(jump_to_else);
        let false_value = if let Some(else_body) = else_body {
            self.compile_block_value(else_body)?
        } else {
            self.emit_constant(Value::Empty)?
        };
        self.emit_expr(
            Instruction::Move {
                dst,
                src: false_value,
            },
            condition,
        );
        self.patch_jump(jump_to_end);
        Ok(dst)
    }

    /// Compile try/catch statements.
    fn compile_try(
        &mut self,
        span: &PosExpr,
        try_body: &[Statement],
        error: &str,
        catch_body: &[Statement],
    ) -> Result<(), CompileError> {
        let error_symbol = self.compile_catch_symbol(error, span)?;
        let enter_try = self.chunk.code.len();
        self.emit_expr(
            Instruction::EnterTry {
                catch_target: 0,
                error_symbol,
            },
            span,
        );
        self.try_depth += 1;
        let result = self.compile_block(try_body);
        self.try_depth = self.try_depth.saturating_sub(1);
        result?;
        self.emit_expr(Instruction::LeaveTry, span);
        let jump_to_end = self.emit_jump();
        self.patch_try_catch(enter_try);
        self.compile_block(catch_body)?;
        self.patch_jump(jump_to_end);
        Ok(())
    }

    /// Compiles try/catch expressions that return values.
    fn compile_try_value(
        &mut self,
        span: &PosExpr,
        try_body: &[Statement],
        error: &str,
        catch_body: &[Statement],
    ) -> Result<Register, CompileError> {
        let dst = self.alloc_register();
        let error_symbol = self.compile_catch_symbol(error, span)?;
        let enter_try = self.chunk.code.len();
        self.emit_expr(
            Instruction::EnterTry {
                catch_target: 0,
                error_symbol,
            },
            span,
        );
        self.try_depth += 1;
        let try_value = self.compile_block_value(try_body);
        self.try_depth = self.try_depth.saturating_sub(1);
        let try_value = try_value?;
        self.emit_expr(
            Instruction::Move {
                dst,
                src: try_value,
            },
            span,
        );
        self.emit_expr(Instruction::LeaveTry, span);
        let jump_to_end = self.emit_jump();
        self.patch_try_catch(enter_try);
        let catch_value = self.compile_block_value(catch_body)?;
        self.emit_expr(
            Instruction::Move {
                dst,
                src: catch_value,
            },
            span,
        );
        self.patch_jump(jump_to_end);
        Ok(dst)
    }

    /// Verifies the catch variable name and returns the symbol number.
    fn compile_catch_symbol(
        &mut self,
        error: &str,
        span: &PosExpr,
    ) -> Result<SymbolId, CompileError> {
        self.validate_mutable_binding_name(error, Self::span_from_expr(span), "catch variable")?;
        let symbol = self.chunk.symbols.intern(error);
        self.chunk.mark_local(symbol);
        Ok(symbol)
    }

    /// Compiles a match expression.
    fn compile_match_value(
        &mut self,
        target: &PosExpr,
        arms: &[MatchArm],
        span: &PosExpr,
    ) -> Result<Register, CompileError> {
        let target = self.compile_expr(target)?;
        let dst = self.alloc_register();
        let mut jumps_to_end = Vec::new();
        let mut has_default = false;

        for arm in arms {
            let Some(pattern) = &arm.pattern else {
                has_default = true;
                let value = self.compile_expr(&arm.value)?;
                self.emit_expr(Instruction::Move { dst, src: value }, &arm.value);
                jumps_to_end.push(self.emit_jump());
                continue;
            };
            let pattern_value = self.compile_expr(pattern)?;
            let matched = self.alloc_register();
            self.emit_expr(
                Instruction::Binary {
                    op: TokenKind::Equal,
                    dst: matched,
                    lhs: target,
                    rhs: pattern_value,
                },
                pattern,
            );
            let jump_to_next = self.emit_jump_if_false(matched, pattern);
            let value = self.compile_expr(&arm.value)?;
            self.emit_expr(Instruction::Move { dst, src: value }, &arm.value);
            jumps_to_end.push(self.emit_jump());
            self.patch_jump(jump_to_next);
        }

        if !has_default {
            let fallback = self.emit_constant(Value::Empty)?;
            self.emit_expr(Instruction::Move { dst, src: fallback }, span);
        }
        for jump in jumps_to_end {
            self.patch_jump(jump);
        }
        Ok(dst)
    }

    /// Compiles a `for key, value in iter { ... }` loop.
    ///
    /// VM handles array, object and string traversal in a unified manner through the two instructions `IterInit` / `IterNext`; only when
    /// `step` is explicitly written out does it switch to a strict integer-number iterator, to avoid affecting the loose semantics of ordinary collection traversal.
    fn compile_for(
        &mut self,
        label: &str,
        key: &str,
        value: &str,
        iter: &PosExpr,
        step: Option<&PosExpr>,
        body: &[Statement],
    ) -> Result<(), CompileError> {
        let iterable = self.compile_expr(iter)?;
        let iterator = self.alloc_register();
        if let Some(step) = step {
            let step_register = self.compile_expr(step)?;
            self.emit_expr(
                Instruction::CountInit {
                    dst: iterator,
                    count: iterable,
                    step: Some(step_register),
                },
                iter,
            );
        } else {
            self.emit_expr(
                Instruction::IterInit {
                    dst: iterator,
                    iterable,
                },
                iter,
            );
        }
        let loop_start = self.chunk.code.len() as u32;
        let key_symbol = self.compile_for_binding_symbol(key, iter, "for key variable")?;
        let value_symbol = self.compile_for_binding_symbol(value, iter, "for value variable")?;
        let iter_next = self.chunk.code.len();
        self.emit_expr(
            Instruction::IterNext {
                iterator,
                key_symbol,
                value_symbol,
                jump_to_end: 0,
            },
            iter,
        );
        self.push_loop(label, loop_start);
        self.compile_block(body)?;
        self.finish_loop(loop_start, None);
        self.emit_expr(Instruction::Jump { target: loop_start }, iter);
        let loop_end = self.chunk.code.len() as u32;
        if let Instruction::IterNext { jump_to_end, .. } = &mut self.chunk.code[iter_next] {
            *jump_to_end = loop_end;
        }
        self.patch_finished_loop(loop_end);
        Ok(())
    }

    /// Compiles a counted `for count { ... }` loop.
    ///
    /// This form repeats the body without creating a loop variable. Runtime requires
    /// a non-negative integer, so collections cannot be mistaken for counts. An
    /// optional `step` is not exposed as a variable and follows the same validation.
    fn compile_for_count(
        &mut self,
        label: &str,
        count: &PosExpr,
        step: Option<&PosExpr>,
        body: &[Statement],
    ) -> Result<(), CompileError> {
        let count_register = self.compile_expr(count)?;
        let step_register = step.map(|step| self.compile_expr(step)).transpose()?;
        let iterator = self.alloc_register();
        self.emit_expr(
            Instruction::CountInit {
                dst: iterator,
                count: count_register,
                step: step_register,
            },
            count,
        );
        let loop_start = self.chunk.code.len() as u32;
        let iter_next = self.chunk.code.len();
        self.emit_expr(
            Instruction::IterNext {
                iterator,
                key_symbol: None,
                value_symbol: None,
                jump_to_end: 0,
            },
            count,
        );
        self.push_loop(label, loop_start);
        self.compile_block(body)?;
        self.finish_loop(loop_start, None);
        self.emit_expr(Instruction::Jump { target: loop_start }, count);
        let loop_end = self.chunk.code.len() as u32;
        if let Instruction::IterNext { jump_to_end, .. } = &mut self.chunk.code[iter_next] {
            *jump_to_end = loop_end;
        }
        self.patch_finished_loop(loop_end);
        Ok(())
    }

    /// Compiles a `for i in a..b step n { ... }` range loop.
    ///
    /// The interval iterator only saves the current value, end point, direction and step size; even if the interval is large, it will not be pre-expanded like set traversal.
    fn compile_for_range(
        &mut self,
        label: &str,
        key: &str,
        value: &str,
        start: Option<&PosExpr>,
        end: Option<&PosExpr>,
        step: Option<&PosExpr>,
        body: &[Statement],
    ) -> Result<(), CompileError> {
        let span = start.or(end).or(step).ok_or_else(|| CompileError {
            file: self.chunk.source_file.clone(),
            line: 1,
            column: 1,
            message: "for interval requires at least one boundary".to_string(),
        })?;
        let start_register = if let Some(start) = start {
            self.compile_expr(start)?
        } else {
            self.emit_constant(Value::Int(0))?
        };
        let end_register = end.map(|end| self.compile_expr(end)).transpose()?;
        let step_register = if let Some(step) = step {
            self.compile_expr(step)?
        } else {
            self.emit_constant(Value::Int(1))?
        };
        let iterator = self.alloc_register();
        self.emit_expr(
            Instruction::RangeInit {
                dst: iterator,
                start: start_register,
                end: end_register,
                step: step_register,
            },
            span,
        );
        let loop_start = self.chunk.code.len() as u32;
        let key_symbol = self.compile_for_binding_symbol(key, span, "for interval key variable")?;
        let value_symbol = self.compile_for_binding_symbol(value, span, "for interval variable")?;
        let iter_next = self.chunk.code.len();
        self.emit_expr(
            Instruction::IterNext {
                iterator,
                key_symbol,
                value_symbol,
                jump_to_end: 0,
            },
            span,
        );
        self.push_loop(label, loop_start);
        self.compile_block(body)?;
        self.finish_loop(loop_start, None);
        self.emit_expr(Instruction::Jump { target: loop_start }, span);
        let loop_end = self.chunk.code.len() as u32;
        if let Instruction::IterNext { jump_to_end, .. } = &mut self.chunk.code[iter_next] {
            *jump_to_end = loop_end;
        }
        self.patch_finished_loop(loop_end);
        Ok(())
    }

    /// Compiles a loop binding; `_` or an empty name discards the current value.
    fn compile_for_binding_symbol(
        &mut self,
        name: &str,
        span: &PosExpr,
        context: &str,
    ) -> Result<Option<crate::bytecode::SymbolId>, CompileError> {
        if Self::is_discard_binding(name) {
            return Ok(None);
        }
        self.validate_mutable_binding_name(name, Self::span_from_expr(span), context)?;
        let symbol = self.chunk.symbols.intern(name);
        self.chunk.mark_local(symbol);
        Ok(Some(symbol))
    }

    /// Returns whether a loop binding discards its value.
    fn is_discard_binding(name: &str) -> bool {
        name.is_empty() || name == "_"
    }

    /// Compiles destructuring loops such as `for (name age) in iter { ... }`.
    ///
    /// The iterator is initialized once. Each iteration assigns bindings from array
    /// positions or matching object fields without introducing a temporary script
    /// variable, leaving the ordinary loop path unchanged.
    fn compile_for_destructure(
        &mut self,
        label: &str,
        names: &[String],
        iter: &PosExpr,
        body: &[Statement],
    ) -> Result<(), CompileError> {
        let iterable = self.compile_expr(iter)?;
        let iterator = self.alloc_register();
        self.emit_expr(
            Instruction::IterInit {
                dst: iterator,
                iterable,
            },
            iter,
        );
        let loop_start = self.chunk.code.len() as u32;
        let mut symbols = Vec::with_capacity(names.len());
        for name in names {
            self.validate_mutable_binding_name(
                name,
                Self::span_from_expr(iter),
                "for destructuring variable",
            )?;
            let symbol = self.chunk.symbols.intern(name);
            self.chunk.mark_local(symbol);
            symbols.push(symbol);
        }
        let iter_next = self.chunk.code.len();
        self.emit_expr(
            Instruction::IterNextDestructure {
                iterator,
                symbols,
                jump_to_end: 0,
            },
            iter,
        );
        self.push_loop(label, loop_start);
        self.compile_block(body)?;
        self.finish_loop(loop_start, None);
        self.emit_expr(Instruction::Jump { target: loop_start }, iter);
        let loop_end = self.chunk.code.len() as u32;
        if let Instruction::IterNextDestructure { jump_to_end, .. } =
            &mut self.chunk.code[iter_next]
        {
            *jump_to_end = loop_end;
        }
        self.patch_finished_loop(loop_end);
        Ok(())
    }

    /// Compile property chain.
    fn compile_property_chain(
        &mut self,
        items: &[PosExpr],
        root: &PosExpr,
    ) -> Result<Register, CompileError> {
        let Some((first, rest)) = items.split_first() else {
            return Err(self.unsupported_expr(root));
        };
        let mut object = self.compile_expr(first)?;
        for key_expr in rest {
            let key = self.compile_expr(key_expr)?;
            let dst = self.alloc_register();
            self.emit_expr(Instruction::GetProperty { dst, object, key }, key_expr);
            object = dst;
        }
        Ok(object)
    }

    /// Compiled function value.
    fn compile_function_value(
        &mut self,
        name: &str,
        params: &[(String, Option<PosExpr>)],
        body: &[Statement],
    ) -> Result<Register, CompileError> {
        let mut function_compiler =
            Compiler::with_source_file(self.chunk.source_file.clone(), self.base_dir.clone());
        function_compiler.include_stack = self.include_stack.clone();
        function_compiler.global_constants = self.global_constants.clone();
        function_compiler.is_function_scope = true;
        let params = params
            .iter()
            .map(|(param, default)| {
                function_compiler.validate_mutable_binding_name(
                    param,
                    default
                        .as_ref()
                        .map(Self::span_from_expr)
                        .unwrap_or_else(|| SourceSpan {
                            file: function_compiler.chunk.source_file.clone(),
                            line: 1,
                            column: 1,
                        }),
                    "function parameter",
                )?;
                let symbol = function_compiler.chunk.symbols.intern(param);
                function_compiler.chunk.mark_local(symbol);
                Ok(FunctionParam {
                    symbol,
                    default: default
                        .as_ref()
                        .map(Self::literal_default_value)
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;
        function_compiler.compile_function_body(body)?;
        function_compiler.chunk.emit(Instruction::Halt);
        function_compiler.chunk.register_count = function_compiler.next_register;
        let function = FunctionChunk {
            name: name.to_string(),
            params,
            chunk: Box::new(function_compiler.chunk),
        };
        let function_id = self.chunk.add_function(function);
        let dst = self.alloc_register();
        self.chunk.emit(Instruction::MakeFunction {
            dst,
            function: function_id,
        });
        Ok(dst)
    }

    /// Compiled function parameter default values.
    ///
    /// Defaults are stored statically when a function is created, covering literals,
    /// arrays, and objects. If defaults later need calls or variable reads, this can
    /// be extended to compile a default-value expression.
    fn literal_default_value(expr: &PosExpr) -> Result<Value, CompileError> {
        match &expr.expr {
            Expr::Int(value) => Ok(Value::Int(*value)),
            Expr::Float(value) => Ok(Value::Float(*value)),
            Expr::Str(value) | Expr::Strs(value) => Ok(Value::Str(value.clone())),
            Expr::Bool(value) => Ok(Value::Bool(*value)),
            Expr::Null => Ok(Value::Null),
            Expr::Empty => Ok(Value::Empty),
            Expr::Array(items) => items
                .iter()
                .map(Self::literal_default_value)
                .collect::<Result<Vec<_>, _>>()
                .map(|items| Value::Array(std::rc::Rc::new(std::cell::RefCell::new(items)))),
            Expr::Object(entries) => {
                let mut values = indexmap::IndexMap::new();
                for (key, value) in entries {
                    values.insert(key.clone(), Self::literal_default_value(value)?);
                }
                Ok(Value::Object(std::rc::Rc::new(std::cell::RefCell::new(
                    values,
                ))))
            }
            _ => Err(CompileError {
                file: expr.file.clone(),
                line: expr.line,
                column: expr.column,
                message: "Function parameter defaults currently support only literals, arrays, and objects".to_string(),
            }),
        }
    }

    /// Compiles a function body.
    ///
    /// The BT function returns the value of the last expression statement by default; if the last statement is not an expression, `empty` is returned.
    fn compile_function_body(&mut self, body: &[Statement]) -> Result<(), CompileError> {
        if body.is_empty() {
            let src = self.emit_constant(Value::Empty)?;
            self.chunk.emit(Instruction::Return { src });
            return Ok(());
        }
        if let Some(Statement::Return(expr)) = body.last() {
            let Some((_, prefix)) = body.split_last() else {
                unreachable!();
            };
            for statement in prefix {
                self.compile_statement(statement)?;
            }
            let src = self.compile_expr(expr)?;
            self.emit_expr(Instruction::Return { src }, expr);
        } else {
            let src = self.compile_block_value(body)?;
            let span =
                Self::span_from_statement(body.last().expect("Function body cannot be empty"))
                    .unwrap_or_else(|| SourceSpan {
                        file: "<unknown>".to_string(),
                        line: 0,
                        column: 0,
                    });
            self.chunk.emit_at(Instruction::Return { src }, span);
        }
        Ok(())
    }

    /// Compiles a class definition value.
    fn compile_class_value(
        &mut self,
        name: &str,
        members: &indexmap::IndexMap<String, (bool, Statement)>,
    ) -> Result<Register, CompileError> {
        let mut compiled = Vec::with_capacity(members.len());
        for (member_name, (is_private, statement)) in members {
            let value = match statement {
                Statement::Expr(expr) => self.compile_expr(expr)?,
                Statement::Fn(_, params, body) => {
                    self.compile_function_value(member_name, params, body)?
                }
                Statement::Empty => self.emit_constant(Value::Empty)?,
                other => return Err(self.unsupported_statement(other)),
            };
            let symbol = self.chunk.symbols.intern(member_name);
            compiled.push((symbol, value, !*is_private));
        }
        let dst = self.alloc_register();
        let name = self.chunk.symbols.intern(name);
        self.chunk.emit(Instruction::MakeClass {
            dst,
            name,
            members: compiled,
        });
        Ok(dst)
    }

    /// Compiles ordinary assignment expressions.
    ///
    /// BT language follows the rule of "all expressions have return values". The return value of the assignment expression is the value of the expression on the right.
    /// Therefore `this.title = title` evaluates to `title`, and chained assignment
    /// such as `a = b = c = 123` carries the same value through every layer.
    fn compile_assign_expr(
        &mut self,
        target: &PosExpr,
        value: &PosExpr,
    ) -> Result<Register, CompileError> {
        if matches!(target.expr, Expr::Destructure(_)) {
            let src = self.compile_expr(value)?;
            self.compile_destructure_assignment(target, src)?;
            return Ok(src);
        }

        let target_ref = self.compile_assignment_target(target, None)?;
        let src = self.compile_expr(value)?;
        self.emit_assignment_store(&target_ref, src, target, true)?;
        Ok(src)
    }

    /// Compile compound assignment expressions.
    fn compile_compound_assign(
        &mut self,
        expr: &Expr,
        left: &PosExpr,
        right: &PosExpr,
        root: &PosExpr,
    ) -> Result<Register, CompileError> {
        let target = self.compile_assignment_target(left, Some("Compound assignment"))?;
        let current = self.emit_assignment_load(&target, left)?;
        let value = self.compile_expr(right)?;
        let dst = self.alloc_register();
        let op = match expr {
            Expr::PlusAssign(_, _) => crate::lexer::TokenKind::Plus,
            Expr::MinusAssign(_, _) => crate::lexer::TokenKind::Minus,
            Expr::MultiplyAssign(_, _) => crate::lexer::TokenKind::Multiply,
            Expr::DivideAssign(_, _) => crate::lexer::TokenKind::Divide,
            Expr::ModuloAssign(_, _) => crate::lexer::TokenKind::Modulo,
            Expr::ShiftLeftAssign(_, _) => crate::lexer::TokenKind::ShiftLeft,
            Expr::ShiftRightAssign(_, _) => crate::lexer::TokenKind::ShiftRight,
            Expr::BitAndAssign(_, _) => crate::lexer::TokenKind::BitAnd,
            Expr::BitXorAssign(_, _) => crate::lexer::TokenKind::Xor,
            Expr::BitOrAssign(_, _) => crate::lexer::TokenKind::BitOr,
            _ => return Err(self.unsupported_expr(root)),
        };
        self.emit_expr(
            Instruction::Binary {
                op,
                dst,
                lhs: current,
                rhs: value,
            },
            root,
        );
        self.emit_assignment_store(&target, dst, left, false)?;
        Ok(dst)
    }

    /// Compiles prefix or postfix increment/decrement expressions.
    ///
    /// Prefix form returns the new value after writing it back; postfix form retains and returns the old value. Both resolve the target once before reading,
    /// calculating, and writing, so dynamic object and index expressions are not evaluated repeatedly.
    fn compile_increment(
        &mut self,
        inner: &PosExpr,
        delta: i8,
        prefix: bool,
        root: &PosExpr,
    ) -> Result<Register, CompileError> {
        let target = self.compile_assignment_target(inner, Some("Increment or decrement"))?;
        let current = self.emit_assignment_load(&target, inner)?;
        let updated = self.alloc_register();
        self.emit_expr(
            Instruction::Increment {
                dst: updated,
                src: current,
                delta,
            },
            root,
        );
        self.emit_assignment_store(&target, updated, inner, false)?;
        Ok(if prefix { updated } else { current })
    }

    /// Compile short circuit logic expression.
    ///
    /// `&&`, `||`, and `??` emit jumps so the right-hand expression is evaluated only
    /// when required. The destination initially holds the left operand and is
    /// overwritten only when needed, preserving results such as `empty && value`.
    fn compile_short_circuit_expr(
        &mut self,
        left: &PosExpr,
        op: &TokenKind,
        right: &PosExpr,
        root: &PosExpr,
    ) -> Result<Register, CompileError> {
        let lhs = self.compile_expr(left)?;
        let dst = self.alloc_register();
        self.emit_expr(Instruction::Move { dst, src: lhs }, root);

        match op {
            TokenKind::And => {
                let jump_to_end = self.emit_jump_if_false(lhs, left);
                let rhs = self.compile_expr(right)?;
                self.emit_expr(Instruction::Move { dst, src: rhs }, root);
                self.patch_jump(jump_to_end);
            }
            TokenKind::Or => {
                let jump_to_end = self.emit_jump_if_true(lhs, left);
                let rhs = self.compile_expr(right)?;
                self.emit_expr(Instruction::Move { dst, src: rhs }, root);
                self.patch_jump(jump_to_end);
            }
            TokenKind::Coalesce => {
                let jump_to_rhs = self.emit_jump_if_nullish(lhs, left);
                let jump_to_end = self.emit_jump();
                self.patch_jump(jump_to_rhs);
                let rhs = self.compile_expr(right)?;
                self.emit_expr(Instruction::Move { dst, src: rhs }, root);
                self.patch_jump(jump_to_end);
            }
            _ => return Err(self.unsupported_expr(root)),
        }

        Ok(dst)
    }

    /// Emits an unconditional jump for later patching.
    fn emit_jump(&mut self) -> usize {
        let index = self.chunk.code.len();
        self.chunk.emit(Instruction::Jump { target: 0 });
        index
    }

    /// Emits a conditional jump for later patching.
    fn emit_jump_if_false(&mut self, condition: Register, span: &PosExpr) -> usize {
        let index = self.chunk.code.len();
        self.emit_expr(
            Instruction::JumpIfFalse {
                condition,
                target: 0,
            },
            span,
        );
        index
    }

    /// Write the instruction to be backfilled that jumps when the condition is true.
    fn emit_jump_if_true(&mut self, condition: Register, span: &PosExpr) -> usize {
        let index = self.chunk.code.len();
        self.emit_expr(
            Instruction::JumpIfTrue {
                condition,
                target: 0,
            },
            span,
        );
        index
    }

    /// Emits a patchable jump taken when the condition is `null` or `empty`.
    fn emit_jump_if_nullish(&mut self, condition: Register, span: &PosExpr) -> usize {
        let index = self.chunk.code.len();
        self.emit_expr(
            Instruction::JumpIfNullish {
                condition,
                target: 0,
            },
            span,
        );
        index
    }

    /// Patches a jump target to the current instruction position.
    fn patch_jump(&mut self, index: usize) {
        let target = self.chunk.code.len() as u32;
        match &mut self.chunk.code[index] {
            Instruction::Jump { target: old }
            | Instruction::JumpIfFalse { target: old, .. }
            | Instruction::JumpIfTrue { target: old, .. }
            | Instruction::JumpIfNullish { target: old, .. } => {
                *old = target;
            }
            _ => {}
        }
    }

    /// Compile destructuring assignment target.
    ///
    /// Ordinary variables and properties use `compile_assignment_target`. Since
    /// destructuring targets have no object or index side effects, they retain a
    /// separate batched-symbol write path.
    fn compile_destructure_assignment(
        &mut self,
        target: &PosExpr,
        src: Register,
    ) -> Result<(), CompileError> {
        match &target.expr {
            Expr::Destructure(names) => {
                let mut symbols = Vec::with_capacity(names.len());
                let mut constants = Vec::with_capacity(names.len());
                for name in names {
                    let kind = self.binding_name_kind(name, Self::span_from_expr(target))?;
                    let symbol = self.chunk.symbols.intern(name);
                    if kind == BindingNameKind::Constant {
                        self.define_constant(name, Self::span_from_expr(target))?;
                        if self.is_function_scope {
                            self.chunk.mark_local(symbol);
                        }
                        constants.push(true);
                    } else {
                        constants.push(false);
                    }
                    symbols.push(symbol);
                }
                self.emit_expr(
                    Instruction::DestructureAssign {
                        src,
                        symbols,
                        constants,
                    },
                    target,
                );
                Ok(())
            }
            _ => Err(CompileError {
                file: target.file.clone(),
                line: target.line,
                column: target.column,
                message: "The left side of the destructuring assignment must be a list of variable names.".to_string(),
            }),
        }
    }

    /// Compiles an assignment target and pins references needed for later reads and writes.
    ///
    /// When `mutation` is `Some`, it means that the target already has a value and will be modified. At this time, the constant cannot be used as the target; normal `=`
    /// Passing in `None` allows uppercase names to complete the first constant definition according to existing rules.
    fn compile_assignment_target(
        &mut self,
        target: &PosExpr,
        mutation: Option<&str>,
    ) -> Result<AssignmentTarget, CompileError> {
        match &target.expr {
            Expr::Variable(name) => {
                let kind = self.binding_name_kind(name, Self::span_from_expr(target))?;
                if kind == BindingNameKind::Constant {
                    if let Some(operation) = mutation {
                        return Err(Self::compile_error(
                            &self.chunk.source_file,
                            Self::span_from_expr(target),
                            format!("Constant `{}` cannot use {}", name, operation),
                        ));
                    }
                }
                let symbol = self.chunk.symbols.intern(name);
                Ok(AssignmentTarget::Binding {
                    name: name.clone(),
                    symbol,
                })
            }
            Expr::ObjectProperty(items) if items.len() >= 2 => {
                let object_items = &items[..items.len() - 1];
                let key_expr = &items[items.len() - 1];
                let object = self.compile_property_chain(object_items, target)?;
                let key = self.compile_expr(key_expr)?;
                Ok(AssignmentTarget::Property { object, key })
            }
            _ => Err(Self::compile_error(
                &self.chunk.source_file,
                Self::span_from_expr(target),
                "The assignment target must be a writable variable, object field, array subscript, or class instance field".to_string(),
            )),
        }
    }

    /// Reads the current value from the resolved assignment target.
    fn emit_assignment_load(
        &mut self,
        target: &AssignmentTarget,
        span: &PosExpr,
    ) -> Result<Register, CompileError> {
        let dst = self.alloc_register();
        let instruction = match target {
            AssignmentTarget::Binding { symbol, .. } => Instruction::LoadGlobal {
                dst,
                symbol: *symbol,
            },
            AssignmentTarget::Property { object, key } => Instruction::GetProperty {
                dst,
                object: *object,
                key: *key,
            },
        };
        self.emit_expr(instruction, span);
        Ok(dst)
    }

    /// Writes the new value back to the resolved assignment target.
    ///
    /// `define_binding` is true only for ordinary `=`, preserving first-definition
    /// rules for uppercase constants. Compound assignment and increment/decrement
    /// reject constants while parsing the target, then emit ordinary writeback.
    fn emit_assignment_store(
        &mut self,
        target: &AssignmentTarget,
        src: Register,
        span: &PosExpr,
        define_binding: bool,
    ) -> Result<(), CompileError> {
        let instruction = match target {
            AssignmentTarget::Binding { name, .. } if define_binding => {
                self.compile_binding_store(name, src, Self::span_from_expr(span), false)?
            }
            AssignmentTarget::Binding { symbol, .. } => Instruction::StoreGlobal {
                symbol: *symbol,
                src,
            },
            AssignmentTarget::Property { object, key } => Instruction::SetProperty {
                object: *object,
                key: *key,
                value: src,
            },
        };
        self.emit_expr(instruction, span);
        Ok(())
    }

    /// Enters a new loop context.
    fn push_loop(&mut self, label: &str, continue_target: u32) {
        self.loop_stack.push(LoopContext {
            label: label.to_string(),
            continue_target,
            breaks: Vec::new(),
            continues: Vec::new(),
        });
    }

    /// Finishes the current loop body while retaining context for final jump patching.
    fn finish_loop(&mut self, _continue_target: u32, _break_target: Option<u32>) {}

    /// Compiles `break` or `continue` as a patchable jump.
    fn compile_loop_jump(&mut self, label: &str, is_break: bool) -> Result<(), CompileError> {
        let Some(index) = self.find_loop_context(label) else {
            return Err(CompileError {
                file: "<unknown>".to_string(),
                line: 0,
                column: 0,
                message: if is_break {
                    "break can only appear inside a loop".to_string()
                } else {
                    "continue can only appear inside a loop".to_string()
                },
            });
        };
        for _ in 0..self.try_depth {
            self.chunk.emit(Instruction::LeaveTry);
        }
        let jump = self.emit_jump();
        if is_break {
            self.loop_stack[index].breaks.push(jump);
        } else {
            self.loop_stack[index].continues.push(jump);
        }
        Ok(())
    }

    /// Finds the loop context matching a label.
    fn find_loop_context(&self, label: &str) -> Option<usize> {
        self.loop_stack
            .iter()
            .enumerate()
            .rev()
            .find(|(_, ctx)| label.is_empty() || ctx.label == label)
            .map(|(index, _)| index)
    }

    /// Backfills the innermost loop that just ended.
    fn patch_finished_loop(&mut self, break_target: u32) {
        let Some(context) = self.loop_stack.pop() else {
            return;
        };
        for jump in context.breaks {
            self.patch_jump_to(jump, break_target);
        }
        for jump in context.continues {
            self.patch_jump_to(jump, context.continue_target);
        }
    }

    /// Backfills a certain jump instruction to the specified target.
    fn patch_jump_to(&mut self, index: usize, target: u32) {
        match self.chunk.code.get_mut(index) {
            Some(
                Instruction::Jump { target: old }
                | Instruction::JumpIfFalse { target: old, .. }
                | Instruction::JumpIfTrue { target: old, .. }
                | Instruction::JumpIfNullish { target: old, .. },
            ) => *old = target,
            _ => {}
        }
    }

    /// Patches the catch target of a try instruction.
    fn patch_try_catch(&mut self, index: usize) {
        let target = self.chunk.code.len() as u32;
        if let Some(Instruction::EnterTry {
            catch_target: old, ..
        }) = self.chunk.code.get_mut(index)
        {
            *old = target;
        }
    }

    /// Split the regular literal text produced by the lexer.
    fn split_regex_literal(pattern: &str, fallback_flags: &str) -> (String, String) {
        if pattern.starts_with('/') {
            if let Some(end) = pattern.rfind('/') {
                if end > 0 {
                    return (pattern[1..end].to_string(), pattern[end + 1..].to_string());
                }
            }
        }
        (pattern.to_string(), fallback_flags.to_string())
    }

    /// Constructs an error that the statement is not currently supported.
    fn unsupported_statement(&self, statement: &Statement) -> CompileError {
        let (file, line, column) = match statement {
            Statement::Expr(expr)
            | Statement::Print(expr)
            | Statement::Println(expr)
            | Statement::Assign(expr, _)
            | Statement::Let(_, expr)
            | Statement::Return(expr)
            | Statement::Throw(expr)
            | Statement::If(expr, _, _)
            | Statement::Try(expr, _, _, _)
            | Statement::For(_, _, _, expr, _, _)
            | Statement::ForCount(_, expr, _, _)
            | Statement::ForDestructure(_, _, expr, _)
            | Statement::While(_, expr, _) => (expr.file.clone(), expr.line, expr.column),
            Statement::ForRange(_, _, _, start, end, step, _) => start
                .as_ref()
                .or(end.as_ref())
                .or(step.as_ref())
                .map(|expr| (expr.file.clone(), expr.line, expr.column))
                .unwrap_or_else(|| ("<unknown>".to_string(), 0, 0)),
            _ => ("<unknown>".to_string(), 0, 0),
        };
        CompileError {
            file,
            line,
            column,
            message: format!(
                "The current bytecode compiler does not support the `{}` statement",
                Self::statement_name(statement)
            ),
        }
    }

    /// Builds an error for an unsupported expression form.
    fn unsupported_expr(&self, expr: &PosExpr) -> CompileError {
        CompileError {
            file: expr.file.clone(),
            line: expr.line,
            column: expr.column,
            message: format!(
                "The current bytecode compiler does not support the `{}` expression",
                Self::expr_name(&expr.expr)
            ),
        }
    }

    /// Returns a stable, concise statement name for error reporting.
    fn statement_name(statement: &Statement) -> &'static str {
        match statement {
            Statement::Empty => "empty",
            Statement::Expr(_) => "expr",
            Statement::Let(_, _) => "let",
            Statement::Use(_, _) => "use",
            Statement::Declare(_, _) => "declare",
            Statement::Assign(_, _) => "assign",
            Statement::Print(_) => "print",
            Statement::Println(_) => "println",
            Statement::If(_, _, _) => "if",
            Statement::Fn(_, _, _) => "fn",
            Statement::Class(_, _) => "class",
            Statement::For(_, _, _, _, _, _) => "for",
            Statement::ForCount(_, _, _, _) => "for_count",
            Statement::ForRange(_, _, _, _, _, _, _) => "for_range",
            Statement::ForDestructure(_, _, _, _) => "for_destructure",
            Statement::While(_, _, _) => "while",
            Statement::Loop(_, _) => "loop",
            Statement::Return(_) => "return",
            Statement::Try(_, _, _, _) => "try",
            Statement::Throw(_) => "throw",
            Statement::Break(_) => "break",
            Statement::Continue(_) => "continue",
        }
    }

    /// Returns the expression type name, used for stable and concise error reporting.
    fn expr_name(expr: &Expr) -> &'static str {
        match expr {
            Expr::Int(_) => "int",
            Expr::Float(_) => "float",
            Expr::Str(_) => "string",
            Expr::Strs(_) => "template_string",
            Expr::Variable(_) => "variable",
            Expr::Bool(_) => "bool",
            Expr::Null => "null",
            Expr::Empty => "empty",
            Expr::Vec(_) => "expression_list",
            Expr::Destructure(_) => "destructure",
            Expr::Binary(_, _, _) => "binary",
            Expr::Not(_) => "not",
            Expr::BitNot(_) => "bit_not",
            Expr::Assign(_, _) => "assign",
            Expr::Fn(_, _, _) => "fn",
            Expr::Call(_, _) => "call",
            Expr::New(_, _, _) => "new",
            Expr::Regex(_, _) => "regex",
            Expr::Array(_) => "array",
            Expr::Object(_) => "object",
            Expr::ObjectProperty(_) => "property",
            Expr::Statement(_) => "statement",
            Expr::PostfixIncrement(_) => "postfix_increment",
            Expr::PostfixDecrement(_) => "postfix_decrement",
            Expr::PrefixIncrement(_) => "prefix_increment",
            Expr::PrefixDecrement(_) => "prefix_decrement",
            Expr::PlusAssign(_, _) => "plus_assign",
            Expr::MinusAssign(_, _) => "minus_assign",
            Expr::MultiplyAssign(_, _) => "multiply_assign",
            Expr::DivideAssign(_, _) => "divide_assign",
            Expr::ModuloAssign(_, _) => "modulo_assign",
            Expr::ShiftLeftAssign(_, _) => "shift_left_assign",
            Expr::ShiftRightAssign(_, _) => "shift_right_assign",
            Expr::BitAndAssign(_, _) => "bit_and_assign",
            Expr::BitXorAssign(_, _) => "bit_xor_assign",
            Expr::BitOrAssign(_, _) => "bit_or_assign",
            Expr::Match(_, _) => "match",
        }
    }
}
