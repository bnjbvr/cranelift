//! There are two main problems in computer science: naming things, invalidating caches, and
//! off-by-one errors.

// MVP:
// - [ ] preserve registers around calls
// - [x] tied operands (=> fuses virtual regs)
// - [ ] fixed operands
// - [ ] spill

// high-level TODO:
// - entry block: fill incoming arguments
// - preallocate live intervals with ISA requirements
// - calls: add live intervals for physical registers, take them all at calls
//  - this might call spilling of all values live accross a call, which sounds like a bad idea.
//  - unless we have a very simple way to split live ranges
// - calls: correctly fill arguments and read return values.
// - resolve_moves:
//  - moves between blocks
//  - moves between stack and live registers...

use core::cmp::Ordering;
use core::fmt;
use std::collections::HashSet;
use std::iter::FromIterator;
use std::mem;
use std::vec::Vec;

use log::debug;

use crate::cursor::{Cursor, EncCursor};
use crate::dominator_tree::DominatorTree;
use crate::entity::into_primary_map;
use crate::entity::{EntityList, EntityRef, ListPool, PrimaryMap, SecondaryMap};
use crate::flowgraph::ControlFlowGraph;
use crate::ir::{
    AbiParam, ArgumentLoc, Ebb, Function, Inst, InstBuilder, InstructionData, Layout, Opcode,
    ProgramOrder, ProgramPoint, StackSlot, Value, ValueLoc,
};
use crate::isa::{
    ConstraintKind, EncInfo, OperandConstraint, RegClass, RegClassIndex, RegInfo, RegUnit,
    TargetIsa,
};
use crate::topo_order::TopoOrder;

use crate::regalloc::affinity::Affinity;
use crate::regalloc::branch_splitting;
use crate::regalloc::register_set::RegisterSet;
use crate::regalloc::virtregs::VirtReg;

struct Context<'a> {
    // Set of registers that the allocator can use.
    usable_regs: RegisterSet,

    // Current instruction as well as reference to function and ISA.
    cur: EncCursor<'a>,

    // Cached ISA information.
    // We save it here to avoid frequent virtual function calls on the `TargetIsa` trait object.
    enc_info: EncInfo,

    reg_info: RegInfo,

    // References to contextual data structures we need.
    domtree: &'a mut DominatorTree,
    topo: &'a mut TopoOrder,

    // The running state.
    state: &'a mut LsraState,
}

impl<'a> Context<'a> {
    fn show(&self, what: &str) {
        println!(
            "==== {} ========================================================",
            what
        );
        println!("");
        println!("{}", self.cur.func.display(self.cur.isa));
    }
}

type ValueList = EntityList<Value>;

#[derive(Clone)]
struct VirtRegCopy {
    src: VirtReg,
    dst: VirtReg,
}

struct VirtualRegs {
    /// The primary table of virtual registers.
    vregs: PrimaryMap<VirtReg, ValueList>,

    /// A mapping from value to its own virtual register. Values with None after live analysis are
    /// unused and thus dead.
    value_vreg: SecondaryMap<Value, Option<VirtReg>>,

    /// A local value pool.
    // TODO see if it can be replaced with the function's value pool.
    value_pool: ListPool<Value>,

    /// A mapping of basic block to virtual registers for all its params.
    ebb_params_vreg: PrimaryMap<Ebb, Vec<VirtReg>>,

    /// Sequence of virtual register copies appending at the end of each basic block. Note they're
    /// supposed to happen in parallel.
    parallel_moves: SecondaryMap<Ebb, Option<Vec<VirtRegCopy>>>,
}

impl VirtualRegs {
    fn new() -> Self {
        Self {
            vregs: PrimaryMap::new(),
            value_vreg: SecondaryMap::new(),
            value_pool: ListPool::new(),
            ebb_params_vreg: PrimaryMap::new(),
            parallel_moves: SecondaryMap::new(),
        }
    }

    fn clear(&mut self) {
        self.vregs.clear();
        self.value_vreg.clear();
        self.value_pool.clear();
        self.ebb_params_vreg.clear();
        self.parallel_moves.clear();
    }
}

trait VirtRegSet {
    fn insert(&mut self, vreg: VirtReg);
    fn contains(&self, vreg: VirtReg) -> bool;
    fn union(&self, other: &Self) -> Self;
    fn difference(&self, other: &Self) -> Self;
}

#[derive(Default, Clone, PartialEq)]
struct VirtRegHashSet {
    set: HashSet<VirtReg>,
}

impl VirtRegHashSet {
    fn new() -> Self {
        Self {
            set: HashSet::new(),
        }
    }
}

// Shorter implementation of debug for VirtRegHashSet, just showing the set without the structure's
// name.
impl fmt::Debug for VirtRegHashSet {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{:?}", self.set)
    }
}

