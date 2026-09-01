//! BT language parser.
//!
//! The lexer emits lightweight `TokenKind + span` records; the parser then reads literal text from those spans, builds the AST,
//! and produces readable syntax errors for the bytecode compiler.
//! Grammar handling stays deliberately centralized around statement dispatch, expression precedence, and chained calls.

use crate::lexer::{Token, TokenKind};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Token with its source location.
///
/// The parser uses this structure for operators and error locations. Tokens retain
/// only integer spans; text is borrowed from `Parser::source` when needed, avoiding
/// a string allocation for every token.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PosToken {
    /// Token type.
    pub kind: TokenKind,
    /// The file name to which the token belongs.
    pub file: String,
    /// Token start byte offset.
    pub start: usize,
    /// Token end byte offset.
    pub end: usize,
    /// One-based starting line.
    pub line: usize,
    /// One-based starting column.
    pub column: usize,
}

/// Expression node with source code location.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PosExpr {
    /// Actual expression.
    pub expr: Expr,
    /// The file name to which the expression belongs.
    pub file: String,
    /// Expression starting line number.
    pub line: usize,
    /// Expression starting column number.
    pub column: usize,
}

/// Expression AST.
///
/// The name is retained from the original interpreter to keep the `Value`, eval,
/// and bytecode layers aligned during the transition.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum Expr {
    /// Integer literal.
    Int(i64),
    /// Floating point numeric literal.
    Float(f64),
    /// Ordinary string literal.
    Str(String),
    /// Template string literal.
    Strs(String),
    /// Variable reference.
    Variable(String),
    /// Boolean literal.
    Bool(bool),
    /// Null literal.
    Null,
    /// Empty expression.
    Empty,
    /// A list of expressions in parentheses, the value of the last expression is returned at runtime.
    Vec(Vec<PosExpr>),
    /// Destructured variable bindings on the left side of an assignment.
    Destructure(Vec<String>),
    /// Binary arithmetic expression.
    Binary(Box<PosExpr>, PosToken, Box<PosExpr>),
    /// Logical not expression, convert any value into Boolean and negate it according to BT truth value rules.
    Not(Box<PosExpr>),
    /// Bitwise negation expression, first convert the operand according to integer rules and then execute `~`.
    BitNot(Box<PosExpr>),
    /// Ordinary assignment expression; evaluates to its right-hand value.
    Assign(Box<PosExpr>, Box<PosExpr>),
    /// Function expression.
    Fn(String, Vec<(String, Option<PosExpr>)>, Vec<Statement>),
    /// Function or method call.
    Call(Box<PosExpr>, Vec<PosExpr>),
    /// Class instantiation expression, such as `Article::new()`.
    New(String, Box<PosExpr>, Vec<PosExpr>),
    /// Regular expression literal.
    Regex(String, String),
    /// Array literal.
    Array(Vec<PosExpr>),
    /// Object literal.
    Object(Vec<(String, PosExpr)>),
    /// Attribute access chain, the first item is the object, and the subsequent items are the attribute name or subscript expression.
    ObjectProperty(Vec<PosExpr>),
    /// Wraps statements as an expression, as used by conditional expressions and arrow bodies.
    Statement(Vec<Statement>),
    /// Post-increment.
    PostfixIncrement(Box<PosExpr>),
    /// Post-decrement.
    PostfixDecrement(Box<PosExpr>),
    /// Prefix auto-increment.
    PrefixIncrement(Box<PosExpr>),
    /// Pre-decrement.
    PrefixDecrement(Box<PosExpr>),
    /// Add-and-assign expression.
    PlusAssign(Box<PosExpr>, Box<PosExpr>),
    /// Subtract and then assign.
    MinusAssign(Box<PosExpr>, Box<PosExpr>),
    /// Multiply and assign.
    MultiplyAssign(Box<PosExpr>, Box<PosExpr>),
    /// Divide-and-assign expression.
    DivideAssign(Box<PosExpr>, Box<PosExpr>),
    /// Modulo-and-assign expression.
    ModuloAssign(Box<PosExpr>, Box<PosExpr>),
    /// Assign value after shifting left.
    ShiftLeftAssign(Box<PosExpr>, Box<PosExpr>),
    /// Shift right and then assign value.
    ShiftRightAssign(Box<PosExpr>, Box<PosExpr>),
    /// Bitwise AND followed by assignment.
    BitAndAssign(Box<PosExpr>, Box<PosExpr>),
    /// Bitwise XOR and then assignment.
    BitXorAssign(Box<PosExpr>, Box<PosExpr>),
    /// Bitwise OR followed by assignment.
    BitOrAssign(Box<PosExpr>, Box<PosExpr>),
    /// Match expression with its target and ordered branches.
    Match(Box<PosExpr>, Vec<MatchArm>),
}

/// Branch of a match expression.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct MatchArm {
    /// Branch pattern; `None` represents the `_` default branch.
    pub pattern: Option<PosExpr>,
    /// The expression returned after a branch hit.
    pub value: PosExpr,
}

/// Statement AST.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum Statement {
    /// Empty statement.
    Empty,
    /// Expression statement.
    Expr(PosExpr),
    /// Local variable declaration.
    Let(String, PosExpr),
    /// Field-import statement.
    Use(PosExpr, Option<Vec<String>>),
    /// Null value variable declaration supports compact writing such as `name` or `name age six`.
    Declare(Vec<String>, PosExpr),
    /// Ordinary assignment statement.
    Assign(PosExpr, PosExpr),
    /// Prints without a trailing newline.
    Print(PosExpr),
    /// Newline output.
    Println(PosExpr),
    /// Conditional statement.
    If(PosExpr, Vec<Statement>, Option<Vec<Statement>>),
    /// Try/catch statement: source location, try body, error binding, and catch body.
    Try(PosExpr, Vec<Statement>, String, Vec<Statement>),
    /// Throw statement.
    Throw(PosExpr),
    /// Function declaration.
    Fn(String, Vec<(String, Option<PosExpr>)>, Vec<Statement>),
    /// Class declaration, bool indicates whether it is private.
    Class(String, IndexMap<String, (bool, Statement)>),
    /// Collection loop: label, key binding, value binding, iterable, optional step, and body.
    For(
        String,
        String,
        String,
        PosExpr,
        Option<PosExpr>,
        Vec<Statement>,
    ),
    /// Counted loop: label, iteration count, optional step, and body.
    ForCount(String, PosExpr, Option<PosExpr>, Vec<Statement>),
    /// Range loop: label, key binding, value binding, start, end, step, and body.
    ForRange(
        String,
        String,
        String,
        Option<PosExpr>,
        Option<PosExpr>,
        Option<PosExpr>,
        Vec<Statement>,
    ),
    /// Destructuring loop: label, bindings, iterable, and body.
    ForDestructure(String, Vec<String>, PosExpr, Vec<Statement>),
    /// While loop: label, condition, and body.
    While(String, PosExpr, Vec<Statement>),
    /// Infinite loop: label and body.
    Loop(String, Vec<Statement>),
    /// Return statement.
    Return(PosExpr),
    /// Break statement.
    Break(String),
    /// Continue statement.
    Continue(String),
}

/// Syntax error.
///
/// Stores the file, line, column, and source excerpt needed for diagnostics similar to those produced by Rust, JavaScript, and PHP tooling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParseError {
    /// The file where the error is located.
    pub file: String,
    /// The line number where the error occurred.
    pub line: usize,
    /// The column number where the error is located.
    pub column: usize,
    /// Error message.
    pub message: String,
    /// Current line source code.
    pub source_line: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let caret_padding = " ".repeat(self.column.saturating_sub(1));
        write!(
            f,
            "{}:{}:{}: Syntax error: {}\n{}\n{}^",
            self.file, self.line, self.column, self.message, self.source_line, caret_padding
        )
    }
}

