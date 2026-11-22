//! libseccomp's rule tree for one call, ported so that the call's rules are
//! put in the order, and refused where, libseccomp decides.
//!
//! libseccomp (src/db.c, `db_rule_add`; 2.5.5 and 2.6.0 are the same here)
//! does not keep a call's conditional rules as a list. `_db_rule_gen_64`
//! turns each rule into a chain of nodes, one level per condition and one
//! node per 32-bit half, high half first; a comparison that needs it is split
//! into a greater-than node and an equal node. `_db_tree_prune` and
//! `_db_tree_add` put the chain into the call's tree, where the nodes of a
//! level stay in the order `_db_chain_lt` gives and a node equal to one that
//! is there (`_db_chain_eq`, which looks at the half, the node's operator,
//! datum and mask, not at the rule) is shared. Where one rule is the start of
//! another they decide by profile order: the longer rule after the shorter is
//! dropped, the longer before the shorter is refused (EEXIST), a shorter rule
//! with the same action takes the longer one's place. The filter then tests
//! each level's nodes in order (`_gen_bpf_chain`): a node that decides jumps
//! to its action or to the next level, anything else falls through to the
//! next node of its level, and a level that fails to the next node of the
//! level above.
//!
//! This is a line-for-line port of those functions, with indexes into one
//! arena in place of pointers and reference counts, so that the same rules
//! give the same tree. [`Db::order`] then lists, in the order the filter
//! would test them, the places in the tree where a rule's action stands: a
//! rule's action stands at one node (equality, masked equality), or at two
//! (greater-than, less-than, not-equal: one for the high half alone, one for
//! the low half once the high halves are equal). The caller tests exactly
//! the rule there, so what libseccomp's tree can get wrong (nodes shared
//! between two parents) is not copied: the answer is always the action of a
//! rule that holds.

use std::cell::Cell;

use super::{Cond, Op, JEQ_K, JGE_K, JGT_K, OFF_ARGS};

/// Which part of a rule's last condition an action stands for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Leaf {
    /// The whole condition (equal, masked equal).
    Whole,
    /// The high half alone decides it: greater than, less than, not equal.
    Hi,
    /// The high halves are equal and the low half decides it.
    Lo,
}

/// A rule, by its position in the call's rules, and a part of its last
/// condition.
pub type Who = (usize, Leaf);
const NOBODY: Who = (usize::MAX, Leaf::Whole);

const MATCH: u32 = 0x1;
const MATCH_ONCE: u32 = 0x2;
const X_FINISHED: u32 = 0x10;
const N_FINISHED: u32 = 0x20;
const X_PREFIX: u32 = 0x100;
const N_PREFIX: u32 = 0x200;
const MATCHSET: u32 = MATCH | MATCH_ONCE;
const REDUNDANT: u32 = MATCH | X_FINISHED | N_PREFIX;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Eq,
    Gt,
    Ge,
    MaskedEq,
}

struct Node {
    arg: u32,
    high: bool,
    op: Cmp,
    orig: Op,
    mask: u32,
    datum: u32,
    full: u64,
    act_t_flg: bool,
    act_f_flg: bool,
    act_t: u32,
    act_f: u32,
    /// The rule and the part of its last condition that each action is for.
    who_t: Who,
    who_f: Who,
    prv: Option<usize>,
    nxt: Option<usize>,
    t: Option<usize>,
    f: Option<usize>,
    refs: i32,
}

impl Node {
    fn new() -> Node {
        Node { arg: 0, high: false, op: Cmp::Eq, orig: Op::Eq, mask: 0, datum: 0, full: 0, act_t_flg: false, act_f_flg: false, act_t: 0, act_f: 0, who_t: NOBODY, who_f: NOBODY, prv: None, nxt: None, t: None, f: None, refs: 0 }
    }
}

#[derive(Clone, Copy)]
struct State {
    flags: u32,
    action: u32,
}