impl VirtRegSet for VirtRegHashSet {
    fn insert(&mut self, vreg: VirtReg) {
        self.set.insert(vreg);
    }
    fn contains(&self, vreg: VirtReg) -> bool {
        self.set.contains(&vreg)
    }
    fn union(&self, other: &Self) -> Self {
        let set = HashSet::from_iter(self.set.union(&other.set).cloned());
        Self { set }
    }
    fn difference(&self, other: &Self) -> Self {
        let set = HashSet::from_iter(self.set.difference(&other.set).cloned());
        Self { set }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
enum Side {
    Input,
    Output,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct ProgramLocation {
    point: ProgramPoint,
    side: Side,
}

impl fmt::Debug for ProgramLocation {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "{}:{}",
            self.point,
            if self.side == Side::Input {
                "in"
            } else {
                "out"
            }
        )
    }
}

impl ProgramLocation {
    fn new(point: impl Into<ProgramPoint>, side: Side) -> Self {
        Self {
            point: point.into(),
            side,
        }
    }

    fn cmp(&self, other: &Self, layout: &Layout) -> Ordering {
        match layout.cmp(self.point, other.point) {
            Ordering::Greater => Ordering::Greater,
            Ordering::Less => Ordering::Less,
            Ordering::Equal => match (self.side, other.side) {
                (Side::Input, Side::Input) | (Side::Output, Side::Output) => Ordering::Equal,
                (Side::Input, Side::Output) => Ordering::Less,
                (Side::Output, Side::Input) => Ordering::Greater,
            },
        }
    }
}

/// Block-local liveness information.
#[derive(Clone, Copy)]
enum LocalLiveness {
    /// Live in and live out to this block.
    LiveThrough,
    /// Live in to this block, last use in this block.
    LiveIn,
    /// Defined in this block, live out to this block.
    LiveOut,
    /// Defined and last use in this block.
    BlockLocal,
}

struct LocalLivenessMap<'liveness> {
    map: SecondaryMap<VirtReg, Option<LocalLiveness>>,
    ebb: Option<Ebb>,
    ebb_last_inst: Option<Inst>,
    liveins: Option<&'liveness VirtRegHashSet>,
    liveouts: Option<&'liveness VirtRegHashSet>,
}

impl<'liveness> LocalLivenessMap<'liveness> {
    fn new() -> Self {
        Self {
            map: SecondaryMap::new(),
            ebb: None,
            ebb_last_inst: None,
            liveins: None,
            liveouts: None,
        }
    }

    fn reset<'own>(
        &'own mut self,
        ebb: Ebb,
        ebb_last_inst: Inst,
        all_liveins: &'liveness LiveMap,
        all_liveouts: &'liveness LiveMap,
    ) {
        self.map.clear();
        self.ebb = Some(ebb);
        self.ebb_last_inst = Some(ebb_last_inst);
        self.liveins = Some(&all_liveins[ebb]);
        self.liveouts = Some(&all_liveouts[ebb]);
    }

    fn analyze(&mut self, vreg: VirtReg) -> LocalLiveness {
        match &self.map[vreg] {
            Some(entry) => *entry,
            None => {
                let liveness = match (
                    self.liveins.unwrap().contains(vreg),
                    self.liveouts.unwrap().contains(vreg),
                ) {
                    // Live in and out.
                    (true, true) => LocalLiveness::LiveThrough,
                    // Live in, last use in this block.
                    (true, false) => LocalLiveness::LiveIn,
                    // Defined in this block, live out.
                    (false, true) => LocalLiveness::LiveOut,
                    // Local to this block.
                    (false, false) => LocalLiveness::BlockLocal,
                };
                self.map[vreg] = Some(liveness);
                liveness
            }
        }
    }
}

// TODO this could be merged with the concept of Affinity, probably?
#[derive(Clone, Debug, PartialEq)]
enum RequirementKind {
    Reg,
    FixedReg(RegUnit),
    Stack(Option<StackSlot>),
    None,
}

impl Default for RequirementKind {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Default)]
struct Requirement {
    kind: RequirementKind,
    rc: Option<RegClass>,
}

impl Requirement {
    fn merge_constraint(&mut self, constraint: &OperandConstraint) {
        let rc = constraint.regclass;

        let prev_rc = match self.rc {
            Some(prev_rc) => prev_rc,
            None => {
                // Easy case: assign the right requirement.
                debug_assert!(self.kind == RequirementKind::None);
                match constraint.kind {
                    ConstraintKind::Reg => self.kind = RequirementKind::Reg,
                    ConstraintKind::FixedReg(ru) | ConstraintKind::FixedTied(ru) => {
                        self.kind = RequirementKind::FixedReg(ru);
                    }
                    ConstraintKind::Stack => self.kind = RequirementKind::Stack(None),
                    ConstraintKind::Tied(_) => {
                        // Assume Reg for now, it may get adjusted later on.
                        self.kind = RequirementKind::Reg;
                    }
                }
                self.rc = Some(rc);
                return;
            }
        };

        if prev_rc != rc {
            unimplemented!(
                "conflicting reg class requirements (kind = Reg, prev = FixedReg/Reg): {:?} / {:?}",
                prev_rc,
                rc
            );
        }

        match constraint.kind {
            ConstraintKind::Reg | ConstraintKind::Tied(_) => {
                debug_assert!(self.kind == RequirementKind::Reg);
            }
            ConstraintKind::FixedReg(reg_unit) | ConstraintKind::FixedTied(reg_unit) => {
                match self.kind {
                    RequirementKind::FixedReg(prev_ru) => {
                        if prev_ru != reg_unit {
                            unimplemented!("redefining fixed register from {} to {} (kind = FixedReg/FixedTied, prev = FixedReg)", prev_ru, reg_unit);
                        }
                    }
                    RequirementKind::Reg => {}
                    RequirementKind::None => unreachable!("can't happen"),
                    RequirementKind::Stack(_) => {
                        unimplemented!("incompatible stack vs fixed-reg requirement (kind == FixedReg/FixedTied, prev = Stack)");
                    }
                }
            }
            ConstraintKind::Stack => {
                self.kind = RequirementKind::Stack(None);
            }
        }
    }

