//! Emitting binary ARM64 machine code.

use binemit::{CodeSink, bad_encoding};
use ir::{Function, Inst, InstructionData};
use isa::{RegUnit, StackRef, StackBaseMask};
use regalloc::RegDiversions;

include!(concat!(env!("OUT_DIR"), "/binemit-arm64.rs"));

pub static RELOC_NAMES: [&'static str; 1] = ["Call"];

/// Move instruction.
///
///   31   22  20    4
///   bits hw  imm16 Rd
///     23  21     5  0
fn put_move<CS: CodeSink + ?Sized>(bits: u16, imm: i64, rd: RegUnit, sink: &mut CS) {
    let rd = u32::from(rd) & 0x1f;

    let mut i: u32 = rd;
    assert!(imm <= 0xffff);
    i |= (imm as u32) << 5;

    // TODO shifts are not handled here... yet.
    let shift = 0;
    assert!(shift <= 48 && shift % 16 == 0);
    let hw = shift / 16;

    i |= hw << 21;
    i |= (bits as u32) << 23;

    sink.put4(i);
}
