//! BT bytecode definition.
//!
//! BT uses register-based bytecode: expression results are written directly to virtual registers, avoiding frequent push/pop operations in a stack VM.
//! This layout also makes future quickening and inline caches easier to implement by replacing generic instructions with specialized ones.

use crate::lexer::TokenKind;
use crate::value::Value;
use std::collections::HashMap;
use std::mem;

/// The current in-process bytecode format version.
///
/// This value invalidates in-process compilation caches. Increment it whenever the
/// layout or semantics of `Instruction`, `Chunk`, the constant pool, or function
/// blocks change, so long-running processes cannot reuse stale bytecode.
pub const BYTECODE_FORMAT_VERSION: u32 = 1;

/// Constant pool index.
pub type ConstId = u32;
/// Index into the symbol pool, used for variable and attribute names.
pub type SymbolId = u32;
/// Virtual register number.
pub type Register = u32;

/// Source location associated with a bytecode instruction.
///
/// The VM uses this information to map an instruction back to a user-facing file,
/// line, and column when reporting an error.
#[derive(Debug, Clone)]
pub struct SourceSpan {
    /// Source code file name.
    pub file: String,
    /// Starting line number, starting from 1.
    pub line: usize,
    /// Starting column number, starting from 1.
    pub column: usize,
}

/// Compiled bytecode block.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Source file that produced this bytecode block.
    ///
    /// Runtime path resolution uses this field to recover the defining location of
    /// functions, included files, and template fragments. Instruction spans alone
    /// are not enough to reconstruct that context reliably.
    pub source_file: String,
    /// Directory containing the source for this bytecode block.
    ///
    /// Relative paths are resolved from this directory. The VM stores the project
    /// root once rather than duplicating it in every `Chunk`.
    pub source_dir: String,
    /// Constant pool, which stores immutable constants such as numbers and strings.
    pub constants: Vec<Value>,
    /// Symbol pool for variable and attribute names.
    pub symbols: SymbolPool,
    /// Instruction sequence.
    pub code: Vec<Instruction>,
    /// The source code location corresponding to each instruction; the length is always consistent with `code`.
    pub spans: Vec<Option<SourceSpan>>,
    /// Function bytecode table, function expressions and function declarations will be compiled here.
    pub functions: Vec<FunctionChunk>,
    /// Symbols local to this bytecode block.
    ///
    /// `let` bindings, function parameters, `this`, and loop bindings are marked
    /// here. Unmarked symbols fall through to the enclosing closure or global
    /// environment, preserving JavaScript-style scoping.
    pub local_symbols: Vec<bool>,
    /// The number of registers allocated at compile time.
    pub register_count: Register,
}

impl Chunk {
    /// Creates an empty bytecode block.
    pub fn new() -> Self {
        Self {
            source_file: String::new(),
            source_dir: String::new(),
            constants: Vec::new(),
            symbols: SymbolPool::new(),
            code: Vec::new(),
            spans: Vec::new(),
            functions: Vec::new(),
            local_symbols: Vec::new(),
            register_count: 0,
        }
    }

    /// Marks a symbol as local to this bytecode block.
    pub fn mark_local(&mut self, symbol: SymbolId) {
        let index = symbol as usize;
        if index >= self.local_symbols.len() {
            self.local_symbols.resize(index + 1, false);
        }
        self.local_symbols[index] = true;
    }