    fn force_reg(&mut self, rc: RegClass, ru: RegUnit) {
        if let Some(prev_rc) = self.rc {
            if rc != prev_rc {
                unimplemented!("different RC constraint between input ABI and vreg interval");
            }
        } else {
            assert!(self.kind == RequirementKind::None);
            self.rc = Some(rc);
            self.kind = RequirementKind::FixedReg(ru);
            return;
        };

        match self.kind {
            RequirementKind::FixedReg(prev_ru) => {
                if ru != prev_ru {
                    unimplemented!("different RU constraint between input ABI and vreg interval");
                }
            }
            RequirementKind::Reg => {
                // Adjust the constraint.
                self.kind = RequirementKind::FixedReg(ru);
            }
            RequirementKind::Stack(_) => {
                unimplemented!("effective stack constraint vs RC/RU ABI constraint");
            }
            RequirementKind::None => {
                unreachable!();
            }
        }
    }

    fn force_stack(&mut self, _offset: i32) {
        let _prev_rc = if let Some(rc) = self.rc {
            rc
        } else {
            assert!(self.kind == RequirementKind::None);
            // TODO use offset here.
            self.kind = RequirementKind::Stack(None);
            return;
        };

        match self.kind {
            RequirementKind::FixedReg(_) | RequirementKind::Reg => {
                unimplemented!("stack ABI vs register requirement");
            }
            RequirementKind::Stack(ref mut slot) => {
                assert!(slot.is_none());
                // TODO use offset here.
                unimplemented!("stack ABI vs already stack requirement");
            }
            RequirementKind::None => {
                unreachable!();
            }
        }
    }
}

impl fmt::Debug for Requirement {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self.kind {
            RequirementKind::FixedReg(ru) => {
                write!(fmt, "fixed_reg({:?}/{:?})", self.rc.unwrap(), ru)
            }
            RequirementKind::Reg => write!(fmt, "any_reg({:?})", self.rc.unwrap()),
            RequirementKind::Stack(ref slot) => write!(fmt, "stack({:?})", slot),
            RequirementKind::None => write!(fmt, "none"),
        }
    }
}

#[derive(Clone)]
struct LiveInterval {
    /// Left bound of the live interval.
    start: ProgramLocation,

    /// Right bound of the live interval.
    end: ProgramLocation,

    /// The virtual register assigned to this live interval.
    vreg: VirtReg,

    /// Preferred location for this live interval / vreg.
    // TODO should we really use this?
    affinity: Affinity,

    /// Final requirement for this class.
    requirement: Requirement,

    /// Value location, once it's assigned one.
    location: ValueLoc,
}

impl fmt::Debug for LiveInterval {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "{:?} [{:?}, {:?}] (req: {:?})",
            self.vreg, self.start, self.end, self.requirement
        )
    }
}

impl LiveInterval {
    fn new(vreg: VirtReg, point: ProgramLocation) -> Self {
        Self {
            start: point.clone(),
            end: point,
            vreg,
            affinity: Default::default(),
            requirement: Default::default(),
            location: Default::default(),
        }
    }

    fn extends_start(&mut self, start: impl Into<ProgramPoint>, side: Side, layout: &Layout) {
        let start = start.into();
        // Replace when start < self.start.point or the input and output side differ.
        match layout.cmp(start, self.start.point) {
            Ordering::Less => {
                self.start = ProgramLocation::new(start, side);
            }
            Ordering::Equal => {
                if self.start.side == Side::Output && side == Side::Input {
                    self.start.side = side;
                }
            }
            Ordering::Greater => {}
        }
    }

    fn extends_end(&mut self, end: impl Into<ProgramPoint>, side: Side, layout: &Layout) {
        let end = end.into();
        // Replace when end > self.end.point or the input and output sides differ.
        match layout.cmp(end, self.end.point) {
            Ordering::Greater => {
                self.end = ProgramLocation::new(end, side);
            }
            Ordering::Equal => {
                if self.end.side == Side::Input && side == Side::Output {
                    self.end.side = side;
                }
            }
            Ordering::Less => {}
        }
    }

