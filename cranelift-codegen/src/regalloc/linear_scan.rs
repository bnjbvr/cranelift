//! There are two main problems in computer science: naming things, invalidating caches, and
//! off-by-one errors.

use core::cmp::Ordering;
use std::collections::HashSet;
use std::iter::FromIterator;
use std::vec::Vec;

use log::debug;

use crate::cursor::{Cursor, EncCursor};
use crate::dominator_tree::DominatorTree;
use crate::flowgraph::ControlFlowGraph;
use crate::ir::{
    AbiParam, ArgumentLoc, Ebb, Function, Inst, InstBuilder, InstructionData, Opcode, ProgramOrder,
    ProgramPoint, StackSlotKind, Type, Value, ValueLoc,
};
use crate::isa::{ConstraintKind, EncInfo, TargetIsa};
use crate::topo_order::TopoOrder;

use crate::isa::registers::{RegClass, RegUnit};
use crate::regalloc::branch_splitting;
use crate::regalloc::register_set::RegisterSet;

use crate::entity::{EntityList, ListPool, PrimaryMap, SecondaryMap};
use crate::regalloc::virtregs::VirtReg;

struct Context<'a> {
    // Set of registers that the allocator can use.
    usable_regs: RegisterSet,

    // Current instruction as well as reference to function and ISA.
    cur: EncCursor<'a>,

    // Cached ISA information.
    // We save it here to avoid frequent virtual function calls on the `TargetIsa` trait object.
    encinfo: EncInfo,

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
    value_vreg: PrimaryMap<Value, VirtReg>,
    value_pool: ListPool<Value>,

    ebb_params_vreg: PrimaryMap<Ebb, Vec<VirtReg>>,
    parallel_assignments: SecondaryMap<Ebb, Option<Vec<VirtRegCopy>>>,
}

impl VirtualRegs {
    fn new() -> Self {
        Self {
            vregs: PrimaryMap::new(),
            value_vreg: PrimaryMap::new(),
            value_pool: ListPool::new(),
            ebb_params_vreg: PrimaryMap::new(),
            parallel_assignments: SecondaryMap::new(),
        }
    }

    fn clear(&mut self) {
        self.vregs.clear();
        self.value_vreg.clear();
        self.value_pool.clear();
        self.ebb_params_vreg.clear();
        self.parallel_assignments.clear();
    }
}

// TODO use the smart bitvec
//struct BitVector {
//vec: Vec<u8>,
//}
//impl BitVector {
//fn new() -> Self {
//Self { vec: Vec::new() }
//}
//fn get(&self, i: usize) -> bool {
//self.vec.get(i / 8).map_or(false, |v| ((v >> (i % 8)) & 1) == 1)
//}
//fn set(&mut self, i: usize) {
//if self.vec.len() < (i / 8) {
//self.vec.resize(i / 8, 0);
//}
//self.vec[i / 8] |= 1 << (i % 8);
//}
//fn unset(&mut self, i: usize) {
//if self.vec.len() < (i / 8) {
//self.vec.resize(i / 8, 0);
//}
//self.vec[i / 8] &= !(1 << (i % 8));
//}
//}

// TODO use the simple and dumb bitvec
//struct BitVector {
//vec: Vec<bool>,
//}
//impl BitVector {
//fn new() -> Self {
//Self { vec: Vec::new() }
//}
//fn get(&self, i: usize) -> bool {
//*self.vec.get(i).unwrap_or(&false)
//}
//fn set(&mut self, i: usize) {
//if self.vec.len() < i {
//self.vec.resize(i, false);
//}
//self.vec[i] = true;
//}
//fn unset(&mut self, i: usize) {
//if self.vec.len() < i {
//self.vec.resize(i, false);
//}
//self.vec[i] = false;
//}
//}

trait VirtRegSet {
    fn insert(&mut self, vreg: VirtReg);
    fn contains(&self, vreg: &VirtReg) -> bool;
    fn union(&self, other: &Self) -> Self;
    fn difference(&self, other: &Self) -> Self;
}

#[derive(Default, Clone, Debug, PartialEq)]
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