    /// Returns whether a symbol is local to this bytecode block.
    pub fn is_local(&self, symbol: SymbolId) -> bool {
        self.local_symbols
            .get(symbol as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Adds a constant and returns its pool index.
    pub fn add_constant(&mut self, value: Value) -> ConstId {
        let id = self.constants.len() as ConstId;
        self.constants.push(value);
        id
    }

    /// Write command.
    pub fn emit(&mut self, instruction: Instruction) {
        self.code.push(instruction);
        self.spans.push(None);
    }

    /// Write instructions with source code location.
    ///
    /// The compiler should give priority to calling this method so that runtime errors can accurately fall back to the user source code.
    pub fn emit_at(&mut self, instruction: Instruction, span: SourceSpan) {
        self.code.push(instruction);
        self.spans.push(Some(span));
    }

    /// Writes a function and returns the function number.
    pub fn add_function(&mut self, function: FunctionChunk) -> u32 {
        let id = self.functions.len() as u32;
        self.functions.push(function);
        id
    }

    /// Estimates the number of bytes of heap memory held by the current bytecode block.
    ///
    /// This estimate bounds the in-process compilation cache rather than mirroring allocator metadata. It counts strings, Vec capacity,
    /// nested function blocks, and constant literals owned directly by the `Chunk`, preventing cache entries from growing without limit in a resident process.
    pub fn estimated_heap_bytes(&self) -> usize {
        let mut bytes = mem::size_of::<Self>()
            .saturating_add(self.source_file.len())
            .saturating_add(self.source_dir.len())
            .saturating_add(self.constants.capacity() * mem::size_of::<Value>())
            .saturating_add(self.symbols.estimated_heap_bytes())
            .saturating_add(self.code.capacity() * mem::size_of::<Instruction>())
            .saturating_add(self.spans.capacity() * mem::size_of::<Option<SourceSpan>>())
            .saturating_add(self.functions.capacity() * mem::size_of::<FunctionChunk>())
            .saturating_add(self.local_symbols.capacity() * mem::size_of::<bool>());
        for value in &self.constants {
            bytes = bytes.saturating_add(value.estimated_literal_heap_bytes());
        }
        for span in self.spans.iter().flatten() {
            bytes = bytes.saturating_add(span.file.len());
        }
        for function in &self.functions {
            bytes = bytes.saturating_add(function.estimated_heap_bytes());
        }
        bytes
    }
}

/// Compiled function block.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct FunctionChunk {
    /// Function name, anonymous function is an empty string.
    pub name: String,
    /// Parameter definition.
    pub params: Vec<FunctionParam>,
    /// Function body bytecode.
    pub chunk: Box<Chunk>,
}

impl FunctionChunk {
    /// Estimates the number of bytes of heap memory held by a function bytecode block.
    ///
    /// The function name, parameter list and function body all belong to the cache cost of the parent `Chunk` and need to be included in the estimate when the cache is eliminated.
    pub fn estimated_heap_bytes(&self) -> usize {
        mem::size_of::<Self>()
            .saturating_add(self.name.len())
            .saturating_add(self.params.capacity() * mem::size_of::<FunctionParam>())
            .saturating_add(self.chunk.estimated_heap_bytes())
    }
}

/// Compiled function parameters.
///
/// Default values are currently stored as evaluated literals, which covers common
/// forms such as `a=1`, `name='bt'`, and `flag=true`. If defaults later need to
/// reference earlier parameters, this can be extended to store bytecode instead.
#[derive(Debug, Clone)]
pub struct FunctionParam {
    /// Parameter name symbol number.
    pub symbol: SymbolId,
    /// The default value used when no parameters are passed.
    pub default: Option<Value>,
}

/// Symbol pool.
///
/// Variable names are interned as integers so runtime lookup can avoid repeated
/// string comparisons on hot paths.
#[derive(Debug, Clone)]
pub struct SymbolPool {
    /// Symbol text list.
    names: Vec<String>,
    /// Symbol text-to-number index.
    index: HashMap<String, SymbolId>,
}

impl SymbolPool {
    /// Creates an empty symbol pool.
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Interns a symbol or reuses its existing index.
    pub fn intern(&mut self, name: &str) -> SymbolId {
        if let Some(id) = self.index.get(name) {
            return *id;
        }
        let id = self.names.len() as SymbolId;
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), id);
        id
    }

    /// Reads symbol text by index.
    #[allow(dead_code)]
    pub fn name(&self, id: SymbolId) -> Option<&str> {
        self.names.get(id as usize).map(String::as_str)
    }

    /// Looks up the index of an existing symbol.
    ///
    /// The VM uses this when binding `this` and initializing local variables. The
    /// lookup never interns new symbols, so runtime execution cannot mutate the
    /// compiled bytecode structure.
    pub fn id(&self, name: &str) -> Option<SymbolId> {
        self.index.get(name).copied()
    }

    /// Returns the number of symbols.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Estimates the heap memory held by the symbol pool.
    ///
    /// The pool lives as long as its `Chunk` after a cache hit, so cache accounting includes
    /// both symbol text and index-table capacity.
    pub fn estimated_heap_bytes(&self) -> usize {
        let names_bytes = self
            .names
            .iter()
            .fold(0usize, |total, name| total.saturating_add(name.len()));
        self.names
            .capacity()
            .saturating_mul(mem::size_of::<String>())
            .saturating_add(names_bytes)
            .saturating_add(
                self.index
                    .capacity()
                    .saturating_mul(mem::size_of::<(String, SymbolId)>()),
            )
    }
}