#[derive(Clone, Copy)]
enum Slot {
    Chains,
    T(usize),
    F(usize),
}

/// A new rule's chain, before it is added.
struct Fresh {
    chains: Option<usize>,
}

/// Steps one rule may take through the tree. libseccomp can corrupt its own
/// tree for some combinations of rules and then loops for ever (Docker's
/// container start hangs on them); here the step budget runs out and the
/// profile is refused instead.
const FUEL: u64 = 2_000_000;

/// One call's rule tree.
#[derive(Default)]
pub struct Db {
    fuel: Cell<u64>,
    spent: Cell<bool>,
    /// Steps `order` may still take, for the whole profile.
    pool: Cell<u64>,
    nodes: Vec<Node>,
    /// The tree (`state->sx->chains`).
    chains: Option<usize>,
    /// Whether a rule has been added.
    started: bool,
}

fn arg_priority(n: &Node) -> u32 {
    (n.arg << 1) + u32::from(n.high)
}

fn op_priority(op: Op) -> u32 {
    match op {
        Op::MaskedEq | Op::Eq | Op::Ne => 3,
        Op::Le | Op::Lt => 2,
        Op::Ge | Op::Gt => 1,
    }
}

impl Db {
    /// One step of a walk along a level or through the tree; false once the
    /// budget is gone.
    fn tick(&self) -> bool {
        let f = self.fuel.get();
        if f == 0 {
            self.spent.set(true);
            return false;
        }
        self.fuel.set(f - 1);
        true
    }

    fn get(&mut self, n: Option<usize>) -> Option<usize> {
        if let Some(i) = n {
            self.nodes[i].refs += 1;
        }
        n
    }

    fn slot(&self, s: Slot) -> Option<usize> {
        match s {
            Slot::Chains => self.chains,
            Slot::T(i) => self.nodes[i].t,
            Slot::F(i) => self.nodes[i].f,
        }
    }

    fn set(&mut self, s: Slot, v: Option<usize>) {
        match s {
            Slot::Chains => self.chains = v,
            Slot::T(i) => self.nodes[i].t = v,
            Slot::F(i) => self.nodes[i].f = v,
        }
    }

    /// `_db_chain_lt`
    fn lt(&self, a: usize, b: usize) -> bool {
        let (a, b) = (&self.nodes[a], &self.nodes[b]);
        let (pa, pb) = (arg_priority(a), arg_priority(b));
        if pa != pb {
            return pa < pb;
        }
        let (oa, ob) = (op_priority(a.orig), op_priority(b.orig));
        if oa != ob {
            return oa < ob;
        }
        match a.orig {
            Op::Le | Op::Lt => a.datum > b.datum,
            _ => a.datum < b.datum,
        }
    }

    /// `_db_chain_eq`
    fn eq(&self, a: usize, b: usize) -> bool {
        let (a, b) = (&self.nodes[a], &self.nodes[b]);
        arg_priority(a) == arg_priority(b) && a.op == b.op && a.datum == b.datum && a.mask == b.mask
    }

    fn leaf(&self, n: usize) -> bool {
        self.nodes[n].t.is_none() && self.nodes[n].f.is_none()
    }

    fn zombie(&self, n: usize) -> bool {
        self.leaf(n) && !self.nodes[n].act_t_flg && !self.nodes[n].act_f_flg
    }

    /// `_db_level_clean`
    fn level_clean(&mut self, node: usize) -> u32 {
        let mut n = node;
        while let Some(p) = self.nodes[n].prv.filter(|_| self.tick()) {
            n = p;
        }
        let start = n;
        let mut cur = Some(n);
        while let Some(i) = cur.filter(|_| self.tick()) {
            let links = u32::from(self.nodes[i].prv.is_some()) + u32::from(self.nodes[i].nxt.is_some());
            if self.nodes[i].refs > links as i32 {
                return 0;
            }
            cur = self.nodes[i].nxt;
        }
        let mut cnt = 0;
        let mut cur = Some(start);
        while cur.is_some() && self.tick() {
            cnt += self.put(&mut cur);
        }
        cnt
    }