impl VirtRegSet for VirtRegHashSet {
    fn insert(&mut self, vreg: VirtReg) {
        self.set.insert(vreg);
    }
    fn contains(&self, vreg: &VirtReg) -> bool {
        self.set.contains(vreg)
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

#[derive(Clone, Default)]
struct LiveInterval {
    from: Option<ProgramPoint>,
    to: Option<ProgramPoint>,
}

/// Make phis explicit: replace each block-terminating jump with params, with a parallel assignment
/// followed by the same jump without params.
///
/// Initially, generate a naive sequentialisation of the parallel assignment just by copying
/// through a fresh set of vregs.
impl<'a> Context<'a> {
    // TODO not sure this is actually needed, so putting it aside.
    fn remove_ebb_params(&mut self) {
        // Eventually, remove branch parameters for all the blocks. Do this after iterating over
        // all the blocks, to make sure we don't lose information related to branch going in either
        // direction.
        for ebb in self.cur.func.layout.ebbs() {
            let ebb_params = self
                .cur
                .func
                .dfg
                .ebb_params(ebb)
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            for ebb_param in ebb_params {
                self.cur.func.dfg.swap_remove_ebb_param(ebb_param);
            }
        }
    }

    // TODO not sure this is actually needed, keeping in case.
    fn remove_cfg_params(&mut self, inst: Inst) {
        debug_assert!(self.cur.func.dfg[inst].opcode().is_terminator());
        // Keep only branch arguments, remove passed variables.
        // TODO see if we could factor this code out in the valuelist impl, or at
        // least in the dfg.
        let dfg = &mut self.cur.func.dfg;
        let branch_args = dfg[inst]
            .take_value_list()
            .expect("branch params")
            .as_slice(&dfg.value_lists)
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let (new_branch_args, _) =
            branch_args.split_at(dfg[inst].opcode().constraints().num_fixed_value_arguments());
        let new_branch_value_list = ValueList::from_slice(new_branch_args, &mut dfg.value_lists);
        dfg[inst].put_value_list(new_branch_value_list);
    }

    fn make_phis_explicit(&mut self) {
        self.topo.reset(self.cur.func.layout.ebbs());

        let vregs = &mut self.state.vregs;
        let mut value_vreg = SecondaryMap::new();
        let mut ebb_params_vreg: SecondaryMap<Ebb, Option<Vec<VirtReg>>> = SecondaryMap::new();

        while let Some(ebb) = self.topo.next(&self.cur.func.layout, self.domtree) {
            // Step 1: assign virtual reg to the ebb parameters.
            if let Some(ebb_vregs) = &ebb_params_vreg[ebb] {
                // If it's already been visited, all the vregs must have been preallocated.
                debug_assert!(ebb_vregs.len() == self.cur.func.dfg.num_ebb_params(ebb));
            } else {
                // This block hasn't ever been visited, allocate vregs.
                let mut ebb_vregs = Vec::with_capacity(self.cur.func.dfg.num_ebb_params(ebb));
                for &ebb_param in self.cur.func.dfg.ebb_params(ebb) {
                    let vreg = vregs.vregs.push(ValueList::new());
                    vregs.vregs[vreg].push(ebb_param, &mut vregs.value_pool);
                    value_vreg[ebb_param] = Some(vreg);
                    debug!("{:?}: param {} has vreg {}", ebb, ebb_param, vreg);
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
                        value_vreg[input].is_some(),
                        "missing vreg for an inst's input"
                    );
                }

                // Assign a virtual register to every result.
                for &result in self.cur.func.dfg.inst_results(inst) {
                    let vreg = vregs.vregs.push(ValueList::new());
                    vregs.vregs[vreg].push(result, &mut vregs.value_pool);
                    debug_assert!(value_vreg[result].is_none(), "ssa value assigned twice");
                    value_vreg[result] = Some(vreg);
                    debug!("{:?}: inst result {} has vreg {}", ebb, result, vreg);
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
                        // TODO common this out in a small helper function.
                        // This block hasn't ever been visited, allocate vregs.
                        let mut ebb_vregs =
                            Vec::with_capacity(self.cur.func.dfg.num_ebb_params(target));
                        for &ebb_param in self.cur.func.dfg.ebb_params(target) {
                            let vreg = vregs.vregs.push(ValueList::new());
                            vregs.vregs[vreg].push(ebb_param, &mut vregs.value_pool);
                            value_vreg[ebb_param] = Some(vreg);
                            ebb_vregs.push(vreg);
                            debug!("{:?}: param {} has vreg {}", ebb, ebb_param, vreg);
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

                    let mut ebb_parallel_assignments = Vec::new();
                    for (&param, &ebb_vreg) in self
                        .cur
                        .func
                        .dfg
                        .inst_variable_args(inst)
                        .iter()
                        .zip(ebb_vregs.iter())
                    {
                        let source_vreg = value_vreg[param].expect("branch param has no vreg");
                        ebb_parallel_assignments.push(VirtRegCopy {
                            src: source_vreg,
                            dst: ebb_vreg,
                        });
                    }
                    vregs.parallel_assignments[ebb] = Some(ebb_parallel_assignments);
                }
            }
        }

        // De-Option-ize all the structures!
        for (value, vreg) in value_vreg.iter() {
            let pushed = vregs.value_vreg.push(vreg.unwrap());
            debug_assert!(pushed.as_u32() == value.as_u32());
        }
        for (ebb, param_vregs) in ebb_params_vreg.iter() {
            // TODO avoid the clone by implementing into_iter on SecondaryMap?
            let pushed = vregs.ebb_params_vreg.push(param_vregs.clone().unwrap());
            debug_assert!(pushed.as_u32() == ebb.as_u32());
        }
    }

    fn compute_live_intervals(&mut self, cfg: &ControlFlowGraph) {
        // For each block:
        // - live_outs (set): a (bit-)vector of vregs, set to false initially.
        // - use_before_defs (set): an empty vector of vregs.
        // - defs (set): an empty vector of vregs.

        let mut ebb_uses = SecondaryMap::new();
        let mut ebb_defs = SecondaryMap::new();

        let vregs = &self.state.vregs;

        for ebb in self.cur.func.layout.ebbs() {
            let mut uses = VirtRegHashSet::new();
            let mut defs = VirtRegHashSet::new();

            for inst in self.cur.func.layout.ebb_insts(ebb) {
                for &result in self.cur.func.dfg.inst_results(inst) {
                    defs.insert(vregs.value_vreg[result]);
                }
                for &param in self.cur.func.dfg.inst_args(inst) {
                    let param_vreg = vregs.value_vreg[param];
                    if !defs.contains(&param_vreg) {
                        uses.insert(param_vreg);
                    }
                }
            }

            if let Some(ref parallel_assignments) = vregs.parallel_assignments[ebb] {
                for assign in parallel_assignments {
                    defs.insert(assign.dst);
                }
                for assign in parallel_assignments {
                    if !defs.contains(&assign.src) {
                        uses.insert(assign.src);
                    }
                }
            }

            debug!("live_range: uses[{}] = {:?}", ebb, uses);
            debug!("live_range: defs[{}] = {:?}", ebb, defs);

            ebb_uses[ebb] = uses;
            ebb_defs[ebb] = defs;
        }

        let mut liveouts = SecondaryMap::new();

        debug!("initial liveouts");
        for ebb in self.cur.func.layout.ebbs() {
            liveouts[ebb] = VirtRegHashSet::new();
            for succ in cfg.succ_iter(ebb) {
                liveouts[ebb] = liveouts[ebb].union(&ebb_uses[succ]);
            }
            debug!("\t{}: {:?}", ebb, liveouts[ebb]);
        }

        let mut i = 1;
        let mut changed = true;
        while changed {
            changed = false;
            for ebb in self.cur.func.layout.ebbs() {
                for succ in cfg.succ_iter(ebb) {
                    // Variables live in successor blocks of the successor, that have not been
                    // redefined.
                    let live_forward = liveouts[succ].difference(&ebb_defs[succ]);
                    // unioned with the set of variables used in this successor.
                    let new_liveout = ebb_uses[succ].union(&live_forward);
                    if new_liveout != liveouts[ebb] {
                        changed = true;
                        liveouts[ebb] = new_liveout;
                    }
                }
            }
            debug!("iteration {}", i);
            for ebb in self.cur.func.layout.ebbs() {
                debug!("\t{}: {:?}", ebb, liveouts[ebb]);
            }
            i += 1;
        }

        // Computing live intervals:
        // - for each block, for each inst, find the vreg.
        // - set interval.from to the first definition point of the vreg.
        // - if vreg isn't liveout to the current block, set interval.to to the last use of this
        // vreg
        // - otherwise continue to other blocks
        let mut live_intervals: SecondaryMap<VirtReg, LiveInterval> = SecondaryMap::new();

        let layout = &self.cur.func.layout;
        let entry_block = layout.entry_block().unwrap();
        for &ebb_param in self.cur.func.dfg.ebb_params(entry_block) {
            let vreg = vregs.value_vreg[ebb_param];
            live_intervals[vreg].from = Some(entry_block.into());
        }

        // TODO Open question: is it actually necessary to compute liveouts?
        // Using LiveOuts, there's a simpler algorithm to compute live intervals by assuming bounds
        // of the live intervals start and end at BBs (instead of single instructions).
        for ebb in layout.ebbs() {
            for inst in layout.ebb_insts(ebb) {
                for &param in self.cur.func.dfg.inst_args(inst) {
                    let vreg = vregs.value_vreg[param];
                    if live_intervals[vreg]
                        .to
                        .map_or(true, |prev_to| layout.cmp(prev_to, inst) == Ordering::Less)
                    {
                        live_intervals[vreg].to = Some(inst.into());
                    }
                }

                for &result in self.cur.func.dfg.inst_results(inst) {
                    let vreg = vregs.value_vreg[result];
                    if live_intervals[vreg].from.map_or(true, |prev_from| {
                        layout.cmp(prev_from, inst) == Ordering::Greater
                    }) {
                        live_intervals[vreg].from = Some(inst.into());
                    }
                }

                // Handle parallel assignements on the last block instruction.
                if self.cur.func.dfg[inst].opcode().is_terminator() {
                    if let Some(ref parallel_assignments) = vregs.parallel_assignments[ebb] {
                        for assign in parallel_assignments {
                            // assign.dst (def, extends FROM side of the interval)
                            // := assign.src (use, extends TO side of the interval)
                            if live_intervals[assign.src]
                                .to
                                .map_or(true, |prev_to| layout.cmp(prev_to, inst) == Ordering::Less)
                            {
                                // TODO ?? should we add real copy instructions?
                                live_intervals[assign.src].to = Some(inst.into());
                            }

                            if live_intervals[assign.dst].from.map_or(true, |prev_from| {
                                layout.cmp(prev_from, inst) == Ordering::Greater
                            }) {
                                // TODO ?? should we add real copy instructions?
                                live_intervals[assign.dst].from = Some(inst.into());
                            }
                        }
                    }
                }
            }
        }

        debug!("live intervals:");
        for (vreg, live_int) in live_intervals.iter() {
            let from = live_int
                .from
                .unwrap_or_else(|| panic!("missing live interval FROM for {}", vreg));
            let to = if let Some(ref to) = live_int.to {
                debug!("\t{}: [{:?}, {:?}]", vreg, from, to);
            } else {
                debug!("\t{}: dead", vreg);
            };
        }
    }
}

pub struct LsraState {
    vregs: VirtualRegs,
}

impl LsraState {
    /// Create a new alt allocator state.
    pub fn new() -> Self {
        Self {
            vregs: VirtualRegs::new(),
        }
    }

    /// Clear the state of the allocator.
    pub fn clear(&mut self) {
        self.vregs.clear();
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
            encinfo: isa.encoding_info(),
            domtree,
            topo,
            state: self,
        };

        ctx.show("Incoming");

        branch_splitting::run(isa, ctx.cur.func, cfg, ctx.domtree, ctx.topo);
        ctx.show("After branch splitting");

        ctx.make_phis_explicit();

        ctx.compute_live_intervals(cfg);

        unimplemented!("regalloc");

        //let r = ctx.run_minimal_allocator();
        //ctx.show(limits, run_number, "Completed");

        //r
    }
}