/// Bytecode instructions.
#[derive(Debug, Clone)]
pub enum Instruction {
    /// Loads a constant into a register.
    LoadConst {
        /// Destination register.
        dst: Register,
        /// Constant pool number.
        constant: ConstId,
    },
    /// Expands a template string.
    ///
    /// The compiler keeps the original backtick text because interpolation reads
    /// variables from the current runtime scope. This instruction evaluates each
    /// `${...}` placeholder and assembles the final string.
    ExpandTemplate {
        /// Destination register.
        dst: Register,
        /// The register where the template original text is located.
        src: Register,
    },
    /// Copies a register value.
    ///
    /// Control flows such as conditional expressions will generate different registers in different branches; by explicitly copying to the unified target register,
    /// subsequent instructions can treat the entire control flow as a normal expression and continue to pass the return value.
    Move {
        /// Destination register.
        dst: Register,
        /// Source register.
        src: Register,
    },
    /// Loads a variable from the global environment.
    LoadGlobal {
        /// Destination register.
        dst: Register,
        /// Variable symbol number.
        symbol: SymbolId,
    },
    /// Writes a register value to the global environment.
    StoreGlobal {
        /// Variable symbol number.
        symbol: SymbolId,
        /// Source register.
        src: Register,
    },
    /// Defines a constant binding.
    ///
    /// A constant may only be written once. Top-level constants go to the global constant table;
    /// constants inside functions go to a local slot in the current function.
    /// Executing this instruction again within the same function call will trigger a runtime error.
    StoreConst {
        /// Constant symbol number.
        symbol: SymbolId,
        /// Source register.
        src: Register,
    },
    /// Performs destructuring assignment by array index or matching object field.
    DestructureAssign {
        /// The register where the value on the right side is located.
        src: Register,
        /// Left-hand variable symbol list.
        symbols: Vec<SymbolId>,
        /// Matches `symbols` and marks which bindings are constants.
        constants: Vec<bool>,
    },
    /// Binary operations.
    Binary {
        /// Operator.
        op: TokenKind,
        /// Destination register.
        dst: Register,
        /// Left operand register.
        lhs: Register,
        /// Right operand register.
        rhs: Register,
    },
    /// Performs increment or decrement on the value.
    ///
    /// This instruction accepts only integers and floating-point numbers. It deliberately does not reuse binary `+`, which also performs string concatenation,
    /// so `text++` cannot be mistaken for a string append. `delta` can currently only be `1` or `-1`.
    Increment {
        /// Destination register.
        dst: Register,
        /// Original value register.
        src: Register,
        /// Increment, `1` means self-increment, `-1` means self-decrement.
        delta: i8,
    },
    /// Logical NOT operation.
    ///
    /// `!value` directly reuses the unified truth value rules at runtime without numerical operations, ensuring that `null`, `empty`,
    /// `0`, blank strings, empty arrays and empty objects can naturally participate in judgment as condition values.
    Not {
        /// Destination register.
        dst: Register,
        /// Source register.
        src: Register,
    },
    /// Bitwise negation operation.
    ///
    /// `~value` uses runtime integer conversion rules and results remain as integer values.
    BitNot {
        /// Destination register.
        dst: Register,
        /// Source register.
        src: Register,
    },
    /// Creates an array with elements from a set of registers.
    MakeArray {
        /// Destination register.
        dst: Register,
        /// Element register list.
        items: Vec<Register>,
    },
    /// Creates an object from symbol-pool keys and value registers.
    MakeObject {
        /// Destination register.
        dst: Register,
        /// List of object properties.
        entries: Vec<(SymbolId, Register)>,
    },
    /// Read attributes or subscripts.
    GetProperty {
        /// Destination register.
        dst: Register,
        /// Object register.
        object: Register,
        /// Attribute name or subscript register.
        key: Register,
    },
    /// Write attributes or subscripts.
    SetProperty {
        /// Object register.
        object: Register,
        /// Attribute name or subscript register.
        key: Register,
        /// New value register.
        value: Register,
    },
    /// Creates a function value.
    MakeFunction {
        /// Destination register.
        dst: Register,
        /// Function table number.
        function: u32,
    },
    /// Creates a class value.
    MakeClass {
        /// Destination register.
        dst: Register,
        /// Class name symbol.
        name: SymbolId,
        /// Member name, member value register and public flag.
        members: Vec<(SymbolId, Register, bool)>,
    },
    /// Converts iterable values to VM-internal iterators.
    ///
    /// Arrays, objects, and strings maintain existing snapshot semantics; integers are converted to lazy iterators, and elements are not preallocated.
    IterInit {
        /// Destination register.
        dst: Register,
        /// The register where the value to be traversed is located.
        iterable: Register,
    },
    /// Creates a strict integer iterator by count.
    CountInit {
        /// Destination register.
        dst: Register,
        /// The register where the times value is located.
        count: Register,
        /// Optional positive integer step size register; when empty, the default step size `1` is used.
        step: Option<Register>,
    },
    /// Creates a closed range or endless range iterator.
    RangeInit {
        /// Destination register.
        dst: Register,
        /// The register where the starting point value is located.
        start: Register,
        /// The register where the optional end point value is located; empty means there is no end point increment interval.
        end: Option<Register>,
        /// Register where the positive integer step size is located.
        step: Register,
    },
    /// Reads the next item of the iterator and writes it to the loop variable.
    IterNext {
        /// Iterator register.
        iterator: Register,
        /// Key variable symbol, `None` when there is no key variable.
        key_symbol: Option<SymbolId>,
        /// Value variable symbol, `None` when dropped without a value variable or using `_`.
        value_symbol: Option<SymbolId>,
        /// The instruction index to jump to at the end of the iteration.
        jump_to_end: u32,
    },
    /// Reads the next iterator item and destructures it into the listed variables.
    IterNextDestructure {
        /// Iterator register.
        iterator: Register,
        /// Deconstructs the list of bind variable symbols.
        symbols: Vec<SymbolId>,
        /// The instruction index to jump to at the end of the iteration.
        jump_to_end: u32,
    },
    /// Imports object fields into the current scope.
    UseFields {
        /// Source object register.
        object: Register,
        /// Fields to import; an empty list imports every field.
        fields: Vec<SymbolId>,
    },
    /// Calls a function or method.
    Call {
        /// Destination register.
        dst: Register,
        /// Called value register.
        callee: Register,
        /// Parameter register list.
        args: Vec<Register>,
    },
    /// Unconditional jump.
    Jump {
        /// Target instruction index.
        target: u32,
    },
    /// Jumps when the condition is false.
    JumpIfFalse {
        /// Condition register.
        condition: Register,
        /// Target instruction index.
        target: u32,
    },
    /// Jump when the condition is true.
    JumpIfTrue {
        /// Condition register.
        condition: Register,
        /// Target instruction index.
        target: u32,
    },
    /// Jump when the condition is `null` or `empty`.
    JumpIfNullish {
        /// Condition register.
        condition: Register,
        /// Target instruction index.
        target: u32,
    },
    /// Enters a try/catch scope.
    EnterTry {
        /// First instruction of the catch block.
        catch_target: u32,
        /// Symbol bound to the caught error.
        error_symbol: SymbolId,
    },
    /// Leaves a try/catch scope.
    LeaveTry,
    /// Output register contents.
    Print {
        /// Source register.
        src: Register,
        /// Whether to append newlines.
        newline: bool,
    },
    /// Discards a register value, primarily for unused expression statements.
    Pop {
        /// Source register.
        src: Register,
    },
    /// Returns from the current function.
    Return {
        /// Return value register.
        src: Register,
    },
    /// Throws an error and interrupts the current flow of execution.
    Throw {
        /// Error value register.
        src: Register,
    },
    /// The program ends.
    Halt,
}