    /// `_db_tree_put`
    fn tree_put(&mut self, tree: &mut Option<usize>) -> u32 {
        let mut cnt = self.put(tree);
        if let Some(t) = *tree {
            cnt += self.level_clean(t);
        }
        cnt
    }

    /// `_db_node_put`
    fn put(&mut self, node: &mut Option<usize>) -> u32 {
        let Some(n) = *node else { return 0 };
        self.nodes[n].refs -= 1;
        if self.nodes[n].refs < 0 {
            // Released more often than taken: the tree is corrupt. Refuse the
            // profile (`add` fails once `spent` is set) rather than abort.
            self.spent.set(true);
            self.nodes[n].refs = 0;
            return 0;
        }
        if self.nodes[n].refs != 0 {
            return 0;
        }
        let mut cnt = 0;
        let (mut lvl_p, mut lvl_n, mut nxt_t, mut nxt_f) = (self.nodes[n].prv, self.nodes[n].nxt, self.nodes[n].t, self.nodes[n].f);
        if let Some(p) = lvl_p {
            self.nodes[p].nxt = None;
        }
        if let Some(x) = lvl_n {
            self.nodes[x].prv = None;
        }
        if lvl_p.is_some() {
            cnt += self.put(&mut lvl_p);
        }
        if lvl_n.is_some() {
            cnt += self.put(&mut lvl_n);
        }
        if let Some(p) = lvl_p {
            let v = self.get(lvl_n);
            self.nodes[p].nxt = v;
        }
        if let Some(x) = lvl_n {
            let v = self.get(lvl_p);
            self.nodes[x].prv = v;
        }
        *node = lvl_p.or(lvl_n);
        cnt += self.tree_put(&mut nxt_t);
        cnt += self.tree_put(&mut nxt_f);
        cnt + 1
    }

    /// `_db_tree_remove`
    fn tree_remove(&mut self, tree: Slot, node: usize) -> u32 {
        let Some(mut c) = self.slot(tree) else { return 0 };
        while let Some(p) = self.nodes[c].prv.filter(|_| self.tick()) {
            c = p;
        }
        let mut cnt = 0;
        loop {
            if !self.tick() {
                return cnt;
            }
            if c == node {
                break;
            }
            cnt += self.tree_remove(Slot::T(c), node);
            cnt += self.tree_remove(Slot::F(c), node);
            if self.zombie(c) {
                break;
            }
            match self.nodes[c].nxt {
                Some(n) if cnt == 0 => c = n,
                _ => return cnt,
            }
        }
        if self.slot(tree) == Some(c) {
            let v = self.nodes[c].prv.or(self.nodes[c].nxt);
            self.set(tree, v);
        }
        let (p, n) = (self.nodes[c].prv, self.nodes[c].nxt);
        if let Some(p) = p {
            self.nodes[p].nxt = n;
        }
        if let Some(n) = n {
            self.nodes[n].prv = p;
        }
        self.nodes[c].prv = None;
        self.nodes[c].nxt = None;
        let mut cc = Some(c);
        cnt + self.put(&mut cc)
    }

    /// `_db_tree_act_check`: Err is EEXIST.
    fn act_check(&self, tree: Option<usize>, action: u32) -> Result<(), ()> {
        let Some(mut c) = tree else { return Ok(()) };
        while let Some(p) = self.nodes[c].prv.filter(|_| self.tick()) {
            c = p;
        }
        let mut cur = Some(c);
        while let Some(i) = cur.filter(|_| self.tick()) {
            let n = &self.nodes[i];
            if n.act_t_flg && n.act_t != action || n.act_f_flg && n.act_f != action {
                return Err(());
            }
            self.act_check(n.t, action)?;
            self.act_check(n.f, action)?;
            cur = n.nxt;
        }
        Ok(())
    }

