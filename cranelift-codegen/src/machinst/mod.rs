//! This module exposes the machine-specific backend definition pieces.
//!
//! The MachInst infrastructure is the compiler backend, from CLIF
//! (ir::Function) to machine code. The purpose of this infrastructure is, at a
//! high level, to do instruction selection/lowering (to machine instructions),
//! register allocation, and then perform all the fixups to branches, constant
//! data references, etc., needed to actually generate machine code.
//!
//! The container for machine instructions, at various stages of construction,
//! is the `VCode` struct. We refer to a sequence of machine instructions organized
//! into basic blocks as "vcode". This is short for "virtual-register code", though
//! it's a bit of a misnomer because near the end of the pipeline, vcode has all
//! real registers. Nevertheless, the name is catchy and we like it.
//!
//! The compilation pipeline, from an `ir::Function` (already optimized as much as
//! you like by machine-independent optimization passes) onward, is as follows.
//! (N.B.: though we show the VCode separately at each stage, the passes
//! mutate the VCode in place; these are not separate copies of the code.)
//!
//! |    ir::Function                (SSA IR, machine-independent opcodes)
//! |        |
//! |        |  [lower]
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - mostly virtual registers.
//! |        |                        - cond branches in two-target form.
//! |        |                        - branch targets are block indices.
//! |        |                        - in-memory constants held by insns,
//! |        |                          with unknown offsets.
//! |        |                        - critical edges (actually all edges)
//! |        |                          are split.)
//! |        | [regalloc]
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - all real registers.
//! |        |                        - new instruction sequence returned
//! |        |                          out-of-band in RegAllocResult.
//! |        |                        - instruction sequence has spills,
//! |        |                          reloads, and moves inserted.
//! |        |                        - other invariants same as above.)
//! |        |
//! |        | [preamble/postamble] -- TODO
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - stack-frame size known (pass over
//! |        |                          code to see stackslot allocs +
//! |        |                          regalloc info on spillslots +
//! |        |                          regalloc info on clobbered
//! |        |                          callee-saves)
//! |        |                        - out-of-band instruction sequence
//! |        |                          has preamble prepended to entry
//! |        |                          block, and postamble appended to
//! |        |                          end of function, with all return-blocks
//! |        |                          branching to shared postamble.
//! |        |                        - all symbolic stack references to
//! |        |                          stackslots and spillslots are resolved
//! |        |                          to concrete FP-offset mem addresses.)
//! |        | [block/insn ordering]
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - vcode.final_block_order is filled in.
//! |        |                        - new insn sequence from regalloc is
//! |        |                          placed back into vcode and block
//! |        |                          boundaries are updated.)
//! |        | [redundant branch/block
//! |        |  removal]
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - all blocks that were just an
//! |        |                          unconditional branch are removed.)
//! |        |
//! |        | [branch finalization
//! |        |  (fallthroughs)]
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - all branches are in lowered one-
//! |        |                          target form, but targets are still
//! |        |                          block indices.)
//! |        |
//! |        | [branch finalization
//! |        |  (offsets)]
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - all branch offsets from start of
//! |        |                          function are known, and all branches
//! |        |                          have resolved-offset targets.)
//! |        |
//! |        | [MemArg finalization] -- TODO
//! |        |
//! |    VCode<arch_backend::Inst>   (machine instructions:
//! |        |                        - all MemArg references to the constant
//! |        |                          pool are replaced with offsets.
//! |        |                        - all constant-pool data is collected
//! |        |                          in the VCode.)
//! |        |
//! |        | [binary emission]
//! |        |
//! |    Vec<u8>                     (machine code!)
//! |

#![allow(unused_imports)]

use crate::binemit::{CodeOffset, CodeSink, MemoryCodeSink, RelocSink, StackmapSink, TrapSink};
use crate::entity::EntityRef;
use crate::entity::SecondaryMap;
use crate::ir::ValueLocations;
use crate::ir::{DataFlowGraph, Function, Inst, Opcode, Type, Value};
use crate::isa::RegUnit;
use crate::result::CodegenResult;
use crate::settings::Flags;
use crate::HashMap;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Debug;
use core::iter::Sum;
use regalloc::InstRegUses;
use regalloc::Map as RegallocMap;
use regalloc::{RealReg, RealRegUniverse, Reg, RegClass, SpillSlot, VirtualReg};
use smallvec::SmallVec;
use std::hash::Hash;

pub mod lower;
pub use lower::*;
pub mod vcode;
pub use vcode::*;
pub mod compile;
pub use compile::*;
pub mod blockorder;
pub use blockorder::*;
pub mod abi;
pub use abi::*;

/// A machine instruction.
pub trait MachInst: Clone + Debug {
    /// Return the registers referenced by this machine instruction along with
    /// the modes of reference (use, def, modify).
    fn get_regs(&self) -> InstRegUses;

    /// Map virtual registers to physical registers using the given virt->phys
    /// maps corresponding to the program points prior to, and after, this instruction.
    fn map_regs(
        &mut self,
        pre_map: &RegallocMap<VirtualReg, RealReg>,
        post_map: &RegallocMap<VirtualReg, RealReg>,
    );

    /// If this is a simple move, return the (source, destination) tuple of registers.
    fn is_move(&self) -> Option<(Reg, Reg)>;

    /// Is this a terminator (branch or ret)? If so, return its type
    /// (ret/uncond/cond) and target if applicable.
    fn is_term(&self) -> MachTerminator;

    /// Get the spill-slot size.
    fn get_spillslot_size(rc: RegClass, ty: Type) -> u32;

    /// Generate a spill.
    fn gen_spill(to_slot: SpillSlot, from_reg: RealReg, ty: Type) -> Self;

    /// Generate a reload (fill).
    fn gen_reload(to_reg: RealReg, from_slot: SpillSlot, ty: Type) -> Self;

    /// Generate a move.
    fn gen_move(to_reg: Reg, from_reg: Reg) -> Self;

    /// Possibly operate on a value directly in a spill-slot rather than a
    /// register. Useful if the machine has register-memory instruction forms
    /// (e.g., add directly from or directly to memory), like x86.
    fn maybe_direct_reload(&self, reg: VirtualReg, slot: SpillSlot) -> Option<Self>;

    /// Determine a register class to store the given CraneLift type.
    fn rc_for_type(ty: Type) -> RegClass;

    /// Generate a jump to another target. Used during lowering of
    /// control flow.
    fn gen_jump(target: BlockIndex) -> Self;

    /// Generate a NOP. The `preferred_size` parameter allows the caller to
    /// request a NOP of that size, or as close to it as possible. The machine
    /// backend may return a NOP whose binary encoding is smaller than the
    /// preferred size, but must not return a NOP that is larger. However,
    /// the instruction must have a nonzero size.
    fn gen_nop(preferred_size: usize) -> Self;

    /// Rewrite block targets using the block-target map.
    fn with_block_rewrites(&mut self, block_target_map: &[BlockIndex]);

    /// Finalize branches once the block order (fallthrough) is known.
    fn with_fallthrough_block(&mut self, fallthrough_block: Option<BlockIndex>);

    /// Update instruction once block offsets are known.  These offsets are
    /// relative to the beginning of the function. `targets` is indexed by
    /// BlockIndex.
    fn with_block_offsets(&mut self, my_offset: CodeOffset, targets: &[CodeOffset]);

    /// Get the register universe for this backend.
    fn reg_universe() -> RealRegUniverse;

    /// Align a basic block offset (from start of function).  By default, no
    /// alignment occurs.
    fn align_basic_block(offset: CodeOffset) -> CodeOffset {
        offset
    }
}

/// Describes a block terminator (not call) in the vcode, when its branches
/// have not yet been finalized (so a branch may have two targets).
#[derive(Clone, Debug)]
pub enum MachTerminator {
    /// Not a terminator.
    None,
    /// A return instruction.
    Ret,
    /// An unconditional branch to another block.
    Uncond(BlockIndex),
    /// A conditional branch to one of two other blocks.
    Cond(BlockIndex, BlockIndex),
}

/// A trait describing the ability to encode a MachInst into binary machine code.
pub trait MachInstEmit<CS: CodeSink> {
    /// Emit the instruction.
    fn emit(&self, cs: &mut CS);
}

/// Top-level machine backend trait, which wraps all monomorphized code and
/// allows a virtual call from the machine-independent `Function::compile()`.
pub trait MachBackend {
    /// Compile the given function to memory. Consumes the function.
    fn compile_function_to_memory(
        &self,
        func: Function,
        relocs: &mut dyn RelocSink,
        traps: &mut dyn TrapSink,
        stackmaps: &mut dyn StackmapSink,
    ) -> CodegenResult<Vec<u8>>;

    /// Return flags for this backend.
    fn flags(&self) -> &Flags;
}
