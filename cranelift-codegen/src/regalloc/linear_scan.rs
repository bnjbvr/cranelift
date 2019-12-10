//! There are two main problems in computer science: naming things, invalidating caches, and
//! off-by-one errors.

use core::cmp;
use core::cmp::Ordering;
use core::fmt;
use std::collections::HashSet;
use std::iter::FromIterator;
use std::vec::Vec;

use log::debug;

use crate::cursor::EncCursor;
use crate::dominator_tree::DominatorTree;
use crate::entity::into_primary_map;
use crate::entity::{EntityList, ListPool, PrimaryMap, SecondaryMap};
use crate::flowgraph::ControlFlowGraph;
use crate::ir::{Ebb, Function, Inst, InstructionData, Layout, ProgramOrder, ProgramPoint, Value};
use crate::isa::{EncInfo, TargetIsa};
use crate::topo_order::TopoOrder;

use crate::regalloc::affinity::Affinity;
use crate::regalloc::branch_splitting;
use crate::regalloc::register_set::RegisterSet;
use crate::regalloc::virtregs::VirtReg;

struct Context<'a> {
    // Set of registers that the allocator can use.
    _usable_regs: RegisterSet,

    // Current instruction as well as reference to function and ISA.
    cur: EncCursor<'a>,

    // Cached ISA information.
    // We save it here to avoid frequent virtual function calls on the `TargetIsa` trait object.
    _encinfo: EncInfo,

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

