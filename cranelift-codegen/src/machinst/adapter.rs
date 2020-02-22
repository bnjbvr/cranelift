//! Adapter for a `MachBackend` to implement the `TargetIsa` trait.

use crate::binemit;
use crate::ir;
use crate::isa::{EncInfo, Encoding, Encodings, Legalize, RegClass, RegInfo, TargetIsa};
use crate::machinst::*;
use crate::regalloc::{RegDiversions, RegisterSet};
use crate::settings::Flags;

use std::borrow::Cow;
use std::fmt;
use target_lexicon::Triple;

/// A wrapper around a `MachBackend` that provides a `TargetIsa` impl.
pub struct TargetIsaAdapter<B: MachBackend> {
    backend: B,
    triple: Triple,
}

impl<B: MachBackend> TargetIsaAdapter<B> {
    /// Create a new `TargetIsa` wrapper around a `MachBackend`.
    pub fn new(backend: B) -> TargetIsaAdapter<B> {
        let triple = backend.triple();
        TargetIsaAdapter { backend, triple }
    }
}

impl<B: MachBackend> fmt::Display for TargetIsaAdapter<B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "MachBackend")
    }
}

impl<B: MachBackend + Send + Sync> TargetIsa for TargetIsaAdapter<B> {
    fn name(&self) -> &'static str {
        self.backend.name()
    }

    fn triple(&self) -> &Triple {
        &self.triple
    }

    fn flags(&self) -> &Flags {
        self.backend.flags()
    }

    fn register_info(&self) -> RegInfo {
        panic!("Should not be called when new-style backend is available!")
    }

    fn legal_encodings<'a>(
        &'a self,
        _func: &'a ir::Function,
        _inst: &'a ir::InstructionData,
        _ctrl_typevar: ir::Type,
    ) -> Encodings<'a> {
        panic!("Should not be called when new-style backend is available!")
    }

    fn encode(
        &self,
        _func: &ir::Function,
        _inst: &ir::InstructionData,
        _ctrl_typevar: ir::Type,
    ) -> Result<Encoding, Legalize> {
        panic!("Should not be called when new-style backend is available!")
    }

    fn encoding_info(&self) -> EncInfo {
        panic!("Should not be called when new-style backend is available!")
    }

    fn legalize_signature(&self, _sig: &mut Cow<ir::Signature>, _current: bool) {
        panic!("Should not be called when new-style backend is available!")
    }

    fn regclass_for_abi_type(&self, _ty: ir::Type) -> RegClass {
        panic!("Should not be called when new-style backend is available!")
    }

    fn allocatable_registers(&self, _func: &ir::Function) -> RegisterSet {
        panic!("Should not be called when new-style backend is available!")
    }

    fn prologue_epilogue(&self, _func: &mut ir::Function) -> CodegenResult<()> {
        panic!("Should not be called when new-style backend is available!")
    }

    #[cfg(feature = "testing_hooks")]
    fn emit_inst(
        &self,
        _func: &ir::Function,
        _inst: ir::Inst,
        _divert: &mut RegDiversions,
        _sink: &mut dyn binemit::CodeSink,
    ) {
        panic!("Should not be called when new-style backend is available!")
    }

    /// Emit a whole function into memory.
    fn emit_function_to_memory(&self, _func: &ir::Function, _sink: &mut binemit::MemoryCodeSink) {
        panic!("Should not be called when new-style backend is available!")
    }

    fn get_mach_backend(&self) -> Option<&dyn MachBackend> {
        Some(&self.backend)
    }

    fn unsigned_add_overflow_condition(&self) -> ir::condcodes::IntCC {
        self.backend.unsigned_add_overflow_condition()
    }

    fn unsigned_sub_overflow_condition(&self) -> ir::condcodes::IntCC {
        self.backend.unsigned_sub_overflow_condition()
    }
}