    fn extend(
        &mut self,
        inst: Inst,
        side: Side,
        local_liveness: &mut LocalLivenessMap,
        layout: &Layout,
    ) {
        let ebb = local_liveness.ebb.unwrap();
        let last_inst = local_liveness.ebb_last_inst.unwrap();
        match local_liveness.analyze(self.vreg) {
            LocalLiveness::BlockLocal => {
                self.extends_start(inst.clone(), side, layout);
                self.extends_end(inst, side, layout);
            }
            LocalLiveness::LiveIn => {
                self.extends_start(ebb, Side::Input, layout);
                self.extends_end(inst, side, layout);
            }
            LocalLiveness::LiveOut => {
                self.extends_start(inst, side, layout);
                self.extends_end(last_inst, Side::Output, layout);
            }
            LocalLiveness::LiveThrough => {
                self.extends_start(ebb, Side::Input, layout);
                self.extends_end(last_inst, Side::Output, layout);
            }
        }
    }

    fn merge_constraints(&mut self, constraint: &OperandConstraint, reginfo: &RegInfo) {
        self.affinity.merge(constraint, reginfo);
        self.requirement.merge_constraint(constraint);
    }

    fn merge_abi(&mut self, abi_param: &AbiParam, reg_info: &RegInfo) {
        match abi_param.location {
            ArgumentLoc::Unassigned => panic!("ABI params should have been assigned"),
            ArgumentLoc::Reg(reg_unit) => {
                let rc_index = reg_info
                    .bank_containing_regunit(reg_unit)
                    .unwrap()
                    .first_toprc;
                let rc = reg_info.rc(RegClassIndex::new(rc_index));
                debug!(
                    "merge_abi({}): fixed register {} / {}",
                    self.vreg,
                    rc,
                    reg_info.display_regunit(reg_unit)
                );
                self.requirement.force_reg(rc, reg_unit);
            }
            ArgumentLoc::Stack(offset) => {
                debug!(
                    "merge_abi({}): stack slot with offset {}",
                    self.vreg, offset
                );
                self.requirement.force_stack(offset);
            }
        }
    }
}

/// Small helper used during the creation of live intervals.
struct LiveIntervalGroup {
    map: SecondaryMap<VirtReg, Option<LiveInterval>>,
}

impl LiveIntervalGroup {
    fn new() -> Self {
        Self {
            map: SecondaryMap::new(),
        }
    }

    fn extend(
        &mut self,
        vreg: VirtReg,
        inst: Inst,
        side: Side,
        local_liveness: &mut LocalLivenessMap,
        layout: &Layout,
    ) {
        if self.map[vreg].is_none() {
            self.map[vreg] = Some(LiveInterval::new(vreg, ProgramLocation::new(inst, side)));
        }
        self.map[vreg]
            .as_mut()
            .unwrap()
            .extend(inst, side, local_liveness, layout)
    }

    fn merge_constraints(
        &mut self,
        vreg: VirtReg,
        constraint: &OperandConstraint,
        reginfo: &RegInfo,
    ) {
        self.map[vreg]
            .as_mut()
            .unwrap()
            .merge_constraints(constraint, reginfo)
    }

    fn merge_abi(&mut self, vreg: VirtReg, abi_param: &AbiParam, reg_info: &RegInfo) {
        self.map[vreg]
            .as_mut()
            .unwrap()
            .merge_abi(abi_param, reg_info);
    }

    fn copy_constraints(&mut self, dst: VirtReg, src: VirtReg) {
        let (affinity, requirement) = {
            let src = self.map[src].as_ref().unwrap();
            (src.affinity.clone(), src.requirement.clone())
        };
        let dst = self.map[dst].as_mut().unwrap();
        dst.affinity = affinity;
        dst.requirement = requirement;
    }

    fn into_vec(self) -> Vec<LiveInterval> {
        self.map
            .into_values_vec()
            .into_iter()
            .map(|opt| opt.unwrap())
            .collect()
    }
}

type LiveMap = SecondaryMap<Ebb, VirtRegHashSet>;