impl std::error::Error for ParseError {}

/// BT syntax analyzer.
pub struct Parser<'a> {
    /// Source code file name, used for error reporting.
    file: String,
    /// Complete source code, parser extracts literals from here based on token span.
    source: &'a str,
    /// A list of tokens with positions.
    tokens: Vec<PosToken>,
    /// Current token index.
    pos: usize,
    /// The current code block nesting depth.
    ///
    /// A single naked identifier can be declared as an empty variable at the top level; after entering a function, if, loop, etc. code block, a single identifier
    /// is more commonly "the last statement returns this variable", so only the multi-identifier declaration continues to take effect within the block. The executable source behind
    block_depth: usize,
}

/// `for`.
///
/// The parsing phase only separates grammatical forms; the VM decides how sets, integer counts, and other runtime values are iterated.
enum ForSource {
    /// General loop source with an optional step. At runtime this becomes collection
    /// traversal or a counted loop, depending on the value.
    Value(PosExpr, Option<PosExpr>),
    /// Interval source, holds optional start, end, and step expressions.
    Range(Option<PosExpr>, Option<PosExpr>, Option<PosExpr>),
}

impl<'a> Parser<'a> {
    /// Create parser.
    ///
    /// The lexer emits lightweight tokens. Here they are combined with source slices, file names, and locations into the parser-friendly `PosToken` form.
    /// This conversion happens once; subsequent parsing advances linearly by token index.
    pub fn new(file: impl Into<String>, source: &'a str, tokens: Vec<Token>) -> Self {
        let file = file.into();
        let tokens = tokens
            .into_iter()
            .map(|token| PosToken {
                kind: token.kind,
                file: file.clone(),
                start: token.start,
                end: token.end,
                line: token.line,
                column: token.column,
            })
            .collect();
        Self {
            file,
            source,
            tokens,
            pos: 0,
            block_depth: 0,
        }
    }

    /// Parses the complete source code into a statement list.
    pub fn parse(&mut self) -> Result<Vec<Statement>, ParseError> {
        let mut statements = Vec::new();
        self.skip_statement_separators();
        while !self.is_eof() {
            let statement = self.parse_statement()?;
            if statement != Statement::Empty {
                statements.push(statement);
            }
            self.skip_statement_separators();
        }
        Ok(statements)
    }

    /// Parses a statement.
    ///
    /// When adding top-level syntax, priority is given to expanding here: keyword statements are directly dispatched in `match`, and
    /// ordinary statements fall into expression parsing, and then judge whether they are assignments based on subsequent tokens.
    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        self.skip_statement_separators();
        let Some(token) = self.current().cloned() else {
            return Ok(Statement::Empty);
        };