impl ProgramLocation {
    fn new(point: impl Into<ProgramPoint>, side: Side) -> Self {
        Self {
            point: point.into(),
            side,
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

#[derive(Clone, Default)]
struct Use {}

#[derive(Hash, PartialEq, Eq, Clone, Default)]
struct LiveInterval {
    from: Option<ProgramLocation>,
    to: Option<ProgramLocation>,
}

impl LiveInterval {
    fn extends_from(&mut self, from: impl Into<ProgramPoint>, side: Side, layout: &Layout) {
        let from = from.into();
        match self.from.as_mut() {
            Some(loc) => {
                // Replace when from < loc or the input and output side differ.
                match layout.cmp(from, loc.point) {
                    Ordering::Less => {
                        *loc = ProgramLocation::new(from, side);
                    }
                    Ordering::Equal => {
                        if loc.side == Side::Output && side == Side::Input {
                            loc.side = side;
                        }
                    }
                    Ordering::Greater => {}
                }
            }
            None => self.from = Some(ProgramLocation::new(from, side)),
        };
    }

    fn extends_to(&mut self, to: impl Into<ProgramPoint>, side: Side, layout: &Layout) {
        let to = to.into();
        match self.to.as_mut() {
            Some(loc) => {
                // Replace when to > loc or to == loc but the input and output sides differ.
                match layout.cmp(to, loc.point) {
                    Ordering::Greater => {
                        *loc = ProgramLocation::new(to, side);
                    }
                    Ordering::Equal => {
                        if loc.side == Side::Input && side == Side::Output {
                            loc.side = side;
                        }
                    }
                    Ordering::Less => {}
                }
            }
            None => self.to = Some(ProgramLocation::new(to, side)),
        }
    }

    fn extend(
        &mut self,
        vreg: VirtReg,
        pp: impl Into<ProgramPoint> + Clone,
        side: Side,
        local_liveness: &mut LocalLivenessMap,
        layout: &Layout,
    ) {
        let ebb = local_liveness.ebb.unwrap();
        let last_inst = local_liveness.ebb_last_inst.unwrap();
        match local_liveness.analyze(vreg) {
            LocalLiveness::BlockLocal => {
                self.extends_from(pp.clone(), side, layout);
                self.extends_to(pp, side, layout);
            }
            LocalLiveness::LiveIn => {
                self.extends_from(ebb, Side::Input, layout);
                self.extends_to(pp, side, layout);
            }
            LocalLiveness::LiveOut => {
                self.extends_from(pp, side, layout);
                self.extends_to(last_inst, Side::Output, layout);
            }
            LocalLiveness::LiveThrough => {
                self.extends_from(ebb, Side::Input, layout);
                self.extends_to(last_inst, Side::Output, layout);
            }
        }
    }

    fn from(&self) -> &ProgramLocation {
        self.from
            .as_ref()
            .expect("no from location for a LiveInterval")
    }

    fn to(&self) -> &ProgramLocation {
        self.to
            .as_ref()
            .expect("no to locaation for a LiveInterval")
    }
}

type LiveMap = SecondaryMap<Ebb, VirtRegHashSet>;

/// Make phis explicit: replace each block-terminating jump with params, with a parallel move
/// followed by the same jump without params.
///
/// Initially, generate a naive sequentialisation of the parallel move just by copying through a
/// fresh set of vregs.
impl<'a> Context<'a> {
    fn make_phis_explicit(&mut self) {
        let vregs = &mut self.state.vregs;

        self.topo.reset(self.cur.func.layout.ebbs());

        let mut ebb_params_vreg: SecondaryMap<Ebb, Option<Vec<VirtReg>>> = SecondaryMap::new();

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

                // Assign a virtual register to every result.
                for &result in self.cur.func.dfg.inst_results(inst) {
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
        let vregs = &self.state.vregs;
        let layout = &self.cur.func.layout;

        let (liveins, liveouts) = self.solve_data_flow_equations(cfg);

        let mut live_intervals: SecondaryMap<VirtReg, LiveInterval> = SecondaryMap::new();

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
                // Parallel moves happen at the end of an EBB, so start with those.
                if let Some(ref parallel_moves) = vregs.parallel_moves[ebb] {
                    for pmove in parallel_moves {
                        // Consider that the point of definition for a parallel move is after the
                        // output of the last instruction, to reflect that all the parallel moves
                        // conflict with each other.
                        live_intervals[pmove.dst].extend(
                            pmove.dst,
                            last_inst,
                            Side::Output,
                            &mut local_liveness,
                            layout,
                        );

                        live_intervals[pmove.src].extend(
                            pmove.src,
                            last_inst,
                            Side::Output,
                            &mut local_liveness,
                            layout,
                        );
                    }
                }

                for &arg in self.cur.func.dfg.inst_args(inst) {
                    let vreg = vregs.value_vreg[arg].unwrap();
                    live_intervals[vreg].extend(
                        vreg,
                        inst,
                        Side::Input,
                        &mut local_liveness,
                        layout,
                    );
                }

                for &result in self.cur.func.dfg.inst_results(inst) {
                    let vreg = vregs.value_vreg[result].unwrap();
                    live_intervals[vreg].extend(
                        vreg,
                        inst,
                        Side::Output,
                        &mut local_liveness,
                        layout,
                    );
                }
            }
        }

        debug!("live intervals:");
        for (vreg, live_int) in live_intervals.iter() {
            let from = live_int
                .from
                .as_ref()
                .unwrap_or_else(|| panic!("missing live interval FROM for {}", vreg));
            if let Some(ref to) = live_int.to {
                debug!(
                    "\t{}: [{:?}:{:?}, {:?}:{:?}]",
                    vreg, from.point, from.side, to.point, to.side
                );
            } else {
                debug!("\t{}: dead", vreg);
            };
        }

        self.state.live_intervals = live_intervals;
    }

    fn expire_old_interval(&mut self, interval: &LiveInterval) {
        unimplemented!("expire_old_interval");
    }

    fn spill_at_interval(&mut self, interval: &LiveInterval) {
        unimplemented!("spill_at_interval");
    }

    fn allocate_registers(&mut self) {
        unimplemented!("allocate registers");

        let active: HashSet<LiveInterval> = HashSet::new();

        let mut intervals = Vec::from_iter(self.state.live_intervals.values());

        // Sort intervals by increasing start point.
        let layout = &self.cur.func.layout;
        intervals.sort_by(|&a, &b| {
            let a = a.from.as_ref().unwrap();
            let b = b.from.as_ref().unwrap();
            match layout.cmp(a.point, b.point) {
                Ordering::Less => Ordering::Less,
                Ordering::Greater => Ordering::Greater,
                Ordering::Equal => match (a.side, b.side) {
                    (Side::Input, Side::Output) => Ordering::Less,
                    (Side::Output, Side::Input) => Ordering::Greater,
                    _ => Ordering::Equal,
                },
            }
        });

        const num_registers: usize = 3; // XXX update this.
        for interval in intervals {
            self.expire_old_interval(interval);
            if active.len() == num_registers {
                self.spill_at_interval(interval);
            } else {
                // XXX continue here.
                // register[i] = a register removed from the pool of registers.
                // add i to active, sorted by increasing end point.
            }
        }

        // for each live int in sorted:
        // - expire_old_intervals(int)
        // - if len(active) == number of available registers
        //   - spill_at_interval(int)
        // -
    }

    fn resolve_moves(&mut self) {
        unimplemented!("resolve_moves");
    }
}

pub struct LsraState {
    vregs: VirtualRegs,
    live_intervals: SecondaryMap<VirtReg, LiveInterval>,
}

impl LsraState {
    /// Create a new alt allocator state.
    pub fn new() -> Self {
        Self {
            vregs: VirtualRegs::new(),
            live_intervals: SecondaryMap::new(),
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
            _usable_regs: isa.allocatable_registers(func),
            cur: EncCursor::new(func, isa),
            _encinfo: isa.encoding_info(),
            domtree,
            topo,
            state: self,
        };

        ctx.show("Incoming");

        branch_splitting::run(isa, ctx.cur.func, cfg, ctx.domtree, ctx.topo);
        ctx.show("After branch splitting");

        ctx.make_phis_explicit();

        ctx.compute_live_intervals(cfg);

        ctx.allocate_registers();

        ctx.resolve_moves();

        ctx.show("After register allocation");
    }
}