impl<'a> Context<'a> {
    /// Make phis explicit: replace each block-terminating jump with params, with a parallel move
    /// followed by the same jump without params.
    ///
    /// Initially, generate a naive sequentialisation of the parallel move just by copying through
    /// a fresh set of vregs.
    fn make_phis_explicit(&mut self) {
        let vregs = &mut self.state.vregs;

        self.topo.reset(self.cur.func.layout.ebbs());

        let mut ebb_params_vreg: SecondaryMap<Ebb, Option<Vec<VirtReg>>> = SecondaryMap::new();

        let mut parallel_copy_locations = Vec::new();
        while let Some(ebb) = self.topo.next(&self.cur.func.layout, self.domtree) {
            // Step 1: assign virtual reg to the ebb parameters.
            if let Some(ref ebb_vregs) = ebb_params_vreg[ebb] {
                // If it's already been visited, all the vregs must have been preallocated.
                debug_assert!(ebb_vregs.len() == self.cur.func.dfg.num_ebb_params(ebb));
            } else {
                // This block hasn't ever been visited, allocate vregs.
                let mut ebb_vregs = Vec::with_capacity(self.cur.func.dfg.num_ebb_params(ebb));
                for &ebb_param in self.cur.func.dfg.ebb_params(ebb) {
                    let vreg = vregs.vregs.push(ValueList::new());
                    vregs.vregs[vreg].push(ebb_param, &mut vregs.value_pool);
                    vregs.value_vreg[ebb_param] = Some(vreg);
                    debug!("{:?}: {} -> {} (param)", ebb, ebb_param, vreg);
                    ebb_vregs.push(vreg);
                }
                ebb_params_vreg[ebb] = Some(ebb_vregs);
            }

            // Step 2: assign values to instructions.
            for inst in self.cur.func.layout.ebb_insts(ebb) {
                // Sanity check: every value mentioned in the instruction has been assigned a
                // virtual register.
                for &input in self.cur.func.dfg.inst_args(inst) {
                    debug_assert!(
                        vregs.value_vreg[input].is_some(),
                        "missing vreg for an inst's input"
                    );
                }

                // Assign a virtual register to every result, unless there's a constraint to reuse
                // an input.
                let encoding = &self.cur.func.encodings[inst];
                let constraints = self.enc_info.operand_constraints(*encoding);
                for (i, &result) in self.cur.func.dfg.inst_results(inst).iter().enumerate() {
                    if let Some(constraints) = constraints {
                        if i < constraints.outs.len() {
                            match constraints.outs[i].kind {
                                ConstraintKind::Tied(input_index) => {
                                    // Reuse the same vreg as the input, since it's a tied operand.
                                    let input =
                                        self.cur.func.dfg.inst_args(inst)[input_index as usize];
                                    let vreg = vregs.value_vreg[input].unwrap();
                                    vregs.vregs[vreg].push(result, &mut vregs.value_pool);
                                    vregs.value_vreg[result] = Some(vreg);
                                    debug!(
                                        "{}: {} -> {} (tied with input {})",
                                        ebb, result, vreg, input
                                    );
                                    continue;
                                }
                                _ => {}
                            }
                        }
                    };

                    let vreg = vregs.vregs.push(ValueList::new());
                    vregs.vregs[vreg].push(result, &mut vregs.value_pool);
                    debug_assert!(
                        vregs.value_vreg[result].is_none(),
                        "SSA value assigned twice"
                    );
                    vregs.value_vreg[result] = Some(vreg);
                    debug!("{:?}: {} -> {} (result)", ebb, result, vreg);
                }

                if self.cur.func.dfg[inst].opcode().is_branch() {
                    let target = match self.cur.func.dfg[inst] {
                        InstructionData::Branch { destination, .. }
                        | InstructionData::BranchIcmp { destination, .. }
                        | InstructionData::BranchInt { destination, .. }
                        | InstructionData::BranchFloat { destination, .. }
                        | InstructionData::BranchTable { destination, .. }
                        | InstructionData::Jump { destination, .. } => destination,
                        _ => panic!("Unexpected branch format in make_phis_explicit"),
                    };

                    // Make sure that the target EBBs has virtual regs.
                    if ebb_params_vreg[target].is_none() {
                        // This block hasn't ever been visited, allocate vregs.
                        let mut ebb_vregs =
                            Vec::with_capacity(self.cur.func.dfg.num_ebb_params(target));
                        for &ebb_param in self.cur.func.dfg.ebb_params(target) {
                            let vreg = vregs.vregs.push(ValueList::new());
                            vregs.vregs[vreg].push(ebb_param, &mut vregs.value_pool);
                            vregs.value_vreg[ebb_param] = Some(vreg);
                            ebb_vregs.push(vreg);
                            debug!("{:?}: {} -> {} (param)", ebb, ebb_param, vreg);
                        }
                        ebb_params_vreg[target] = Some(ebb_vregs);
                    }

                    // Introduce a parallel copy for every single EBB param.
                    let ebb_vregs = ebb_params_vreg[target].as_ref().unwrap();
                    debug_assert!(
                        self.cur.func.dfg.inst_variable_args(inst).len() == ebb_vregs.len()
                    );

                    // Because of branch splitting, only terminator instructions can have branch
                    // parameters.
                    debug_assert!(
                        self.cur.func.dfg[inst].opcode().is_terminator()
                            || self.cur.func.dfg.inst_variable_args(inst).len() == 0
                    );

                    if self.cur.func.dfg.inst_variable_args(inst).len() == 0 {
                        continue;
                    }

                    parallel_copy_locations.push(inst);

                    let mut ebb_parallel_moves = Vec::new();
                    for (&param, &ebb_vreg) in self
                        .cur
                        .func
                        .dfg
                        .inst_variable_args(inst)
                        .iter()
                        .zip(ebb_vregs.iter())
                    {
                        let source_vreg =
                            vregs.value_vreg[param].expect("branch param has no vreg");
                        ebb_parallel_moves.push(VirtRegCopy {
                            src: source_vreg,
                            dst: ebb_vreg,
                        });
                    }
                    debug_assert!(vregs.parallel_moves[ebb].is_none());
                    vregs.parallel_moves[ebb] = Some(ebb_parallel_moves);
                }
            }
        }

        for location in parallel_copy_locations {
            self.cur.goto_after_inst(location);
            self.cur.ins().regalloc_parallel_copies();
        }

        vregs.ebb_params_vreg = into_primary_map(ebb_params_vreg);
    }

