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
use crate::entity::{EntityList, ListPool, PrimaryMap, SecondaryMap};
use crate::flowgraph::ControlFlowGraph;
use crate::ir::{
    Ebb, Function, Inst, InstBuilder, InstructionData, Layout, ProgramOrder, ProgramPoint, Value,
    ValueLoc,
};
use crate::isa::{ConstraintKind, EncInfo, OperandConstraint, RegClass, RegInfo, TargetIsa};
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
    fn union(&self, other: &VirtRegHashSet) -> VirtRegHashSet {
        let set = HashSet::from_iter(self.set.union(&other.set).cloned());
        VirtRegHashSet { set }
    }
    fn difference(&self, other: &VirtRegHashSet) -> VirtRegHashSet {
        let set = HashSet::from_iter(self.set.difference(&other.set).cloned());
        VirtRegHashSet { set }
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

    fn cmp(&self, other: &ProgramLocation, layout: &Layout) -> Ordering {
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

#[derive(Clone)]
struct LiveInterval {
    /// Left bound of the live interval.
    from: ProgramLocation,

    /// Right bound of the live interval.
    to: ProgramLocation,

    /// The virtual register related to this live interval.
    vreg: VirtReg,

    /// Preferred location for this live interval / vreg.
    // TODO should we really use this?
    affinity: Affinity,

    /// Value location, once it's assigned one.
    location: ValueLoc,

    /// Preferred register class for this live interval / vreg.
    reg_class: Option<RegClass>,
}

impl fmt::Debug for LiveInterval {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "{:?} [{:?}, {:?}] / rc {:?}",
            self.vreg, self.from, self.to, self.reg_class
        )
    }
}

impl LiveInterval {
    fn new(vreg: VirtReg, point: ProgramLocation) -> Self {
        Self {
            from: point.clone(),
            to: point,
            vreg,
            affinity: Default::default(),
            location: Default::default(),
            reg_class: None,
        }
    }

    fn extends_from(&mut self, from: impl Into<ProgramPoint>, side: Side, layout: &Layout) {
        let from = from.into();
        // Replace when from < self.from.point or the input and output side differ.
        match layout.cmp(from, self.from.point) {
            Ordering::Less => {
                self.from = ProgramLocation::new(from, side);
            }
            Ordering::Equal => {
                if self.from.side == Side::Output && side == Side::Input {
                    self.from.side = side;
                }
            }
            Ordering::Greater => {}
        }
    }

    fn extends_to(&mut self, to: impl Into<ProgramPoint>, side: Side, layout: &Layout) {
        let to = to.into();
        // Replace when to > self.to.point or the input and output sides differ.
        match layout.cmp(to, self.to.point) {
            Ordering::Greater => {
                self.to = ProgramLocation::new(to, side);
            }
            Ordering::Equal => {
                if self.to.side == Side::Input && side == Side::Output {
                    self.to.side = side;
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
                self.extends_from(inst.clone(), side, layout);
                self.extends_to(inst, side, layout);
            }
            LocalLiveness::LiveIn => {
                self.extends_from(ebb, Side::Input, layout);
                self.extends_to(inst, side, layout);
            }
            LocalLiveness::LiveOut => {
                self.extends_from(inst, side, layout);
                self.extends_to(last_inst, Side::Output, layout);
            }
            LocalLiveness::LiveThrough => {
                self.extends_from(ebb, Side::Input, layout);
                self.extends_to(last_inst, Side::Output, layout);
            }
        }
    }

    fn merge_constraints(&mut self, constraint: &OperandConstraint, reginfo: &RegInfo) {
        let rc = constraint.regclass;
        match self.reg_class {
            Some(prev_rc) => debug_assert_eq!(rc, prev_rc),
            None => {
                self.reg_class = Some(rc);
            }
        }
        self.affinity.merge(constraint, reginfo);
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

    fn copy_constraints(&mut self, dst: VirtReg, src: VirtReg) {
        let (affinity, reg_class) = {
            let src = self.map[src].as_ref().unwrap();
            (src.affinity.clone(), src.reg_class)
        };
        let dst = self.map[dst].as_mut().unwrap();
        dst.affinity = affinity;
        dst.reg_class = reg_class;
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

        self.state.live_intervals = live_intervals.into_vec();

        debug!("live intervals:");
        for (i, live_int) in self.state.live_intervals.iter().enumerate() {
            debug!(
                "\t{} {}: [{:?}:{:?}, {:?}:{:?}]",
                i,
                live_int.vreg,
                live_int.from.point,
                live_int.from.side,
                live_int.to.point,
                live_int.to.side
            );
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
            if active_int.to.cmp(&cur_int.from, &self.cur.func.layout) == Ordering::Less {
                // Yes, remove it and free the associated register.
                match active_int.location {
                    ValueLoc::Reg(reg_unit) => {
                        debug!(
                            "expire_old_intervals: freeing interval {:?} and its register {}",
                            active_int,
                            self.reg_info.display_regunit(reg_unit)
                        );
                        available_registers.free(active_int.reg_class.unwrap(), reg_unit);
                    }
                    _ => {}
                }
                return false;
            }

            // This is the first active interval which is overlapping with the current one.
            // Since intervals are oredered by start point, this means all the next intervals
            // must be kept.
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
            .to
            .cmp(&intervals[cur].to, &self.cur.func.layout)
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
                    intervals[index].to.cmp(&cur_int.to, &self.cur.func.layout)
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

    fn allocate_registers(&mut self) {
        let mut intervals = Vec::new();
        mem::swap(&mut self.state.live_intervals, &mut intervals);

        // Sort intervals by increasing start point.
        intervals.sort_by(|a, b| a.from.cmp(&b.from, &self.cur.func.layout));

        // The intervals array is immutable at this point, so we can use plain indices into it to
        // reference its elements, and work around the borrow checker.
        let mut active: Vec<usize> = Vec::new();
        let mut available_registers = self.usable_regs.clone();

        for i in 0..intervals.len() {
            debug!("allocate_registers: handling interval {:?}", intervals[i]);
            self.expire_old_intervals(i, &mut active, &mut available_registers, &intervals);

            let reg_class = if let Some(reg_class) = intervals[i].reg_class {
                reg_class
            } else {
                debug!("allocate_registers: dead interval {:?}", i);
                continue;
            };

            match available_registers.iter(reg_class).next() {
                None => {
                    debug!("allocate_registers: spill!");
                    self.spill_at_interval(i, &mut active, &mut intervals);
                }
                Some(reg_unit) => {
                    // Assign register as taken.
                    debug!(
                        "allocate_registers: using {} register",
                        self.reg_info.display_regunit(reg_unit)
                    );

                    available_registers.take(reg_class, reg_unit);
                    debug_assert!(intervals[i].location == ValueLoc::Unassigned);
                    intervals[i].location = ValueLoc::Reg(reg_unit);

                    // Add i to active, sorted by increasing end point.
                    let interval = &intervals[i];
                    let index = active
                        .binary_search_by(|&index| {
                            intervals[index].to.cmp(&interval.to, &self.cur.func.layout)
                        })
                        .expect_err("interval should not have been active first");
                    active.insert(index, i);
                }
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
