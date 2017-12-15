"""
ARM64 Encoding recipes.

The encoding recipes defined here correspond to the ARM64 native instruction
formats described in the reference.
"""
from __future__ import absolute_import
from cdsl.isa import EncRecipe
from cdsl.predicates import IsSignedInt
from cdsl.registers import Stack
from base.formats import Binary, BinaryImm, MultiAry, IntCompare, IntCompareImm
from base.formats import Unary, UnaryImm, BranchIcmp, Branch, Jump
from base.formats import Call, IndirectCall, RegMove
from .registers import GPR, FPR, FLAG

def dataProcessing(op0):
    assert op0 <= 0b111
    return (0b100 << 26) | (op0 << 23)

def moveWide(is64bits, opc):
    assert is64bits <= 0b1
    assert opc <= 0b11
    return ((is64bits << 31) | (opc << 29) | dataProcessing(0b101)) >> 23

def MOVN(is64bits):
    return moveWide(is64bits, 0b00)
def MOVZ(is64bits):
    return moveWide(is64bits, 0b10)
def MOVK(is64bits):
    return moveWide(is64bits, 0b11)

Move16 = EncRecipe(
        'Move16', UnaryImm, size=4, ins=(), outs=GPR,
        # TODO the predicate could be more precise: has <= 16 adjacent non-zero bits
        instp=IsSignedInt(UnaryImm.imm, 16),
        emit='put_move(bits, imm.into(), out_reg0, sink);')