    fn solve_data_flow_equations(&self, cfg: &ControlFlowGraph) -> (LiveMap, LiveMap) {
        // For each block:
        // - live_outs (set): a (bit-)vector of vregs, set to false initially.
        // - use_before_defs (set): an empty vector of vregs.
        // - defs (set): an empty vector of vregs.

        let vregs = &self.state.vregs;

        let mut ebb_uses = SecondaryMap::new();
        let mut ebb_defs = SecondaryMap::new();

        debug!("Solving data flow equations...");
        for ebb in self.cur.func.layout.ebbs() {
            let mut uses = VirtRegHashSet::new();
            let mut defs = VirtRegHashSet::new();

            for inst in self.cur.func.layout.ebb_insts(ebb) {
                for &result in self.cur.func.dfg.inst_results(inst) {
                    defs.insert(vregs.value_vreg[result].unwrap());
                }
                for &param in self.cur.func.dfg.inst_args(inst) {
                    let param_vreg = vregs.value_vreg[param].unwrap();
                    if !defs.contains(param_vreg) {
                        uses.insert(param_vreg);
                    }
                }
            }

            if let Some(ref parallel_moves) = vregs.parallel_moves[ebb] {
                for pmove in parallel_moves {
                    defs.insert(pmove.dst);
                }
                for pmove in parallel_moves {
                    if !defs.contains(pmove.src) {
                        uses.insert(pmove.src);
                    }
                }
            }

            debug!("uses[{}] = {:?}", ebb, uses);
            debug!("defs[{}] = {:?}", ebb, defs);

            ebb_uses[ebb] = uses;
            ebb_defs[ebb] = defs;
        }

        let mut liveins = LiveMap::new();
        let mut liveouts = LiveMap::new();

        debug!("initial values of liveins/liveouts");
        for ebb in self.cur.func.layout.ebbs() {
            liveins[ebb] = ebb_uses[ebb].clone();
            liveouts[ebb] = VirtRegHashSet::new();
            debug!("\t{}: in {:?}", ebb, liveins[ebb]);
        }

        let mut i = 1;
        let mut changed = true;
        while changed {
            changed = false;
            for ebb in self.cur.func.layout.ebbs() {
                let new_livein = ebb_uses[ebb].union(&liveouts[ebb].difference(&ebb_defs[ebb]));
                if new_livein != liveins[ebb] {
                    changed = true;
                    liveins[ebb] = new_livein;
                }

                let new_liveout = cfg
                    .succ_iter(ebb)
                    .fold(VirtRegHashSet::new(), |acc, succ| acc.union(&liveins[succ]));
                if new_liveout != liveouts[ebb] {
                    changed = true;
                    liveouts[ebb] = new_liveout;
                }
            }
            debug!("iteration {}", i);
            for ebb in self.cur.func.layout.ebbs() {
                debug!("\t{}: in {:?}, out {:?}", ebb, liveins[ebb], liveouts[ebb]);
            }
            i += 1;
        }

        (liveins, liveouts)
    }

    fn compute_live_intervals(&mut self, cfg: &ControlFlowGraph) {
        let (liveins, liveouts) = self.solve_data_flow_equations(cfg);

        let layout = &self.cur.func.layout;
        let vregs = &self.state.vregs;

        let mut live_intervals = LiveIntervalGroup::new();

        // A map to keep track of block-local liveness status, reset between each block.
        let mut local_liveness = LocalLivenessMap::new();

        // Go through all the blocks in post order, reading them backwards, to infer live
        // intervals.

        // TODO: revisit this. I am pretty sure this can be do in a better way:
        // - if a live interval hasn't been created, create a full range the first time, according
        // to the local analysis.
        // - if it exists, extend it with local liveness analysis first.
        // - then when visiting uses and defs (including parallel moves), only refine those
        // intervals, and skip refining when a value is through.

        for &ebb in self.domtree.cfg_postorder() {
            let last_inst = self.cur.func.layout.last_inst(ebb).unwrap();
            local_liveness.reset(ebb, last_inst, &liveins, &liveouts);

            for inst in self.cur.func.layout.ebb_insts(ebb).rev() {
                let encoding = self.cur.func.encodings[inst];
                let constraints = self.enc_info.operand_constraints(encoding);

                let is_return = match self.cur.func.dfg[inst].opcode() {
                    Opcode::Return | Opcode::FallthroughReturn => true,
                    _ => false,
                };

                for (i, &arg) in self.cur.func.dfg.inst_args(inst).iter().enumerate() {
                    let vreg = vregs.value_vreg[arg].unwrap();
                    live_intervals.extend(vreg, inst, Side::Input, &mut local_liveness, layout);
                    if let Some(constraints) = constraints {
                        if i < constraints.ins.len() {
                            live_intervals.merge_constraints(
                                vreg,
                                &constraints.ins[i],
                                &self.reg_info,
                            );
                        }
                    }

                    // Add opcode-specific constraints if required.
                    if is_return {
                        let abi_param = &self.cur.func.signature.returns[i];
                        // XXX something is wrong with this code here.
                        live_intervals.merge_abi(vreg, abi_param, &self.reg_info);
                    }
                }

                for (i, &result) in self.cur.func.dfg.inst_results(inst).iter().enumerate() {
                    let vreg = vregs.value_vreg[result].unwrap();
                    live_intervals.extend(vreg, inst, Side::Output, &mut local_liveness, layout);
                    if let Some(constraints) = constraints {
                        if i < constraints.outs.len() {
                            live_intervals.merge_constraints(
                                vreg,
                                &constraints.outs[i],
                                &self.reg_info,
                            );
                        }
                    }
                }
            }

            // Parallel moves happen at the end of an EBB, and require to have analyzed the values
            // that are coming inbound.
            if let Some(ref parallel_moves) = vregs.parallel_moves[ebb] {
                for pmove in parallel_moves {
                    // Consider that the point of definition for a parallel move is after the
                    // output of the last instruction, to reflect that all the parallel moves
                    // conflict with each other.
                    live_intervals.extend(
                        pmove.dst,
                        last_inst,
                        Side::Output,
                        &mut local_liveness,
                        layout,
                    );

                    live_intervals.extend(
                        pmove.src,
                        last_inst,
                        Side::Output,
                        &mut local_liveness,
                        layout,
                    );

                    live_intervals.copy_constraints(pmove.dst, pmove.src);
                }
            }
        }

        // Combine ABI constraints into the entry block.
        let entry_block = self.cur.func.layout.entry_block().unwrap();
        for (ebb_param, abi_param) in self
            .cur
            .func
            .dfg
            .ebb_params(entry_block)
            .iter()
            .zip(self.cur.func.signature.params.iter())
        {
            let vreg = vregs.value_vreg[*ebb_param].unwrap();
            live_intervals.merge_abi(vreg, abi_param, &self.reg_info);
        }

        self.state.live_intervals = live_intervals.into_vec();

        debug!("live intervals:");
        for (i, live_int) in self.state.live_intervals.iter().enumerate() {
            debug!("\t{} {:?}", i, live_int);
        }
        debug!("");
    }