    /// `_db_tree_prune`; the tree being pruned is `self.chains`.
    fn prune(&mut self, existing: Option<usize>, new: Option<usize>, state: &mut State) -> u32 {
        let mut cnt = 0;
        let mut state_new = *state;
        'ret: {
            let (Some(first), Some(n)) = (existing, new) else { break 'ret };
            if state.flags & MATCHSET == MATCH_ONCE {
                break 'ret;
            }
            let mut start = first;
            while let Some(p) = self.nodes[start].prv.filter(|_| self.tick()) {
                start = p;
            }
            let mut x_iter = Some(start);
            loop {
                if !self.tick() {
                    break 'ret;
                }
                let Some(x) = x_iter else { break 'ret };
                let x_next = self.nodes[x].nxt;
                'next: {
                    if self.eq(x, n) {
                        state_new.flags |= MATCHSET;
                        if self.leaf(n) {
                            state_new.flags |= N_FINISHED;
                        }
                        if self.leaf(x) {
                            state_new.flags |= X_FINISHED;
                        }
                        {
                            let (xn, nn) = (&self.nodes[x], &self.nodes[n]);
                            if (xn.act_t_flg || xn.t.is_some()) && !(nn.act_t_flg || nn.t.is_some()) {
                                break 'ret;
                            }
                            if (xn.act_f_flg || xn.f.is_some()) && !(nn.act_f_flg || nn.f.is_some()) {
                                break 'ret;
                            }
                            if state_new.flags & N_FINISHED != 0 && state_new.flags & X_FINISHED != 0 && (nn.act_t_flg != xn.act_t_flg || nn.act_t != xn.act_t || nn.act_f_flg != xn.act_f_flg || nn.act_f != xn.act_f) {
                                break 'ret;
                            }
                        }
                        if let Some(nt) = self.nodes[n].t {
                            let Some(xi) = x_iter else { break 'ret };
                            self.get(Some(xi));
                            let mut st = *state;
                            st.flags |= MATCHSET;
                            let sub = self.nodes[xi].t;
                            cnt += self.prune(sub, Some(nt), &mut st);
                            cnt += self.put(&mut x_iter);
                            if st.flags & MATCH != 0 {
                                state_new.flags |= st.flags;
                            }
                            if x_iter.is_none() {
                                break 'next;
                            }
                        }
                        if let Some(nf) = self.nodes[n].f {
                            let Some(xi) = x_iter else { break 'ret };
                            self.get(Some(xi));
                            let mut st = *state;
                            st.flags |= MATCHSET;
                            let sub = self.nodes[xi].f;
                            cnt += self.prune(sub, Some(nf), &mut st);
                            cnt += self.put(&mut x_iter);
                            if st.flags & MATCH != 0 {
                                state_new.flags |= st.flags;
                            }
                            if x_iter.is_none() {
                                break 'next;
                            }
                        }
                        let ok = self.act_check(x_iter, state_new.action).is_ok();
                        if ok && state_new.flags & MATCH != 0 && state_new.flags & N_FINISHED != 0 && state_new.flags & X_PREFIX != 0 {
                            let Some(xr) = x_iter else { break 'ret };
                            cnt += self.tree_remove(Slot::Chains, xr);
                            if self.chains.is_none() {
                                break 'ret;
                            }
                        } else if ok && state_new.flags & MATCH != 0 && state_new.flags & X_FINISHED != 0 && state_new.flags & N_PREFIX != 0 {
                            break 'ret;
                        }
                    } else if self.lt(x, n) {
                        if state.flags & N_PREFIX != 0 {
                            break 'ret;
                        }
                        for tf in [true, false] {
                            let Some(xi) = x_iter else { break };
                            let sub = if tf { self.nodes[xi].t } else { self.nodes[xi].f };
                            if sub.is_none() {
                                continue;
                            }
                            self.get(Some(xi));
                            let mut st = *state;
                            st.flags &= !MATCH;
                            st.flags |= X_PREFIX;
                            cnt += self.prune(sub, Some(n), &mut st);
                            cnt += self.put(&mut x_iter);
                            if st.flags & MATCH != 0 {
                                state_new.flags |= st.flags;
                                break 'ret;
                            }
                            if x_iter.is_none() {
                                break 'next;
                            }
                        }
                    } else {
                        if state.flags & X_PREFIX != 0 {
                            break 'ret;
                        }
                        for tf in [true, false] {
                            let sub = if tf { self.nodes[n].t } else { self.nodes[n].f };
                            if sub.is_none() {
                                continue;
                            }
                            let Some(xi) = x_iter else { break 'ret };
                            self.get(Some(xi));
                            let mut st = *state;
                            st.flags &= !MATCH;
                            st.flags |= N_PREFIX;
                            cnt += self.prune(Some(xi), sub, &mut st);
                            cnt += self.put(&mut x_iter);
                            if st.flags & MATCH != 0 {
                                state_new.flags |= st.flags;
                                break 'ret;
                            }
                            if x_iter.is_none() {
                                break 'next;
                            }
                        }
                    }
                }
                x_iter = x_next;
                if x_iter.is_none() {
                    break;
                }
            }
            state_new.flags &= !MATCH;
        }
        if state_new.flags & MATCH != 0 {
            state.flags |= state_new.flags;
        } else {
            state.flags &= !MATCH;
        }
        cnt
    }

    /// `_db_tree_add`: Err is EEXIST.
    fn tree_add(&mut self, existing: Slot, new: usize) -> Result<(), ()> {
        let mut x_iter = self.slot(existing);
        let n = new;
        while let Some(x) = x_iter.filter(|_| self.tick()) {
            if self.eq(x, n) {
                if self.nodes[n].act_t_flg {
                    if !self.nodes[x].act_t_flg {
                        self.act_check(self.nodes[x].t, self.nodes[n].act_t)?;
                        let mut t = self.nodes[x].t;
                        self.put(&mut t);
                        self.nodes[x].t = None;
                        self.nodes[x].act_t = self.nodes[n].act_t;
                        self.nodes[x].who_t = self.nodes[n].who_t;
                        self.nodes[x].act_t_flg = true;
                    } else if self.nodes[n].act_t != self.nodes[x].act_t {
                        if self.nodes[n].high && self.nodes[n].full > self.nodes[x].full {
                            self.nodes[x].act_t = self.nodes[n].act_t;
                            self.nodes[x].who_t = self.nodes[n].who_t;
                        }
                        if self.leaf(x) || self.leaf(n) {
                            return Err(());
                        }
                    }
                }
                if self.nodes[n].act_f_flg {
                    if !self.nodes[x].act_f_flg {
                        self.act_check(self.nodes[x].f, self.nodes[n].act_f)?;
                        let mut f = self.nodes[x].f;
                        self.put(&mut f);
                        self.nodes[x].f = None;
                        self.nodes[x].act_f = self.nodes[n].act_f;
                        self.nodes[x].who_f = self.nodes[n].who_f;
                        self.nodes[x].act_f_flg = true;
                    } else if self.nodes[n].act_f != self.nodes[x].act_f {
                        // libseccomp writes the true action here.
                        if self.nodes[n].high && self.nodes[n].full < self.nodes[x].full {
                            self.nodes[x].act_t = self.nodes[n].act_t;
                            self.nodes[x].who_t = self.nodes[n].who_t;
                        }
                        if self.leaf(x) || self.leaf(n) {
                            return Err(());
                        }
                    }
                }
                if let Some(nt) = self.nodes[n].t {
                    if self.nodes[x].t.is_some() {
                        self.tree_add(Slot::T(x), nt)?;
                    } else if !self.nodes[x].act_t_flg {
                        let v = self.get(Some(nt));
                        self.nodes[x].t = v;
                    } else {
                        return Ok(());
                    }
                }
                if let Some(nf) = self.nodes[n].f {
                    if self.nodes[x].f.is_some() {
                        self.tree_add(Slot::F(x), nf)?;
                    } else if !self.nodes[x].act_f_flg {
                        let v = self.get(Some(nf));
                        self.nodes[x].f = v;
                    } else {
                        return Ok(());
                    }
                }
                return Ok(());
            } else if !self.lt(x, n) {
                match self.nodes[x].nxt {
                    None => {
                        let v = self.get(Some(x));
                        self.nodes[n].prv = v;
                        let v = self.get(Some(n));
                        self.nodes[x].nxt = v;
                        return Ok(());
                    }
                    next => x_iter = next,
                }
            } else {
                if let Some(p) = self.nodes[x].prv {
                    let v = self.get(Some(n));
                    self.nodes[p].nxt = v;
                    self.nodes[n].prv = Some(p);
                    let v = self.get(Some(n));
                    self.nodes[x].prv = v;
                    self.nodes[n].nxt = Some(x);
                } else {
                    let v = self.get(Some(n));
                    self.nodes[x].prv = v;
                    let v = self.get(Some(x));
                    self.nodes[n].nxt = v;
                }
                if self.slot(existing) == Some(x) {
                    let v = self.get(Some(n));
                    self.set(existing, v);
                    let mut xx = Some(x);
                    self.put(&mut xx);
                }
                return Ok(());
            }
        }
        Ok(())
    }

    fn alloc(&mut self, n: Node) -> usize {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    /// `_db_rule_gen_64`: the chain for one rule, its conditions in argument
    /// order (`conds` has no argument twice).
    fn chain(&mut self, conds: &[Cond], action: u32, rule: usize) -> Fresh {
        // The shapes of `it` and `prev` follow from the operator, so these
        // are always Some; a missing one refuses the rule instead of aborting.
        macro_rules! need {
            ($o:expr) => {
                match $o {
                    Some(v) => v,
                    None => return Fresh { chains: None },
                }
            };
        }
        let mut chains = None;
        let mut prev: [Option<usize>; 3] = [None; 3];
        let mut op_prev = Op::Eq;
        let mut last: Option<[Option<usize>; 3]> = None;
        for c in conds {
            let (mask, datum) = if c.op == Op::MaskedEq { (c.value, c.datum) } else { (u64::MAX, c.value) };
            // A mask of nothing tests nothing.
            if c.op == Op::MaskedEq && mask == 0 {
                continue;
            }
            let (mh, ml, dh, dl) = ((mask >> 32) as u32, mask as u32, (datum >> 32) as u32, datum as u32);
            let mk = |high: bool, op: Cmp, m: u32, d: u32| {
                let mut n = Node::new();
                n.arg = c.index;
                n.high = high;
                n.mask = m;
                n.datum = d & m;
                n.full = datum;
                n.op = op;
                n.orig = c.op;
                n
            };
            let it: [Option<usize>; 3] = match c.op {
                Op::Gt | Op::Ge | Op::Le | Op::Lt => {
                    let lo = if matches!(c.op, Op::Gt | Op::Le) { Cmp::Gt } else { Cmp::Ge };
                    let a = self.alloc(mk(true, Cmp::Gt, mh, dh));
                    let b = self.alloc(mk(true, Cmp::Eq, mh, dh));
                    let l = self.alloc(mk(false, lo, ml, dl));
                    let v = self.get(Some(b));
                    self.nodes[a].f = v;
                    let v = self.get(Some(l));
                    self.nodes[b].t = v;
                    [Some(a), Some(b), Some(l)]
                }
                Op::Eq | Op::MaskedEq | Op::Ne => {
                    let op = if c.op == Op::MaskedEq { Cmp::MaskedEq } else { Cmp::Eq };
                    let a = self.alloc(mk(true, op, mh, dh));
                    let b = self.alloc(mk(false, op, ml, dl));
                    let v = self.get(Some(b));
                    self.nodes[a].t = v;
                    [Some(a), Some(b), None]
                }
            };
            if let Some(p0) = prev[0] {
                let head = it[0];
                match op_prev {
                    Op::Gt | Op::Ge => {
                        let v = self.get(head);
                        self.nodes[p0].t = v;
                        let v = self.get(head);
                        self.nodes[need!(prev[2])].t = v;
                    }
                    Op::Eq | Op::MaskedEq => {
                        let v = self.get(head);
                        self.nodes[need!(prev[1])].t = v;
                    }
                    Op::Le | Op::Lt => {
                        let v = self.get(head);
                        self.nodes[need!(prev[1])].f = v;
                        let v = self.get(head);
                        self.nodes[need!(prev[2])].f = v;
                    }
                    Op::Ne => {
                        let v = self.get(head);
                        self.nodes[p0].f = v;
                        let v = self.get(head);
                        self.nodes[need!(prev[1])].f = v;
                    }
                }
            } else {
                chains = self.get(it[0]);
            }
            prev = it;
            op_prev = c.op;
            last = Some(it);
        }
        let Some(it) = last else { return Fresh { chains: None } };
        let (a, b, l) = (need!(it[0]), need!(it[1]), it[2]);
        match op_prev {
            Op::Gt | Op::Ge => {
                for (i, leaf) in [(a, Leaf::Hi), (need!(l), Leaf::Lo)] {
                    self.nodes[i].act_t_flg = true;
                    self.nodes[i].act_t = action;
                    self.nodes[i].who_t = (rule, leaf);
                }
            }
            Op::Le | Op::Lt => {
                for (i, leaf) in [(b, Leaf::Hi), (need!(l), Leaf::Lo)] {
                    self.nodes[i].act_f_flg = true;
                    self.nodes[i].act_f = action;
                    self.nodes[i].who_f = (rule, leaf);
                }
            }
            Op::Eq | Op::MaskedEq => {
                self.nodes[b].act_t_flg = true;
                self.nodes[b].act_t = action;
                self.nodes[b].who_t = (rule, Leaf::Whole);
            }
            Op::Ne => {
                for (i, leaf) in [(a, Leaf::Hi), (b, Leaf::Lo)] {
                    self.nodes[i].act_f_flg = true;
                    self.nodes[i].act_f = action;
                    self.nodes[i].who_f = (rule, leaf);
                }
            }
        }
        Fresh { chains }
    }
}

impl Db {
    /// A tree whose `order` draws on a profile's shared pool of steps.
    pub fn with_pool(pool: u64) -> Db {
        let db = Db::default();
        db.pool.set(pool);
        db
    }

    /// The steps left in the pool.
    pub fn pool(&self) -> u64 {
        self.pool.get()
    }

    /// `db_rule_add` for one rule of this call; `conds` are in argument order
    /// and name each argument once (the caller splits a rule that does, as
    /// runc does). Err is EEXIST, or a tree libseccomp would never finish.
    pub fn add(&mut self, rule: usize, action: u32, conds: &[Cond]) -> Result<(), ()> {
        let mut new = self.chain(conds, action, rule);
        let Some(first) = new.chains else { return Err(()) };
        if !self.started {
            self.started = true;
            self.chains = new.chains;
            return Ok(());
        }
        let mut rm_flag = false;
        self.fuel.set(FUEL);
        let result = loop {
            if !self.tick() {
                break Err(());
            }
            if self.chains.is_none() {
                // The old tree was pruned away: the new rule stands alone.
                debug_assert!(rm_flag);
                self.chains = new.chains;
                break Ok(());
            }
            let mut state = State { flags: 0, action };
            let rc = self.prune(self.chains, new.chains, &mut state);
            if rc > 0 {
                rm_flag = true;
                if self.chains.is_none() {
                    continue;
                }
            } else if state.flags & REDUNDANT == REDUNDANT {
                self.tree_put(&mut new.chains);
                break Ok(());
            }
            let r = self.tree_add(Slot::Chains, first);
            self.tree_put(&mut new.chains);
            break r;
        };
        if self.spent.get() {
            return Err(());
        }
        result
    }

    fn level_start(&self, mut n: usize) -> usize {
        while let Some(p) = self.nodes[n].prv.filter(|_| self.tick()) {
            n = p;
        }
        n
    }

    /// Where the rules' actions stand, in the order the filter reaches them,
    /// each once.
    pub fn order(&self) -> Vec<Who> {
        let start = self.pool.get().min(FUEL);
        self.fuel.set(start);
        let out = self.order_inner();
        self.pool.set(self.pool.get() - (start - self.fuel.get()));
        out
    }

    fn order_inner(&self) -> Vec<Who> {
        let mut out = Vec::new();
        let mut seen = vec![false; self.nodes.len()];
        if let Some(root) = self.chains {
            self.level(self.level_start(root), &mut seen, &mut out);
        }
        let mut done = std::collections::HashSet::new();
        out.retain(|w| done.insert(*w));
        out
    }

    fn level(&self, first: usize, seen: &mut [bool], out: &mut Vec<Who>) {
        let mut cur = Some(first);
        while let Some(i) = cur.filter(|_| self.tick()) {
            if std::mem::replace(&mut seen[i], true) {
                // Reached through another parent already: its actions are
                // listed there.
                cur = self.nodes[i].nxt;
                continue;
            }
            let n = &self.nodes[i];
            match n.t {
                Some(c) => self.level(self.level_start(c), seen, out),
                None if n.act_t_flg => out.push(n.who_t),
                None => {}
            }
            match n.f {
                Some(c) => self.level(self.level_start(c), seen, out),
                None if n.act_f_flg => out.push(n.who_f),
                None => {}
            }
            cur = n.nxt;
        }
    }

    /// The first level of the tree.
    pub fn first_level(&self) -> Option<usize> {
        self.chains.map(|r| self.level_start(r))
    }

    /// The next node on a node's level.
    pub fn sibling(&self, i: usize) -> Option<usize> {
        self.nodes[i].nxt
    }

    /// The test a node makes.
    pub fn test(&self, i: usize) -> Test {
        let n = &self.nodes[i];
        Test {
            off: OFF_ARGS + 8 * n.arg + if n.high { 4 } else { 0 },
            mask: n.mask,
            code: match n.op {
                Cmp::Eq | Cmp::MaskedEq => JEQ_K,
                Cmp::Gt => JGT_K,
                Cmp::Ge => JGE_K,
            },
            datum: n.datum,
        }
    }

    /// Where a node goes when its test holds (`t`) or does not.
    pub fn branch(&self, i: usize, t: bool) -> Branch {
        let n = &self.nodes[i];
        let (next, flg, act, who) = if t { (n.t, n.act_t_flg, n.act_t, n.who_t) } else { (n.f, n.act_f_flg, n.act_f, n.who_f) };
        match next {
            Some(c) => Branch::Level(self.level_start(c)),
            None if flg => Branch::Act(act, who),
            None => Branch::Nothing,
        }
    }
}

/// A node's test: load the half-argument at `off`, mask it unless the mask is
/// all ones, compare with `datum`.
pub struct Test {
    pub off: u32,
    pub mask: u32,
    pub code: u16,
    pub datum: u32,
}

/// Where one side of a node's test goes.
pub enum Branch {
    /// On to the next node of the level.
    Nothing,
    Act(u32, Who),
    /// The level below, by its first node.
    Level(usize),
}