        match token.kind {
            TokenKind::Use => self.parse_use_statement(),
            TokenKind::Let => self.parse_let_statement(),
            TokenKind::Print => self.parse_print_statement(false),
            TokenKind::Println => self.parse_print_statement(true),
            TokenKind::If => self.parse_if_statement(),
            TokenKind::Try => self.parse_try_statement(),
            TokenKind::Throw => self.parse_throw_statement(),
            TokenKind::Fn => self.parse_fn_statement(),
            TokenKind::Class => self.parse_class_statement(),
            TokenKind::For => self.parse_for_statement(),
            TokenKind::While => self.parse_while_statement(),
            TokenKind::Loop => self.parse_loop_statement(),
            TokenKind::Return => self.parse_return_statement(),
            TokenKind::Break => self.parse_jump_statement(true),
            TokenKind::Continue => self.parse_jump_statement(false),
            TokenKind::RightBrace => {
                Err(self.error_here("Unexpected `}`: the current code block has already ended"))
            }
            _ => {
                if let Some(statement) = self.parse_bare_system_call_statement()? {
                    return Ok(statement);
                }
                if self.check(TokenKind::Identifier)
                    && self
                        .current()
                        .is_none_or(|token| self.token_text(token) != "this")
                    && (self.peek_kind(1) == Some(TokenKind::Identifier)
                        || self.peek_kind(1).is_some_and(|kind| {
                            self.block_depth == 0
                                && matches!(
                                    kind,
                                    TokenKind::Wrap | TokenKind::Semicolon | TokenKind::RightBrace
                                )
                        })
                        || self.peek_kind(1).is_none())
                {
                    return self.parse_empty_declare_statement();
                }
                let target = self.parse_expression(0)?;
                if self.match_kind(TokenKind::Assign) {
                    let value = self.parse_expression(0)?;
                    Ok(Statement::Assign(target, value))
                } else {
                    Ok(Statement::Expr(target))
                }
            }
        }
    }

    /// Parse null variable declaration.
    ///
    /// BT allows a row of consecutive identifiers to be treated as multiple null value variable declarations. For example, `name age six` will create three
    /// `Empty` variables; this rule is only triggered at the statement entry, and expressions such as `obj.name` and `fn()` still maintain the original semantics.
    fn parse_empty_declare_statement(&mut self) -> Result<Statement, ParseError> {
        let first = self.expect_identifier_like("Variable declaration requires variable name")?;
        let span = self.empty_expr(&first);
        let mut names = vec![self.token_text(&first).to_string()];
        while self.check(TokenKind::Identifier) {
            let name =
                self.expect_identifier_like("Variable declaration requires variable name")?;
            names.push(self.token_text(&name).to_string());
        }
        Ok(Statement::Declare(names, span))
    }

    /// Parse directed open parenthesesless system function call.
    ///
    /// This syntax only covers `echo value`, `include path` and `include_once path`, and does not spread to all system functions;
    /// bracket forms are still processed by ordinary call expressions, and end-of-line naked identifiers continue to be handed over to the null declaration rules.
    fn parse_bare_system_call_statement(&mut self) -> Result<Option<Statement>, ParseError> {
        let Some(token) = self.current().cloned() else {
            return Ok(None);
        };
        if token.kind != TokenKind::Identifier {
            return Ok(None);
        }
        let name = self.token_text(&token);
        if !matches!(name, "echo" | "include" | "include_once")
            || self.peek_kind(1) == Some(TokenKind::LeftParen)
            || self.peek_kind(1).is_none()
            || self.peek_kind(1).is_some_and(|kind| {
                matches!(
                    kind,
                    TokenKind::Wrap
                        | TokenKind::Semicolon
                        | TokenKind::Comma
                        | TokenKind::RightBrace
                )
            })
        {
            return Ok(None);
        }

        let name = name.to_string();
        self.advance();
        let callee = self.pos_expr(&token, Expr::Variable(name));
        let arg = self.parse_expression(0)?;
        Ok(Some(Statement::Expr(self.pos_expr(
            &token,
            Expr::Call(Box::new(callee), vec![arg]),
        ))))
    }

    /// Parse expressions.
    ///
    /// Pratt parser is used here: all binary operator precedence is concentrated in `binary_binding_power`,
    /// right associative or left associative rules are also maintained in a table. Ternary conditionals, arrow functions, and compound assignments are handled in the same loop as
    /// expression-level syntax for easy later expansion.
    fn parse_expression(&mut self, min_bp: u8) -> Result<PosExpr, ParseError> {
        self.skip_soft_separators();
        let mut left = self.parse_prefix()?;

        loop {
            self.skip_soft_separators();
            let Some(op) = self.current().cloned() else {
                break;
            };

            if self.is_expression_stop(&op.kind) {
                break;
            }

            if op.kind == TokenKind::Question {
                if min_bp > 1 {
                    break;
                }
                self.advance();
                let then_expr = self.parse_expression(0)?;
                let else_expr = if self.match_kind(TokenKind::Colon) {
                    Some(self.parse_expression(0)?)
                } else {
                    None
                };
                let else_body = else_expr.map(|expr| vec![Statement::Expr(expr)]);
                left = self.pos_expr(
                    &op,
                    Expr::Statement(vec![Statement::If(
                        left,
                        vec![Statement::Expr(then_expr)],
                        else_body,
                    )]),
                );
                continue;
            }

            if op.kind == TokenKind::Arrow {
                if min_bp > 1 {
                    break;
                }
                self.advance();
                let params = self.expr_to_arrow_params(left.clone(), &op)?;
                left = self.parse_arrow_body(params, &op)?;
                continue;
            }

            if op.kind == TokenKind::Assign {
                if min_bp > 1 {
                    break;
                }
                self.advance();
                let right = self.parse_expression(1)?;
                left = self.pos_expr(&op, Expr::Assign(Box::new(left), Box::new(right)));
                continue;
            }

            if min_bp <= 1 {
                if let Some(assign_expr) = self.parse_compound_assignment(&left, &op)? {
                    left = assign_expr;
                    continue;
                }
            }

            let Some((left_bp, right_bp)) = Self::binary_binding_power(&op.kind) else {
                break;
            };
            if left_bp < min_bp {
                break;
            }
            self.advance();
            let right = self.parse_expression(right_bp)?;
            left = self.pos_expr(
                &op,
                Expr::Binary(Box::new(left), op.clone(), Box::new(right)),
            );
        }

        Ok(left)
    }

    /// Parse prefix expressions.
    fn parse_prefix(&mut self) -> Result<PosExpr, ParseError> {
        self.skip_soft_separators();
        let token = self
            .advance()
            .ok_or_else(|| self.error_at_end("Expression ended unexpectedly"))?;

        match token.kind {
            TokenKind::Int => {
                let expr = self.parse_int_literal(token)?;
                self.parse_postfix(expr)
            }
            TokenKind::Float => {
                let expr = self.parse_float_literal(token)?;
                self.parse_postfix(expr)
            }
            TokenKind::Str => {
                let expr = self.pos_expr(&token, Expr::Str(Self::unquote(self.token_text(&token))));
                self.parse_postfix(expr)
            }
            TokenKind::Strs => {
                let expr = self.pos_expr(
                    &token,
                    Expr::Strs(Self::unquote_template(self.token_text(&token))),
                );
                self.parse_postfix(expr)
            }
            TokenKind::Regex => {
                let expr = self.pos_expr(
                    &token,
                    Expr::Regex(self.token_text(&token).to_string(), String::new()),
                );
                self.parse_postfix(expr)
            }
            TokenKind::Identifier => {
                let text = self.token_text(&token);
                let expr = match text {
                    "true" => self.pos_expr(&token, Expr::Bool(true)),
                    "false" => self.pos_expr(&token, Expr::Bool(false)),
                    "null" => self.pos_expr(&token, Expr::Null),
                    "empty" => self.pos_expr(&token, Expr::Empty),
                    _ => self.pos_expr(&token, Expr::Variable(text.to_string())),
                };
                self.parse_postfix(expr)
            }
            TokenKind::Minus => {
                let right = self.parse_expression(13)?;
                let zero = self.pos_expr(&token, Expr::Int(0));
                Ok(self.pos_expr(
                    &token,
                    Expr::Binary(Box::new(zero), token.clone(), Box::new(right)),
                ))
            }
            TokenKind::Not => {
                let right = self.parse_expression(13)?;
                Ok(self.pos_expr(&token, Expr::Not(Box::new(right))))
            }
            TokenKind::BitNot => {
                let right = self.parse_expression(13)?;
                Ok(self.pos_expr(&token, Expr::BitNot(Box::new(right))))
            }
            TokenKind::Increment => {
                let expr = self.parse_prefix()?;
                Ok(self.pos_expr(&token, Expr::PrefixIncrement(Box::new(expr))))
            }
            TokenKind::Decrement => {
                let expr = self.parse_prefix()?;
                Ok(self.pos_expr(&token, Expr::PrefixDecrement(Box::new(expr))))
            }
            TokenKind::Fn => self.parse_fn_expression(token, None),
            TokenKind::Print
            | TokenKind::Println
            | TokenKind::Return
            | TokenKind::Break
            | TokenKind::Continue => {
                self.rewind_one();
                let statement = self.parse_statement()?;
                Ok(self.pos_expr(&token, Expr::Statement(vec![statement])))
            }
            TokenKind::If => {
                self.rewind_one();
                let statement = self.parse_if_statement()?;
                Ok(self.pos_expr(&token, Expr::Statement(vec![statement])))
            }
            TokenKind::Try => {
                self.rewind_one();
                let statement = self.parse_try_statement()?;
                Ok(self.pos_expr(&token, Expr::Statement(vec![statement])))
            }
            TokenKind::Match => self.parse_match_expression(token),
            TokenKind::For | TokenKind::While | TokenKind::Loop => {
                self.rewind_one();
                let statement = self.parse_statement()?;
                Ok(self.pos_expr(&token, Expr::Statement(vec![statement])))
            }
            TokenKind::LeftParen => self.parse_group_or_arrow(token),
            TokenKind::LeftBracket => self.parse_array_literal(token),
            TokenKind::LeftBrace => self.parse_object_literal(token),
            _ => Err(self.error_at(
                &token,
                format!(
                    "Expected an expression, found `{}`",
                    self.token_text(&token)
                ),
            )),
        }
    }

    /// Parses calls, property and index access, class construction, and postfix increment/decrement.
    fn parse_postfix(&mut self, mut expr: PosExpr) -> Result<PosExpr, ParseError> {
        loop {
            self.skip_wraps_before_dot();
            let Some(token) = self.current().cloned() else {
                break;
            };
            match token.kind {
                TokenKind::LeftParen => {
                    self.advance();
                    let args = self.parse_expression_list(TokenKind::RightParen)?;
                    expr = self.pos_expr(&token, Expr::Call(Box::new(expr), args));
                }
                TokenKind::Dot => {
                    self.advance();
                    let property =
                        self.expect_property_name("`.` must be followed by a property name")?;
                    let property_expr =
                        self.pos_expr(&property, Expr::Str(self.token_text(&property).to_string()));
                    expr = self.chain_property(expr, property_expr);
                }
                TokenKind::LeftBracket => {
                    if !self.is_tight_to_previous(&token) {
                        break;
                    }
                    self.advance();
                    let index = self.parse_expression(0)?;
                    self.expect(
                        TokenKind::RightBracket,
                        "`[` is missing the corresponding `]`",
                    )?;
                    expr = self.chain_property(expr, index);
                }
                TokenKind::Colon if self.peek_kind(1) == Some(TokenKind::Colon) => {
                    self.advance();
                    self.advance();
                    let ctor = self
                        .expect_identifier_or_new("`::` must be followed by a constructor name")?;
                    self.expect(
                        TokenKind::LeftParen,
                        "A constructor name must be followed by `(`",
                    )?;
                    let args = self.parse_expression_list(TokenKind::RightParen)?;
                    expr = self.pos_expr(
                        &ctor,
                        Expr::New(self.token_text(&ctor).to_string(), Box::new(expr), args),
                    );
                }
                TokenKind::Increment => {
                    self.advance();
                    expr = self.pos_expr(&token, Expr::PostfixIncrement(Box::new(expr)));
                }
                TokenKind::Decrement => {
                    self.advance();
                    expr = self.pos_expr(&token, Expr::PostfixDecrement(Box::new(expr)));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// Parsing `use` Import statement.
    fn parse_use_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::Use, "Internal error: expected `use`")?;
        let path = self.parse_expression(0)?;
        let imports = if self.match_kind(TokenKind::LeftBrace) {
            let mut imports = Vec::new();
            while !self.check(TokenKind::RightBrace) {
                let name = self.expect_identifier_like("use import list requires identifier")?;
                imports.push(self.token_text(&name).to_string());
                if !self.match_kind(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(
                TokenKind::RightBrace,
                "use import list missing terminating `}`",
            )?;
            Some(imports)
        } else {
            None
        };
        Ok(Statement::Use(path, imports))
    }

    /// Parses a `let name = value` local declaration.
    fn parse_let_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::Let, "Internal error: let")?;
        let name = self.expect_identifier_like("Variable declaration requires a name")?;
        let value = if self.match_kind(TokenKind::Assign) {
            self.parse_expression(0)?
        } else {
            self.empty_expr(&name)
        };
        Ok(Statement::Let(self.token_text(&name).to_string(), value))
    }

    /// Parses `print` or `println` after its keyword has been consumed.
    fn parse_print_statement(&mut self, newline: bool) -> Result<Statement, ParseError> {
        if newline {
            self.expect(TokenKind::Println, "Internal error: println required")?;
        } else {
            self.expect(TokenKind::Print, "Internal error: print")?;
        }
        let expr = self.parse_expression(0)?;
        if newline {
            Ok(Statement::Println(expr))
        } else {
            Ok(Statement::Print(expr))
        }
    }

    /// Parses an `if / elseif / else` statement.
    fn parse_if_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::If, "Internal error: if")?;
        let condition = self.parse_expression(0)?;
        let true_branch = self.parse_required_block("The if condition must be followed by `{`")?;
        self.skip_statement_separators();
        let else_branch = if self.match_kind(TokenKind::ElseIf) {
            let elseif_token = self.previous().clone();
            self.rewind_one();
            Some(vec![self.parse_elseif_as_if(&elseif_token)?])
        } else if self.match_kind(TokenKind::Else) {
            if self.check(TokenKind::If) {
                Some(vec![self.parse_if_statement()?])
            } else {
                Some(self.parse_required_block("The else branch must begin with `{`")?)
            }
        } else {
            None
        };
        Ok(Statement::If(condition, true_branch, else_branch))
    }

    /// Parses `elseif` as a nested `if`, keeping execution limited to one conditional form.
    fn parse_elseif_as_if(&mut self, _token: &PosToken) -> Result<Statement, ParseError> {
        self.expect(TokenKind::ElseIf, "Internal error: elseif required")?;
        let condition = self.parse_expression(0)?;
        let true_branch = self.parse_required_block("The elseif condition requires `{`")?;
        self.skip_statement_separators();
        let else_branch = if self.match_kind(TokenKind::ElseIf) {
            let elseif_token = self.previous().clone();
            self.rewind_one();
            Some(vec![self.parse_elseif_as_if(&elseif_token)?])
        } else if self.match_kind(TokenKind::Else) {
            Some(self.parse_required_block("The else branch must begin with `{`")?)
        } else {
            None
        };
        Ok(Statement::If(condition, true_branch, else_branch))
    }

    /// Parses a `try { ... } catch e { ... }` statement.
    fn parse_try_statement(&mut self) -> Result<Statement, ParseError> {
        let token = self.expect(TokenKind::Try, "Internal error: try")?;
        let try_body = self.parse_required_block("`try` must be followed by `{`")?;
        self.skip_statement_separators();
        self.expect(TokenKind::Catch, "A try block must be followed by `catch`")?;
        let error = self.expect_identifier_like("`catch` must be followed by an error variable")?;
        let catch_body = self.parse_required_block("The catch variable must be followed by `{`")?;
        Ok(Statement::Try(
            self.empty_expr(&token),
            try_body,
            self.token_text(&error).to_string(),
            catch_body,
        ))
    }

    /// Parses a function declaration.
    fn parse_fn_statement(&mut self) -> Result<Statement, ParseError> {
        let fn_token = self.expect(TokenKind::Fn, "Internal error: fn is required")?;
        let name = self.expect_identifier_like("`fn` must be followed by a function name")?;
        self.expect(
            TokenKind::LeftParen,
            "A function name must be followed by `(`",
        )?;
        let params = self.parse_params()?;
        let body = self.parse_required_block("The parameter list must be followed by `{`")?;
        if self.token_text(&name).is_empty() {
            return Err(self.error_at(&fn_token, "The function name cannot be empty"));
        }
        Ok(Statement::Fn(
            self.token_text(&name).to_string(),
            params,
            body,
        ))
    }

    /// Parse function expression, such as `add = fn(a) { ... }`.
    fn parse_fn_expression(
        &mut self,
        token: PosToken,
        name: Option<String>,
    ) -> Result<PosExpr, ParseError> {
        self.expect(
            TokenKind::LeftParen,
            "`fn` requires a parameter list beginning with `(`",
        )?;
        let params = self.parse_params()?;
        let body = self.parse_required_block("The parameter list must be followed by `{`")?;
        Ok(self.pos_expr(&token, Expr::Fn(name.unwrap_or_default(), params, body)))
    }

    /// Parses a class declaration.
    fn parse_class_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::Class, "Internal error: class is required")?;
        let name = self.expect_identifier_like("`class` must be followed by a class name")?;
        self.expect(TokenKind::LeftBrace, "A class name must be followed by `{`")?;
        let mut members = IndexMap::new();
        self.skip_statement_separators();
        while !self.check(TokenKind::RightBrace) {
            if self.is_eof() {
                return Err(self.error_at_end("class is missing the ending `}`"));
            }
            let is_private = !self.match_kind(TokenKind::Pub);
            let key = self.expect_identifier_or_new(
                "Class members require attribute names or method names",
            )?;
            if self.match_kind(TokenKind::Colon) {
                let value = self.parse_expression(0)?;
                members.insert(
                    self.token_text(&key).to_string(),
                    (is_private, Statement::Expr(value)),
                );
            } else if self.match_kind(TokenKind::LeftParen) {
                let params = self.parse_params()?;
                let body =
                    self.parse_required_block("Class method parameters must be followed by `{`")?;
                members.insert(
                    self.token_text(&key).to_string(),
                    (
                        is_private,
                        Statement::Fn(self.token_text(&key).to_string(), params, body),
                    ),
                );
            } else {
                members.insert(
                    self.token_text(&key).to_string(),
                    (is_private, Statement::Expr(self.empty_expr(&key))),
                );
            }
            self.skip_statement_separators();
        }
        self.expect(TokenKind::RightBrace, "class is missing the ending `}`")?;
        Ok(Statement::Class(
            self.token_text(&name).to_string(),
            members,
        ))
    }

    /// Parse `for` loop.
    ///
    /// The first branch retains the original set traversal and deconstruction traversal; the new times, intervals and infinite loops are all syntactically divided here.
    /// Avoid spreading `..` as a normal expression operator to other syntax locations.
    fn parse_for_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::For, "Internal error: for")?;
        let label = self.parse_optional_label()?;
        if self.check(TokenKind::LeftBrace) {
            let body = self.parse_required_block("The for clause must be followed by `{`")?;
            return Ok(Statement::Loop(label, body));
        }
        if self.match_kind(TokenKind::LeftParen) {
            let token = self.previous().clone();
            let items = self.parse_expression_list(TokenKind::RightParen)?;
            let names = self.destructure_binding_names(&items, &token)?;
            self.expect(
                TokenKind::In,
                "A destructuring variable in `for` must be followed by `in`",
            )?;
            let iter = self.parse_expression(0)?;
            let body = self.parse_required_block("The for expression must be followed by `{`")?;
            return Ok(Statement::ForDestructure(label, names, iter, body));
        }

        if self.looks_like_for_binding() {
            let first = self.expect_identifier_like("`for` requires a variable name")?;
            self.skip_soft_separators();
            let (key, value) = if self.match_kind(TokenKind::Comma) {
                self.skip_soft_separators();
                let second = self.expect_identifier_like("The second `for` variable is missing")?;
                (
                    self.token_text(&first).to_string(),
                    self.token_text(&second).to_string(),
                )
            } else if self.check(TokenKind::Identifier) {
                let second = self.expect_identifier_like("The second `for` variable is missing")?;
                (
                    self.token_text(&first).to_string(),
                    self.token_text(&second).to_string(),
                )
            } else {
                (String::new(), self.token_text(&first).to_string())
            };
            self.skip_soft_separators();
            self.expect(TokenKind::In, ". The for variable needs `in`")?;
            let source = self.parse_for_source()?;
            let body = self.parse_required_block("The for expression must be followed by `{`")?;
            return match source {
                ForSource::Value(iter, step) => {
                    Ok(Statement::For(label, key, value, iter, step, body))
                }
                ForSource::Range(start, end, step) => Ok(Statement::ForRange(
                    label, key, value, start, end, step, body,
                )),
            };
        }

        let source = self.parse_for_source()?;
        let body = self.parse_required_block("The for expression must be followed by `{`")?;
        match source {
            ForSource::Value(count, step) => Ok(Statement::ForCount(label, count, step, body)),
            ForSource::Range(start, end, step) => Ok(Statement::ForRange(
                label,
                String::new(),
                String::new(),
                start,
                end,
                step,
                body,
            )),
        }
    }

    /// Detects a binding header such as `for value in`, `for key, value in`, or `for key value in`.
    fn looks_like_for_binding(&self) -> bool {
        let mut index = self.pos;
        if !self
            .tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Identifier)
        {
            return false;
        }
        index += 1;
        index = self.skip_soft_separator_index(index);
        if self
            .tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Comma)
        {
            index += 1;
            index = self.skip_soft_separator_index(index);
            if !self
                .tokens
                .get(index)
                .is_some_and(|token| token.kind == TokenKind::Identifier)
            {
                return false;
            }
            index += 1;
            index = self.skip_soft_separator_index(index);
        } else if self
            .tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Identifier)
        {
            index += 1;
            index = self.skip_soft_separator_index(index);
        }
        self.tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::In)
    }

    /// Parses the `for` source expression, identifying `count step n`, `a..b`, `..b`, `a..`, and the optional integer `step`.
    fn parse_for_source(&mut self) -> Result<ForSource, ParseError> {
        if self.match_kind(TokenKind::Range) {
            let range_token = self.previous().clone();
            let (end, step) = self.parse_for_range_tail(true)?;
            if end.is_none() {
                return Err(self.error_at(
                    &range_token,
                    "`..` The interval requires at least one boundary",
                ));
            }
            return Ok(ForSource::Range(None, end, step));
        }

        let start = self.parse_expression(0)?;
        if self.match_kind(TokenKind::Range) {
            let (end, step) = self.parse_for_range_tail(false)?;
            return Ok(ForSource::Range(Some(start), end, step));
        }
        let step = if self.match_step_keyword() {
            if self.check(TokenKind::LeftBrace) {
                return Err(self.error_here("`step` must be followed by a step expression"));
            }
            Some(self.parse_expression(0)?)
        } else {
            None
        };
        Ok(ForSource::Value(start, step))
    }

    /// Parses a range endpoint and optional `step`.
    fn parse_for_range_tail(
        &mut self,
        missing_start: bool,
    ) -> Result<(Option<PosExpr>, Option<PosExpr>), ParseError> {
        let end = if self.check(TokenKind::LeftBrace) || self.check_step_keyword() {
            None
        } else {
            Some(self.parse_expression(0)?)
        };
        let step = if self.match_step_keyword() {
            if self.check(TokenKind::LeftBrace) {
                return Err(self.error_here("`step` must be followed by a step expression"));
            }
            Some(self.parse_expression(0)?)
        } else {
            None
        };
        if missing_start && end.is_none() {
            return Err(self.error_here("`..` The interval requires at least one boundary"));
        }
        Ok((end, step))
    }

    /// Parse while loop.
    fn parse_while_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::While, "Internal error: while")?;
        let label = self.parse_optional_label()?;
        let condition = self.parse_expression(0)?;
        let body = self.parse_required_block("The while condition must be followed by `{`")?;
        Ok(Statement::While(label, condition, body))
    }

    /// Parses an infinite `loop` statement.
    fn parse_loop_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(TokenKind::Loop, "Internal error: loop")?;
        let label = self.parse_optional_label()?;
        let body = self.parse_required_block("is required. `{`")?;
        Ok(Statement::Loop(label, body))
    }

    /// Parses a return statement after the `return` keyword.
    fn parse_return_statement(&mut self) -> Result<Statement, ParseError> {
        let token = self.expect(TokenKind::Return, "Internal error: expected `return`")?;
        if self.is_statement_end() {
            Ok(Statement::Return(self.empty_expr(&token)))
        } else {
            Ok(Statement::Return(self.parse_expression(0)?))
        }
    }

    /// Parses a `throw value` statement.
    fn parse_throw_statement(&mut self) -> Result<Statement, ParseError> {
        let token = self.expect(TokenKind::Throw, "Internal error: expected `throw`")?;
        if self.is_statement_end() {
            Ok(Statement::Throw(self.empty_expr(&token)))
        } else {
            Ok(Statement::Throw(self.parse_expression(0)?))
        }
    }

    /// Parses a `break` or `continue` statement.
    fn parse_jump_statement(&mut self, is_break: bool) -> Result<Statement, ParseError> {
        if is_break {
            self.expect(TokenKind::Break, "Internal error: break")?;
        } else {
            self.expect(TokenKind::Continue, "Internal error: expected `continue`")?;
        }
        let label = if self.match_kind(TokenKind::Colon) {
            let label = self.expect_identifier_like("Loop jump label is missing a name")?;
            self.token_text(&label).to_string()
        } else {
            String::new()
        };
        if is_break {
            Ok(Statement::Break(label))
        } else {
            Ok(Statement::Continue(label))
        }
    }

    /// Parses a required code block.
    fn parse_required_block(&mut self, message: &str) -> Result<Vec<Statement>, ParseError> {
        self.expect(TokenKind::LeftBrace, message)?;
        self.block_depth += 1;
        let mut statements = Vec::new();
        self.skip_statement_separators();
        while !self.check(TokenKind::RightBrace) {
            if self.is_eof() {
                self.block_depth = self.block_depth.saturating_sub(1);
                return Err(self.error_at_end("code block lacks the ending `}`"));
            }
            let statement = self.parse_statement()?;
            if statement != Statement::Empty {
                statements.push(statement);
            }
            self.skip_statement_separators();
        }
        self.block_depth = self.block_depth.saturating_sub(1);
        self.expect(TokenKind::RightBrace, "code block lacks the ending `}`")?;
        Ok(statements)
    }

    /// Parses a function parameter list after `(`.
    fn parse_params(&mut self) -> Result<Vec<(String, Option<PosExpr>)>, ParseError> {
        let mut params = Vec::new();
        self.skip_soft_separators();
        while !self.check(TokenKind::RightParen) {
            if self.is_eof() {
                return Err(self.error_at_end("Parameter list missing terminating `)`"));
            }
            let name = self.expect_identifier_like("Parameter requires name")?;
            let default = if self.match_kind(TokenKind::Assign) {
                Some(self.parse_expression(0)?)
            } else {
                None
            };
            params.push((self.token_text(&name).to_string(), default));
            if !self.consume_list_separator(TokenKind::RightParen) {
                break;
            }
        }
        self.expect(
            TokenKind::RightParen,
            "Parameter list missing terminating `)`",
        )?;
        Ok(params)
    }

    /// Parse expression list, supports comma, newline, or directly adjacent expression separation.
    fn parse_expression_list(&mut self, end: TokenKind) -> Result<Vec<PosExpr>, ParseError> {
        let mut items = Vec::new();
        self.skip_soft_separators();
        while !self.check(end.clone()) {
            if self.is_eof() {
                return Err(self.error_at_end(format!(
                    "Missing terminator of list `{}`",
                    Self::kind_name(&end)
                )));
            }
            items.push(self.parse_expression(0)?);
            self.consume_list_separator(end.clone());
        }
        self.expect(end, "Missing terminator of list")?;
        Ok(items)
    }

    /// Parse array literal.
    fn parse_array_literal(&mut self, token: PosToken) -> Result<PosExpr, ParseError> {
        let items = self.parse_expression_list(TokenKind::RightBracket)?;
        let expr = self.pos_expr(&token, Expr::Array(items));
        self.parse_postfix(expr)
    }

    /// Parses match expressions.
    fn parse_match_expression(&mut self, token: PosToken) -> Result<PosExpr, ParseError> {
        let target = self.parse_expression(0)?;
        self.expect(
            TokenKind::LeftBrace,
            "match requires `{` after the expression",
        )?;
        let mut arms = Vec::new();
        self.skip_statement_separators();
        while !self.check(TokenKind::RightBrace) {
            if self.is_eof() {
                return Err(self.error_at_end("match lacks the ending `}`"));
            }
            let pattern = if self.check(TokenKind::Identifier)
                && self
                    .current()
                    .is_some_and(|current| self.token_text(current) == "_")
                && self.peek_kind(1) == Some(TokenKind::FatArrow)
            {
                self.advance();
                None
            } else {
                Some(self.parse_expression(0)?)
            };
            self.expect(TokenKind::FatArrow, "match branch requires `=>`")?;
            let value = if self.check(TokenKind::LeftBrace) {
                let block = self
                    .current()
                    .cloned()
                    .expect("current token was already confirmed to be a code block");
                let statements =
                    self.parse_required_block("match branch code block requires `{`")?;
                self.pos_expr(&block, Expr::Statement(statements))
            } else {
                self.parse_expression(0)?
            };
            arms.push(MatchArm { pattern, value });
            if !self.consume_list_separator(TokenKind::RightBrace) {
                break;
            }
        }
        self.expect(TokenKind::RightBrace, "match lacks the ending `}`")?;
        let expr = self.pos_expr(&token, Expr::Match(Box::new(target), arms));
        self.parse_postfix(expr)
    }

    /// Parses object literals.
    fn parse_object_literal(&mut self, token: PosToken) -> Result<PosExpr, ParseError> {
        let mut properties = Vec::new();
        self.skip_statement_separators();
        while !self.check(TokenKind::RightBrace) {
            if self.is_eof() {
                return Err(self.error_at_end("Object literal missing terminating `}`"));
            }
            let key = self.expect_object_key()?;
            if self.match_kind(TokenKind::Colon) {
                let value = if self.match_kind(TokenKind::Fn) {
                    let fn_token = self.previous().clone();
                    self.parse_fn_expression(fn_token, Some(self.token_text(&key).to_string()))?
                } else {
                    self.parse_expression(0)?
                };
                properties.push((self.key_text(&key), value));
            } else if self.match_kind(TokenKind::LeftParen) {
                let params = self.parse_params()?;
                let body =
                    self.parse_required_block("Object method parameters must be followed by `{`")?;
                properties.push((
                    self.key_text(&key),
                    self.pos_expr(&key, Expr::Fn(self.key_text(&key), params, body)),
                ));
            } else {
                return Err(self.error_at(&key, "Object attribute requires `:` or `(`"));
            }
            self.consume_list_separator(TokenKind::RightBrace);
        }
        self.expect(
            TokenKind::RightBrace,
            "Object literal missing terminating `}`",
        )?;
        let expr = self.pos_expr(&token, Expr::Object(properties));
        self.parse_postfix(expr)
    }

    /// Parses parentheses expressions and recognizes `(a, b=1) -> ...` arrow functions.
    ///
    /// When parentheses are followed by ordinary assignment symbols, only variable names are allowed within the parentheses and will be retained as destructuring bindings;
    /// Other positions are still processed as ordinary grouping expressions to avoid introducing independent runtime types.
    fn parse_group_or_arrow(&mut self, token: PosToken) -> Result<PosExpr, ParseError> {
        let checkpoint = self.pos;
        if let Ok(params) = self.try_parse_arrow_params_in_group() {
            if self.match_kind(TokenKind::Arrow) {
                return self.parse_arrow_body(params, &token);
            }
        }
        self.pos = checkpoint;
        let items = self.parse_expression_list(TokenKind::RightParen)?;
        if self.check(TokenKind::Assign) {
            let names = self.destructure_binding_names(&items, &token)?;
            return Ok(self.pos_expr(&token, Expr::Destructure(names)));
        }
        let expr = if let [single] = items.as_slice() {
            single.clone()
        } else {
            self.pos_expr(&token, Expr::Vec(items))
        };
        self.parse_postfix(expr)
    }

    /// Extracts destructuring variable names from a list of parenthesized expressions. The first version of
    ///
    /// only accepts ordinary variable names; nested destructuring, default values, and renames are rejected here to avoid additional branches in subsequent compilation and
    /// VM execution phases.
    fn destructure_binding_names(
        &self,
        items: &[PosExpr],
        token: &PosToken,
    ) -> Result<Vec<String>, ParseError> {
        if items.is_empty() {
            return Err(self.error_at(
                token,
                "Destructuring assignment requires at least one variable name",
            ));
        }
        let mut names = Vec::with_capacity(items.len());
        for item in items {
            let Expr::Variable(name) = &item.expr else {
                return Err(self.error_at(
                    token,
                    "Destructuring assignment currently supports only variable names, without nesting, defaults, or renaming",
                ));
            };
            names.push(name.clone());
        }
        Ok(names)
    }

    /// Try to parse the contents of parentheses into arrow function parameters.
    fn try_parse_arrow_params_in_group(
        &mut self,
    ) -> Result<Vec<(String, Option<PosExpr>)>, ParseError> {
        let mut params = Vec::new();
        self.skip_soft_separators();
        while !self.check(TokenKind::RightParen) {
            let name = self.expect_identifier_like("Arrow function parameter requires name")?;
            let default = if self.match_kind(TokenKind::Assign) {
                Some(self.parse_expression(0)?)
            } else {
                None
            };
            params.push((self.token_text(&name).to_string(), default));
            if !self.consume_list_separator(TokenKind::RightParen) {
                break;
            }
        }
        self.expect(
            TokenKind::RightParen,
            "Arrow function parameter is missing `)`",
        )?;
        Ok(params)
    }

    /// Parse arrow function body.
    fn parse_arrow_body(
        &mut self,
        params: Vec<(String, Option<PosExpr>)>,
        token: &PosToken,
    ) -> Result<PosExpr, ParseError> {
        let body = if self.check(TokenKind::LeftBrace) {
            self.parse_required_block("Arrow function body requires `{`")?
        } else {
            vec![Statement::Expr(self.parse_expression(0)?)]
        };
        Ok(self.pos_expr(token, Expr::Fn(String::new(), params, body)))
    }

    /// Converts an expression into an arrow-function parameter list.
    fn expr_to_arrow_params(
        &self,
        expr: PosExpr,
        token: &PosToken,
    ) -> Result<Vec<(String, Option<PosExpr>)>, ParseError> {
        match expr.expr {
            Expr::Variable(name) => Ok(vec![(name, None)]),
            Expr::Vec(items) => items
                .into_iter()
                .map(|item| match item.expr {
                    Expr::Variable(name) => Ok((name, None)),
                    _ => Err(self.error_at(
                        token,
                        "Arrow function parameters can only be variable names",
                    )),
                })
                .collect(),
            _ => Err(self.error_at(
                token,
                "The left side of the arrow function requires parameter names",
            )),
        }
    }

    /// Parses compound assignment expressions.
    fn parse_compound_assignment(
        &mut self,
        left: &PosExpr,
        op: &PosToken,
    ) -> Result<Option<PosExpr>, ParseError> {
        let build = match op.kind {
            TokenKind::PlusAssign => Expr::PlusAssign as fn(Box<PosExpr>, Box<PosExpr>) -> Expr,
            TokenKind::MinusAssign => Expr::MinusAssign,
            TokenKind::MultiplyAssign => Expr::MultiplyAssign,
            TokenKind::DivideAssign => Expr::DivideAssign,
            TokenKind::ModuloAssign => Expr::ModuloAssign,
            TokenKind::ShiftLeftAssign => Expr::ShiftLeftAssign,
            TokenKind::ShiftRightAssign => Expr::ShiftRightAssign,
            TokenKind::BitAndAssign => Expr::BitAndAssign,
            TokenKind::BitXorAssign => Expr::BitXorAssign,
            TokenKind::BitOrAssign => Expr::BitOrAssign,
            _ => return Ok(None),
        };
        self.advance();
        let right = self.parse_expression(1)?;
        Ok(Some(self.pos_expr(
            op,
            build(Box::new(left.clone()), Box::new(right)),
        )))
    }

    /// Operator binding force table, the larger the number, the higher the priority.
    fn binary_binding_power(kind: &TokenKind) -> Option<(u8, u8)> {
        match kind {
            TokenKind::Coalesce => Some((2, 3)),
            TokenKind::Or => Some((4, 5)),
            TokenKind::And => Some((6, 7)),
            TokenKind::BitOr => Some((8, 9)),
            TokenKind::Xor => Some((10, 11)),
            TokenKind::BitAnd => Some((12, 13)),
            TokenKind::Equal
            | TokenKind::NotEqual
            | TokenKind::StrictEqual
            | TokenKind::StrictNotEqual => Some((14, 15)),
            TokenKind::Less
            | TokenKind::LessEqual
            | TokenKind::Greater
            | TokenKind::GreaterEqual => Some((16, 17)),
            TokenKind::ShiftLeft | TokenKind::ShiftRight => Some((18, 19)),
            TokenKind::Plus | TokenKind::Minus => Some((20, 21)),
            TokenKind::Multiply | TokenKind::Divide | TokenKind::Modulo => Some((22, 23)),
            TokenKind::Power => Some((25, 24)),
            _ => None,
        }
    }

    /// Constructs the attribute access chain.
    fn chain_property(&self, left: PosExpr, right: PosExpr) -> PosExpr {
        let mut items = match left.expr {
            Expr::ObjectProperty(items) => items,
            _ => vec![left.clone()],
        };
        items.push(right);
        PosExpr {
            expr: Expr::ObjectProperty(items),
            file: left.file,
            line: left.line,
            column: left.column,
        }
    }

    /// Returns whether the current token ends an expression.
    fn is_expression_stop(&self, kind: &TokenKind) -> bool {
        matches!(
            kind,
            TokenKind::RightParen
                | TokenKind::RightBracket
                | TokenKind::RightBrace
                | TokenKind::Wrap
                | TokenKind::Comma
                | TokenKind::Range
                | TokenKind::FatArrow
                | TokenKind::Semicolon
                | TokenKind::Else
                | TokenKind::ElseIf
                | TokenKind::Catch
        )
    }

    /// Returns whether the current token is a statement boundary.
    fn is_statement_end(&self) -> bool {
        self.is_eof()
            || self.current().is_some_and(|token| {
                matches!(
                    token.kind,
                    TokenKind::Wrap | TokenKind::Semicolon | TokenKind::RightBrace
                )
            })
    }

    /// Consumption list separator.
    fn consume_list_separator(&mut self, end: TokenKind) -> bool {
        let mut consumed = false;
        while self.match_kind(TokenKind::Comma)
            || self.match_kind(TokenKind::Wrap)
            || self.match_kind(TokenKind::Semicolon)
        {
            consumed = true;
        }
        consumed || !self.check(end)
    }

    /// Skips statement-level delimiters.
    fn skip_statement_separators(&mut self) {
        while self.match_kind(TokenKind::Wrap)
            || self.match_kind(TokenKind::Semicolon)
            || self.match_kind(TokenKind::Comma)
        {}
    }

    /// Skips allowed soft delimiters inside expressions.
    fn skip_soft_separators(&mut self) {
        while self.match_kind(TokenKind::Wrap) || self.match_kind(TokenKind::Semicolon) {}
    }

    /// Skips soft delimiters starting at the specified subscript and returns the first non-delimited token subscript.
    fn skip_soft_separator_index(&self, mut index: usize) -> usize {
        while self
            .tokens
            .get(index)
            .is_some_and(|token| matches!(token.kind, TokenKind::Wrap | TokenKind::Semicolon))
        {
            index += 1;
        }
        index
    }

    /// Detects the special `step` keyword in a range loop.
    fn check_step_keyword(&self) -> bool {
        self.current().is_some_and(|token| {
            token.kind == TokenKind::Identifier && self.token_text(token) == "step"
        })
    }

    /// If the current position is an interval loop-specific `step` mark, consume it.
    fn match_step_keyword(&mut self) -> bool {
        if self.check_step_keyword() {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Allows only `.` to continue an expression across a newline; `[` must remain adjacent.
    fn skip_wraps_before_dot(&mut self) {
        let mut index = self.pos;
        while self
            .tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Wrap)
        {
            index += 1;
        }
        if self
            .tokens
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Dot)
        {
            self.pos = index;
        }
    }

    /// Determines whether the current token is directly attached to the previous token, without spaces, newlines or semicolons in between.
    fn is_tight_to_previous(&self, token: &PosToken) -> bool {
        self.pos > 0
            && self
                .tokens
                .get(self.pos - 1)
                .is_some_and(|previous| previous.end == token.start)
    }

    /// Parses optional loop tags.
    fn parse_optional_label(&mut self) -> Result<String, ParseError> {
        if self.match_kind(TokenKind::Colon) {
            let label = self.expect_identifier_like("Label is missing a name")?;
            Ok(self.token_text(&label).to_string())
        } else {
            Ok(String::new())
        }
    }

    /// Get object key.
    fn expect_object_key(&mut self) -> Result<PosToken, ParseError> {
        let token = self
            .advance()
            .ok_or_else(|| self.error_at_end("Object attribute is missing key"))?;
        match token.kind {
            TokenKind::Identifier | TokenKind::Str | TokenKind::Int | TokenKind::Float => Ok(token),
            _ => Err(self.error_at(
                &token,
                "Object attribute key can only be an identifier, string or number",
            )),
        }
    }

    /// Read identifier or ordinary keyword name.
    fn expect_identifier_like(&mut self, message: &str) -> Result<PosToken, ParseError> {
        let token = self.advance().ok_or_else(|| self.error_at_end(message))?;
        match token.kind {
            TokenKind::Identifier => Ok(token),
            _ => Err(self.error_at(&token, message)),
        }
    }

    /// Reads a property name after a dot.
    ///
    /// Because this position follows `.`, keywords such as `match` may still be used
    /// as method or field names without colliding with language syntax.
    fn expect_property_name(&mut self, message: &str) -> Result<PosToken, ParseError> {
        let token = self.advance().ok_or_else(|| self.error_at_end(message))?;
        match &token.kind {
            TokenKind::Identifier
            | TokenKind::Use
            | TokenKind::Let
            | TokenKind::Print
            | TokenKind::Println
            | TokenKind::If
            | TokenKind::Else
            | TokenKind::ElseIf
            | TokenKind::Fn
            | TokenKind::Return
            | TokenKind::Class
            | TokenKind::Pub
            | TokenKind::New
            | TokenKind::While
            | TokenKind::Loop
            | TokenKind::For
            | TokenKind::In
            | TokenKind::Break
            | TokenKind::Continue
            | TokenKind::Try
            | TokenKind::Catch
            | TokenKind::Throw
            | TokenKind::Match => Ok(token),
            _ => Err(self.error_at(&token, message)),
        }
    }

    /// Reads a constructor name, allowing `new` itself as a method name.
    fn expect_identifier_or_new(&mut self, message: &str) -> Result<PosToken, ParseError> {
        let token = self.advance().ok_or_else(|| self.error_at_end(message))?;
        match token.kind {
            TokenKind::Identifier | TokenKind::New => Ok(token),
            _ => Err(self.error_at(&token, message)),
        }
    }

    /// Consumes the expected token.
    fn expect(&mut self, kind: TokenKind, message: &str) -> Result<PosToken, ParseError> {
        let token = self.advance().ok_or_else(|| self.error_at_end(message))?;
        if token.kind == kind {
            Ok(token)
        } else {
            Err(self.error_at(&token, message))
        }
    }

    /// Consumes the current token type if it matches.
    fn match_kind(&mut self, kind: TokenKind) -> bool {
        if self.check(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Checks the current token type.
    fn check(&self, kind: TokenKind) -> bool {
        self.current().is_some_and(|token| token.kind == kind)
    }

    /// Check the subsequent token type.
    fn peek_kind(&self, offset: usize) -> Option<TokenKind> {
        self.tokens
            .get(self.pos + offset)
            .map(|token| token.kind.clone())
    }

    /// The current token.
    fn current(&self) -> Option<&PosToken> {
        self.tokens.get(self.pos)
    }

    /// Previous token.
    fn previous(&self) -> &PosToken {
        &self.tokens[self.pos - 1]
    }

    /// Consumes and returns the current token.
    fn advance(&mut self) -> Option<PosToken> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    /// Fallback a token for reparsing a small number of keywords.
    fn rewind_one(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    /// Whether the end of file is reached.
    fn is_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// Reads source text from the token's byte span.
    ///
    /// Tokens do not cache their text. A string is allocated at the call site only when the AST
    /// actually needs it.
    fn token_text(&self, token: &PosToken) -> &str {
        self.source.get(token.start..token.end).unwrap_or_default()
    }

    /// Create positional expressions.
    fn pos_expr(&self, token: &PosToken, expr: Expr) -> PosExpr {
        PosExpr {
            expr,
            file: token.file.clone(),
            line: token.line,
            column: token.column,
        }
    }

    /// Creates an empty expression at the given token.
    fn empty_expr(&self, token: &PosToken) -> PosExpr {
        self.pos_expr(token, Expr::Empty)
    }

    /// Parses an integer literal.
    fn parse_int_literal(&self, token: PosToken) -> Result<PosExpr, ParseError> {
        let token_text = self.token_text(&token);
        let text = token_text.replace('_', "");
        let value = text.parse::<i64>().map_err(|_| {
            self.error_at(
                &token,
                format!("Integer `{}` out of representable range", token_text),
            )
        })?;
        Ok(self.pos_expr(&token, Expr::Int(value)))
    }

    /// Parsed floating-point numeric literal.
    fn parse_float_literal(&self, token: PosToken) -> Result<PosExpr, ParseError> {
        let token_text = self.token_text(&token);
        let text = token_text.replace('_', "");
        let value = text.parse::<f64>().map_err(|_| {
            self.error_at(
                &token,
                format!("Floating point number `{}` cannot be parsed", token_text),
            )
        })?;
        Ok(self.pos_expr(&token, Expr::Float(value)))
    }

    /// String unquotes and handles common escapes.
    fn unquote(text: &str) -> String {
        let inner = text
            .strip_prefix(['"', '\''])
            .and_then(|s| s.strip_suffix(['"', '\'']))
            .unwrap_or(text);
        Self::unescape(inner)
    }

    /// The template string removes the backticks, and the internal interpolation is left to the execution phase.
    fn unquote_template(text: &str) -> String {
        let inner = text
            .strip_prefix('`')
            .and_then(|s| s.strip_suffix('`'))
            .unwrap_or(text);
        Self::unescape(inner)
    }

    /// Decodes basic string escape sequences.
    fn unescape(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                result.push(ch);
                continue;
            }
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('\'') => result.push('\''),
                Some('`') => result.push('`'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        }
        result
    }

    /// Object key normalized to string.
    fn key_text(&self, token: &PosToken) -> String {
        match token.kind {
            TokenKind::Str => Self::unquote(self.token_text(token)),
            _ => self.token_text(token).to_string(),
        }
    }

    /// Creates an error at the current location.
    fn error_here(&self, message: impl Into<String>) -> ParseError {
        if let Some(token) = self.current() {
            self.error_at(token, message)
        } else {
            self.error_at_end(message)
        }
    }

    /// Generates an error for the specified token.
    fn error_at(&self, token: &PosToken, message: impl Into<String>) -> ParseError {
        ParseError {
            file: token.file.clone(),
            line: token.line,
            column: token.column,
            message: message.into(),
            source_line: self.line_text(token.line),
        }
    }

    /// Generate end-of-file error.
    fn error_at_end(&self, message: impl Into<String>) -> ParseError {
        let (line, column) = self.end_line_column();
        ParseError {
            file: self.file.clone(),
            line,
            column,
            message: message.into(),
            source_line: self.line_text(line),
        }
    }

    /// Gets the source code of the specified line.
    fn line_text(&self, line: usize) -> String {
        self.source
            .lines()
            .nth(line.saturating_sub(1))
            .unwrap_or_default()
            .to_string()
    }

    /// Calculates the ending row and column of the source code.
    fn end_line_column(&self) -> (usize, usize) {
        let mut line = 1;
        let mut column = 1;
        for ch in self.source.chars() {
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        (line, column)
    }

    /// Display name for a token type.
    fn kind_name(kind: &TokenKind) -> &'static str {
        match kind {
            TokenKind::RightParen => ")",
            TokenKind::RightBracket => "]",
            TokenKind::RightBrace => "}",
            _ => "token",
        }
    }
}