    fn expire_old_intervals(
        &mut self,
        cur: usize,
        active: &mut Vec<usize>,
        available_registers: &mut RegisterSet,
        live_intervals: &Vec<LiveInterval>,
    ) {
        debug!("expire_old_intervals for {}", cur);

        let cur_int = &live_intervals[cur];

        let mut keep_all = false;
        active.retain(|&active_index| {
            if keep_all {
                return true;
            }

            let active_int = &live_intervals[active_index];

            // Has this active interval ended before the current interval?
            if active_int.end.cmp(&cur_int.start, &self.cur.func.layout) == Ordering::Less {
                // Yes, remove it and free the associated register.
                match active_int.location {
                    ValueLoc::Reg(reg_unit) => {
                        debug!(
                            "expire_old_intervals: freeing interval {:?} and its register {}",
                            active_int,
                            self.reg_info.display_regunit(reg_unit)
                        );
                        available_registers.free(active_int.requirement.rc.unwrap(), reg_unit);
                    }
                    _ => {}
                }
                return false;
            }

            // This is the first interval that overlaps with the current one. Since intervals are
            // ordered by start point, this means all the following intervals aren't overlapping
            // either and thus must be kept.
            keep_all = true;
            true
        });
    }

    fn spill_at_interval(
        &mut self,
        cur: usize,
        active: &mut Vec<usize>,
        intervals: &mut Vec<LiveInterval>,
    ) {
        let last_active = *active
            .last()
            .expect("spill requires at least one active interval");

        if intervals[last_active]
            .end
            .cmp(&intervals[cur].end, &self.cur.func.layout)
            == Ordering::Greater
        {
            debug!(
                "spill_at_interval: spilling furthest use (last active) and reusing its register"
            );
            match intervals[last_active].location {
                ValueLoc::Reg(reg) => {
                    // TODO this is invalid to steal a register if the constraints don't match.
                    intervals[cur].location = ValueLoc::Reg(reg);
                }
                _ => unreachable!("impossible spill from a spilled or unassigned location"),
            }

            // Find what the spill type is, and make sure all the values in the same virtual
            // register would use the same spill type.
            let spill_ty = {
                let cur_int = &intervals[cur];
                let vreg_value_list = &self.state.vregs.vregs[cur_int.vreg];
                let first_value = vreg_value_list
                    .get(0, &self.state.vregs.value_pool)
                    .expect("at least one value associated to a vreg");

                let spill_ty = self.cur.func.dfg.value_type(first_value);
                for i in 1..=vreg_value_list.len(&self.state.vregs.value_pool) {
                    let other_val = vreg_value_list
                        .get(i, &self.state.vregs.value_pool)
                        .unwrap();
                    debug_assert_eq!(spill_ty, self.cur.func.dfg.value_type(other_val));
                }
                spill_ty
            };

            intervals[last_active].location =
                ValueLoc::Stack(self.cur.func.stack_slots.make_spill_slot(spill_ty));

            active.pop();

            let cur_int = &intervals[cur];
            let index = active
                .binary_search_by(|&index| {
                    intervals[index]
                        .end
                        .cmp(&cur_int.end, &self.cur.func.layout)
                })
                .expect_err("interval should not have been active first");
            active.insert(index, cur);
        } else {
            debug!("spill_at_interval: spilling current interval");
            let spill_ty = {
                let cur_int = &intervals[cur];
                let vreg_value_list = &self.state.vregs.vregs[cur_int.vreg];
                let first_value = vreg_value_list
                    .get(0, &self.state.vregs.value_pool)
                    .expect("at least one value associated to a vreg");

                let spill_ty = self.cur.func.dfg.value_type(first_value);
                for i in 1..=vreg_value_list.len(&self.state.vregs.value_pool) {
                    let other_val = vreg_value_list
                        .get(i, &self.state.vregs.value_pool)
                        .unwrap();
                    debug_assert_eq!(spill_ty, self.cur.func.dfg.value_type(other_val));
                }
                spill_ty
            };

            intervals[cur].location =
                ValueLoc::Stack(self.cur.func.stack_slots.make_spill_slot(spill_ty));
        }
    }

    fn try_allocate_required(
        &mut self,
        i: usize,
        interval: &mut LiveInterval,
        available_registers: &mut RegisterSet,
    ) -> bool {
        let reg_class = interval
            .requirement
            .rc
            .unwrap_or_else(|| panic!("missing reg class requirement for live interval {}", i));

        match interval.requirement.kind {
            RequirementKind::Reg => {
                // Any register in this reg class will do, pick one.
                match available_registers.iter(reg_class).rnext() {
                    None => false,
                    Some(reg_unit) => {
                        debug!(
                            "try_allocate_required: using {} register (RC constraint)",
                            self.reg_info.display_regunit(reg_unit)
                        );
                        available_registers.take(reg_class, reg_unit);
                        debug_assert!(interval.location == ValueLoc::Unassigned);
                        interval.location = ValueLoc::Reg(reg_unit);
                        true
                    }
                }
            }

            RequirementKind::FixedReg(reg_unit) => {
                if available_registers.is_avail(reg_class, reg_unit) {
                    debug!(
                        "try_allocate_required: using {} register (fixed reg constraint)",
                        self.reg_info.display_regunit(reg_unit)
                    );
                    available_registers.take(reg_class, reg_unit);
                    interval.location = ValueLoc::Reg(reg_unit);
                    true
                } else {
                    false
                }
            }

            RequirementKind::Stack(_slot) => {
                unimplemented!("try_allocate_required: requires stack slot");
            }

            RequirementKind::None => unreachable!("unexpected dead interval"),
        }
    }

    fn allocate_registers(&mut self) {
        let mut intervals = Vec::new();
        mem::swap(&mut self.state.live_intervals, &mut intervals);

        // Sort intervals by increasing start point.
        intervals.sort_by(|a, b| a.start.cmp(&b.start, &self.cur.func.layout));

        // The intervals array is immutable at this point, so we can use plain indices into it to
        // reference its elements, and work around the borrow checker.
        let mut active: Vec<usize> = Vec::new();
        let mut available_registers = self.usable_regs.clone();

        for i in 0..intervals.len() {
            debug!("allocate_registers: handling interval {:?}", intervals[i]);
            self.expire_old_intervals(i, &mut active, &mut available_registers, &intervals);

            if self.try_allocate_required(i, &mut intervals[i], &mut available_registers) {
                // Add i to active, sorted by increasing end point.
                let interval = &intervals[i];
                let index = active
                    .binary_search_by(|&index| {
                        intervals[index]
                            .end
                            .cmp(&interval.end, &self.cur.func.layout)
                    })
                    .expect_err("interval should not have been active first");
                active.insert(index, i);
            } else {
                debug!("allocate_registers: spill!");
                self.spill_at_interval(i, &mut active, &mut intervals);
            }
        }

        debug!("allocate_registers: final results:");
        for (i, interval) in intervals.iter().enumerate() {
            debug!(
                "\t{} {:?} -> {}",
                i,
                interval.vreg,
                interval.location.display(&self.reg_info)
            );
        }

        self.state.live_intervals = intervals;
    }

    fn resolve_moves(&mut self) {
        unimplemented!("resolve_moves");
        // TODO: cleanup moves
    }
}

pub struct LsraState {
    vregs: VirtualRegs,
    live_intervals: Vec<LiveInterval>,
}

impl LsraState {
    /// Create a new alt allocator state.
    pub fn new() -> Self {
        Self {
            vregs: VirtualRegs::new(),
            live_intervals: Vec::new(),
        }
    }

    /// Clear the state of the allocator.
    pub fn clear(&mut self) {
        self.vregs.clear();
        self.live_intervals.clear();
    }

    /// Run register allocation.
    pub fn run(
        &mut self,
        isa: &dyn TargetIsa,
        func: &mut Function,
        cfg: &mut ControlFlowGraph,
        domtree: &mut DominatorTree,
        topo: &mut TopoOrder,
    ) {
        let mut ctx = Context {
            usable_regs: isa.allocatable_registers(func),
            cur: EncCursor::new(func, isa),
            enc_info: isa.encoding_info(),
            reg_info: isa.register_info(),
            domtree,
            topo,
            state: self,
        };

        ctx.show("Incoming");

        branch_splitting::run(isa, ctx.cur.func, cfg, ctx.domtree, ctx.topo);
        ctx.show("After branch splitting");

        ctx.make_phis_explicit();
        ctx.show("After making phis explicit");

        ctx.compute_live_intervals(cfg);

        ctx.allocate_registers();

        ctx.resolve_moves();

        ctx.show("After register allocation");
    }
}
